//! The per-table filter (meta) block.
//!
//! One bloom filter is built for every `2^base_lg` bytes of data-block output, so
//! a lookup can map a data block's file offset to the filter covering it and, in
//! the common miss case, skip reading the block at all. The block layout is:
//!
//! ```text
//! [ filter 0 ][ filter 1 ] ... [ filter N-1 ]
//! [ offset_0 (u32) ] ... [ offset_{N-1} (u32) ]
//! [ offset_of_offset_array (u32) ]
//! [ base_lg (u8) ]
//! ```

use std::sync::Arc;

use crate::filter::FilterPolicy;
use crate::util::coding::{decode_fixed32, encode_fixed32};

/// Filters cover `2^FILTER_BASE_LG`-byte spans of data-block output.
const FILTER_BASE_LG: u8 = 11;
const FILTER_BASE: u64 = 1 << FILTER_BASE_LG;

/// Accumulates keys and emits the filter block.
pub struct FilterBlockBuilder {
    policy: Arc<dyn FilterPolicy>,
    keys: Vec<u8>,
    start: Vec<usize>,
    result: Vec<u8>,
    filter_offsets: Vec<u32>,
}

impl FilterBlockBuilder {
    /// Create a builder using `policy` to synthesise each filter.
    pub fn new(policy: Arc<dyn FilterPolicy>) -> Self {
        FilterBlockBuilder {
            policy,
            keys: Vec::new(),
            start: Vec::new(),
            result: Vec::new(),
            filter_offsets: Vec::new(),
        }
    }

    /// Signal that a data block ending near `block_offset` is starting; emits as
    /// many filters as needed to keep pace with the data offset.
    pub fn start_block(&mut self, block_offset: u64) {
        let filter_index = block_offset / FILTER_BASE;
        while (self.filter_offsets.len() as u64) < filter_index {
            self.generate_filter();
        }
    }

    /// Record a key to be summarised by the current filter.
    pub fn add_key(&mut self, key: &[u8]) {
        self.start.push(self.keys.len());
        self.keys.extend_from_slice(key);
    }

    fn generate_filter(&mut self) {
        let num_keys = self.start.len();
        if num_keys == 0 {
            // No keys since the last filter: record an empty span.
            self.filter_offsets.push(self.result.len() as u32);
            return;
        }
        // Sentinel to bound the last key's slice.
        self.start.push(self.keys.len());
        let mut key_slices: Vec<&[u8]> = Vec::with_capacity(num_keys);
        for i in 0..num_keys {
            key_slices.push(&self.keys[self.start[i]..self.start[i + 1]]);
        }
        self.filter_offsets.push(self.result.len() as u32);
        self.policy.create_filter(&key_slices, &mut self.result);
        self.keys.clear();
        self.start.clear();
    }

    /// Finish building and return the encoded filter block.
    pub fn finish(&mut self) -> Vec<u8> {
        if !self.start.is_empty() {
            self.generate_filter();
        }
        let array_offset = self.result.len() as u32;
        for &off in &self.filter_offsets {
            encode_fixed32(&mut self.result, off);
        }
        encode_fixed32(&mut self.result, array_offset);
        self.result.push(FILTER_BASE_LG);
        std::mem::take(&mut self.result)
    }
}

/// Queries a finished filter block.
pub struct FilterBlockReader {
    policy: Arc<dyn FilterPolicy>,
    contents: Arc<Vec<u8>>,
    array_offset: usize,
    num_filters: usize,
    base_lg: u8,
}

impl FilterBlockReader {
    /// Parse `contents`. A malformed block yields a reader that matches
    /// everything (never filters a key out), which is always safe.
    pub fn new(policy: Arc<dyn FilterPolicy>, contents: Arc<Vec<u8>>) -> Self {
        let n = contents.len();
        if n < 5 {
            return FilterBlockReader {
                policy,
                contents,
                array_offset: 0,
                num_filters: 0,
                base_lg: FILTER_BASE_LG,
            };
        }
        let base_lg = contents[n - 1];
        let array_offset = decode_fixed32(&contents[n - 5..]).unwrap_or(0) as usize;
        let num_filters = if array_offset <= n - 5 {
            (n - 5 - array_offset) / 4
        } else {
            0
        };
        FilterBlockReader {
            policy,
            contents,
            array_offset,
            num_filters,
            base_lg,
        }
    }

    /// Could the data block at `block_offset` contain `key`? A `false` return is
    /// definitive; `true` may be a false positive.
    pub fn key_may_match(&self, block_offset: u64, key: &[u8]) -> bool {
        let index = (block_offset >> self.base_lg) as usize;
        if index >= self.num_filters {
            return true; // out of range: do not exclude
        }
        let base = self.array_offset;
        let start = decode_fixed32(&self.contents[base + index * 4..]).unwrap_or(0) as usize;
        let limit = if index + 1 < self.num_filters {
            decode_fixed32(&self.contents[base + (index + 1) * 4..]).unwrap_or(0) as usize
        } else {
            self.array_offset
        };
        if start > limit || limit > self.array_offset {
            return true; // corrupt entry: do not exclude
        }
        if start == limit {
            return false; // empty filter => definitely no keys
        }
        self.policy
            .key_may_match(key, &self.contents[start..limit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::BloomFilterPolicy;

    fn policy() -> Arc<dyn FilterPolicy> {
        Arc::new(BloomFilterPolicy::new(10))
    }

    #[test]
    fn single_filter_matches_inserted_keys() {
        let p = policy();
        let mut b = FilterBlockBuilder::new(Arc::clone(&p));
        b.start_block(0);
        b.add_key(b"alpha");
        b.add_key(b"beta");
        b.add_key(b"gamma");
        let bytes = Arc::new(b.finish());
        let r = FilterBlockReader::new(p, bytes);
        assert!(r.key_may_match(0, b"alpha"));
        assert!(r.key_may_match(0, b"beta"));
        assert!(r.key_may_match(0, b"gamma"));
        // A key that was never inserted is very likely excluded.
        assert!(!r.key_may_match(0, b"zzzz-not-present-9137"));
    }

    #[test]
    fn filters_track_block_offsets() {
        let p = policy();
        let mut b = FilterBlockBuilder::new(Arc::clone(&p));
        b.start_block(0);
        b.add_key(b"k-at-0");
        // Jump well past several filter spans.
        b.start_block(FILTER_BASE * 3);
        b.add_key(b"k-at-3");
        let bytes = Arc::new(b.finish());
        let r = FilterBlockReader::new(p, bytes);
        assert!(r.key_may_match(0, b"k-at-0"));
        assert!(r.key_may_match(FILTER_BASE * 3, b"k-at-3"));
    }
}
