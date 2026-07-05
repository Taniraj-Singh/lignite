//! Reads a block produced by [`BlockBuilder`](super::block_builder::BlockBuilder)
//! and exposes it as an [`InternalIterator`].
//!
//! The iterator reconstructs each key from the shared/non-shared prefix encoding
//! as it advances. Seeks first binary-search the restart array to land on the
//! restart run that may contain the target, then scan that run linearly. Reverse
//! iteration re-scans from the preceding restart because entries carry no back
//! links.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::iterator::InternalIterator;
use crate::key::InternalKeyComparator;
use crate::util::coding::{decode_fixed32, decode_varint32};
use std::cmp::Ordering;

/// An immutable, parsed view over a single block's bytes.
pub struct Block {
    data: Arc<Vec<u8>>,
    restart_offset: usize,
    num_restarts: usize,
}

impl Block {
    /// Wrap raw block contents (without the file trailer).
    pub fn new(data: Arc<Vec<u8>>) -> Result<Block> {
        if data.len() < 4 {
            return Err(Error::corruption("block smaller than restart count"));
        }
        let num_restarts = decode_fixed32(&data[data.len() - 4..])
            .ok_or_else(|| Error::corruption("block missing restart count"))? as usize;
        let max_restarts = (data.len() - 4) / 4;
        if num_restarts > max_restarts {
            return Err(Error::corruption("block restart count exceeds capacity"));
        }
        let restart_offset = data.len() - 4 - num_restarts * 4;
        Ok(Block {
            data,
            restart_offset,
            num_restarts,
        })
    }

    /// Approximate in-memory footprint.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// A cursor over this block, ordered by `icmp`.
    pub fn iter(&self, icmp: InternalKeyComparator) -> BlockIter {
        BlockIter {
            data: Arc::clone(&self.data),
            restart_offset: self.restart_offset,
            num_restarts: self.num_restarts,
            icmp,
            current: self.restart_offset,
            key: Vec::new(),
            value_offset: 0,
            value_len: 0,
            valid: false,
            error: None,
        }
    }
}

/// A bidirectional cursor over one [`Block`].
pub struct BlockIter {
    data: Arc<Vec<u8>>,
    restart_offset: usize,
    num_restarts: usize,
    icmp: InternalKeyComparator,
    current: usize,
    key: Vec<u8>,
    value_offset: usize,
    value_len: usize,
    valid: bool,
    error: Option<String>,
}

impl BlockIter {
    fn restart_point(&self, index: usize) -> usize {
        decode_fixed32(&self.data[self.restart_offset + index * 4..]).unwrap_or(0) as usize
    }

    fn next_entry_offset(&self) -> usize {
        self.value_offset + self.value_len
    }

    fn mark_corrupt(&mut self) -> bool {
        self.error = Some("bad block entry".to_string());
        self.valid = false;
        self.key.clear();
        self.current = self.restart_offset;
        false
    }

    /// Parse the entry beginning at absolute `offset`, reconstructing its key
    /// from the shared prefix currently held in `self.key`.
    fn parse_entry_at(&mut self, offset: usize) -> bool {
        if offset >= self.restart_offset {
            self.current = self.restart_offset;
            self.valid = false;
            self.key.clear();
            return false;
        }
        let base = &self.data[offset..self.restart_offset];
        let (shared, r1) = match decode_varint32(base) {
            Some(x) => x,
            None => return self.mark_corrupt(),
        };
        let (non_shared, r2) = match decode_varint32(r1) {
            Some(x) => x,
            None => return self.mark_corrupt(),
        };
        let (value_len, r3) = match decode_varint32(r2) {
            Some(x) => x,
            None => return self.mark_corrupt(),
        };
        let (shared, non_shared, value_len) = (shared as usize, non_shared as usize, value_len as usize);
        let header_len = base.len() - r3.len();
        if r3.len() < non_shared + value_len || shared > self.key.len() {
            return self.mark_corrupt();
        }
        self.key.truncate(shared);
        let delta_start = offset + header_len;
        self.key
            .extend_from_slice(&self.data[delta_start..delta_start + non_shared]);
        self.value_offset = delta_start + non_shared;
        self.value_len = value_len;
        self.current = offset;
        self.valid = true;
        true
    }

    /// Position at restart run `index` and parse its first entry.
    fn seek_to_restart(&mut self, index: usize) {
        self.key.clear();
        let offset = self.restart_point(index);
        self.parse_entry_at(offset);
    }

    /// The full key stored at restart run `index` (its entries have `shared==0`).
    ///
    /// Every offset here derives from a restart-array entry, which is
    /// attacker-controlled in an untrusted block, so each bound is validated: a
    /// restart offset past the entry region, or a non-shared key length that runs
    /// past it, yields `None` rather than an out-of-bounds slice.
    fn restart_key(&self, index: usize) -> Option<&[u8]> {
        let offset = self.restart_point(index);
        if offset >= self.restart_offset {
            return None;
        }
        let base = &self.data[offset..self.restart_offset];
        let (_shared, r1) = decode_varint32(base)?;
        let (non_shared, r2) = decode_varint32(r1)?;
        let (_value_len, r3) = decode_varint32(r2)?;
        let header_len = base.len() - r3.len();
        let start = offset + header_len;
        let end = start.checked_add(non_shared as usize)?;
        if end > self.restart_offset {
            return None;
        }
        Some(&self.data[start..end])
    }
}

impl InternalIterator for BlockIter {
    fn valid(&self) -> bool {
        self.valid
    }

    fn seek_to_first(&mut self) {
        if self.num_restarts == 0 {
            self.valid = false;
            return;
        }
        self.seek_to_restart(0);
    }

    fn seek_to_last(&mut self) {
        if self.num_restarts == 0 {
            self.valid = false;
            return;
        }
        self.seek_to_restart(self.num_restarts - 1);
        while self.valid && self.next_entry_offset() < self.restart_offset {
            let off = self.next_entry_offset();
            self.parse_entry_at(off);
        }
    }

    fn seek(&mut self, target: &[u8]) {
        // Binary search for the last restart whose key sorts before `target`.
        let mut left = 0usize;
        let mut right = self.num_restarts.saturating_sub(1);
        while left < right {
            let mid = left + (right - left + 1) / 2;
            let ordering = match self.restart_key(mid) {
                Some(k) => self.icmp.compare(k, target),
                None => Ordering::Greater,
            };
            if ordering == Ordering::Less {
                left = mid;
            } else {
                right = mid - 1;
            }
        }
        if self.num_restarts == 0 {
            self.valid = false;
            return;
        }
        self.seek_to_restart(left);
        while self.valid && self.icmp.compare(&self.key, target) == Ordering::Less {
            let off = self.next_entry_offset();
            self.parse_entry_at(off);
        }
    }

    fn next(&mut self) {
        if !self.valid {
            return;
        }
        let off = self.next_entry_offset();
        self.parse_entry_at(off);
    }

    fn prev(&mut self) {
        if !self.valid {
            return;
        }
        let original = self.current;
        if original == 0 {
            // No entry precedes the first.
            self.valid = false;
            self.key.clear();
            return;
        }
        // Largest restart index whose offset is strictly before `original`.
        let mut index = self.num_restarts - 1;
        while index > 0 && self.restart_point(index) >= original {
            index -= 1;
        }
        self.seek_to_restart(index);
        while self.valid && self.next_entry_offset() < original {
            let off = self.next_entry_offset();
            self.parse_entry_at(off);
        }
    }

    fn key(&self) -> &[u8] {
        &self.key
    }

    fn value(&self) -> &[u8] {
        &self.data[self.value_offset..self.value_offset + self.value_len]
    }

    fn status(&self) -> Result<()> {
        match &self.error {
            Some(msg) => Err(Error::corruption(msg.clone())),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::{append_internal_key, extract_user_key, ValueType};
    use crate::sstable::block_builder::BlockBuilder;

    fn icmp() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    fn ikey(user: &[u8], seq: u64) -> Vec<u8> {
        let mut k = Vec::new();
        append_internal_key(&mut k, user, seq, ValueType::Value);
        k
    }

    fn build_block(pairs: &[(&[u8], u64, &[u8])], restart_interval: usize) -> Block {
        let mut bb = BlockBuilder::new(restart_interval);
        for (u, seq, v) in pairs {
            bb.add(&ikey(u, *seq), v);
        }
        let bytes = bb.finish().to_vec();
        Block::new(Arc::new(bytes)).unwrap()
    }

    #[test]
    fn forward_scan_reconstructs_keys() {
        let pairs: Vec<(&[u8], u64, &[u8])> = vec![
            (b"apple", 3, b"a"),
            (b"applesauce", 3, b"b"),
            (b"banana", 3, b"c"),
            (b"band", 3, b"d"),
            (b"bandana", 3, b"e"),
        ];
        let block = build_block(&pairs, 2);
        let mut it = block.iter(icmp());
        it.seek_to_first();
        let mut seen = Vec::new();
        while it.valid() {
            seen.push((extract_user_key(it.key()).to_vec(), it.value().to_vec()));
            it.next();
        }
        assert_eq!(seen.len(), 5);
        assert_eq!(extract_user_key(&ikey(b"applesauce", 3)), b"applesauce");
        assert_eq!(seen[1].0, b"applesauce");
        assert_eq!(seen[4].1, b"e");
    }

    #[test]
    fn seek_lands_on_first_ge() {
        let pairs: Vec<(&[u8], u64, &[u8])> =
            vec![(b"a", 1, b"1"), (b"c", 1, b"3"), (b"e", 1, b"5"), (b"g", 1, b"7")];
        let block = build_block(&pairs, 2);
        let mut it = block.iter(icmp());
        it.seek(&ikey(b"d", crate::key::MAX_SEQUENCE_NUMBER));
        assert!(it.valid());
        assert_eq!(extract_user_key(it.key()), b"e");
    }

    #[test]
    fn seek_past_end_is_invalid() {
        let pairs: Vec<(&[u8], u64, &[u8])> = vec![(b"a", 1, b"1"), (b"b", 1, b"2")];
        let block = build_block(&pairs, 4);
        let mut it = block.iter(icmp());
        it.seek(&ikey(b"z", crate::key::MAX_SEQUENCE_NUMBER));
        assert!(!it.valid());
    }

    #[test]
    fn reverse_scan() {
        let pairs: Vec<(&[u8], u64, &[u8])> =
            vec![(b"a", 1, b"1"), (b"b", 1, b"2"), (b"c", 1, b"3"), (b"d", 1, b"4")];
        let block = build_block(&pairs, 2);
        let mut it = block.iter(icmp());
        it.seek_to_last();
        let mut seen = Vec::new();
        while it.valid() {
            seen.push(extract_user_key(it.key()).to_vec());
            it.prev();
        }
        assert_eq!(seen, vec![b"d".to_vec(), b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]);
    }

    #[test]
    fn seek_to_exact_key() {
        let pairs: Vec<(&[u8], u64, &[u8])> =
            vec![(b"aa", 1, b"1"), (b"ab", 1, b"2"), (b"ac", 1, b"3")];
        let block = build_block(&pairs, 1);
        let mut it = block.iter(icmp());
        it.seek(&ikey(b"ab", crate::key::MAX_SEQUENCE_NUMBER));
        assert!(it.valid());
        assert_eq!(extract_user_key(it.key()), b"ab");
        assert_eq!(it.value(), b"2");
    }
}
