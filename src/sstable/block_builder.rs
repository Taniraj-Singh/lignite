//! Builds a single delta-encoded data (or index) block.
//!
//! Within a block, consecutive keys usually share a long common prefix, so each
//! entry stores only the bytes by which its key differs from its predecessor:
//!
//! ```text
//! entry := varint(shared) varint(non_shared) varint(value_len)
//!          key[shared .. shared+non_shared]  value[0 .. value_len]
//! ```
//!
//! Every `restart_interval` entries the prefix is reset (a *restart point*, whose
//! key is stored in full). The block trailer lists the byte offset of each
//! restart point followed by their count, letting a reader binary-search restarts
//! before scanning linearly.

use crate::util::coding::{encode_fixed32, put_varint32};

/// Accumulates key/value pairs into one block's byte buffer.
pub struct BlockBuilder {
    buffer: Vec<u8>,
    restarts: Vec<u32>,
    counter: usize,
    restart_interval: usize,
    last_key: Vec<u8>,
    finished: bool,
}

impl BlockBuilder {
    /// Create a builder that inserts a restart point every `restart_interval`
    /// entries (must be `>= 1`).
    pub fn new(restart_interval: usize) -> Self {
        let restart_interval = restart_interval.max(1);
        BlockBuilder {
            buffer: Vec::new(),
            restarts: vec![0], // first restart is always at offset 0
            counter: 0,
            restart_interval,
            last_key: Vec::new(),
            finished: false,
        }
    }

    /// Discard all state, ready to build a fresh block.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.restarts.clear();
        self.restarts.push(0);
        self.counter = 0;
        self.last_key.clear();
        self.finished = false;
    }

    /// True if nothing has been added yet.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Estimated size of the block once [`finish`](Self::finish) is called.
    pub fn current_size_estimate(&self) -> usize {
        self.buffer.len() + self.restarts.len() * 4 + 4
    }

    /// Append `key`/`value`.
    ///
    /// Keys must arrive in the order imposed by the caller's key comparator. That
    /// order is *not* necessarily raw-byte order: internal keys sort a shared
    /// user key by descending sequence trailer, so the builder makes no byte-wise
    /// ordering assumption and only relies on the shared-prefix optimisation,
    /// which is valid for any key sequence.
    pub fn add(&mut self, key: &[u8], value: &[u8]) {
        debug_assert!(!self.finished);
        debug_assert!(self.counter <= self.restart_interval);

        let shared = if self.counter < self.restart_interval {
            common_prefix_len(&self.last_key, key)
        } else {
            // Begin a new restart run with a fully-stored key.
            self.restarts.push(self.buffer.len() as u32);
            self.counter = 0;
            0
        };
        let non_shared = key.len() - shared;

        put_varint32(&mut self.buffer, shared as u32);
        put_varint32(&mut self.buffer, non_shared as u32);
        put_varint32(&mut self.buffer, value.len() as u32);
        self.buffer.extend_from_slice(&key[shared..]);
        self.buffer.extend_from_slice(value);

        self.last_key.truncate(shared);
        self.last_key.extend_from_slice(&key[shared..]);
        debug_assert_eq!(self.last_key.as_slice(), key);

        self.counter += 1;
    }

    /// Append the restart array and count, returning the finished block bytes.
    pub fn finish(&mut self) -> &[u8] {
        for &r in &self.restarts {
            encode_fixed32(&mut self.buffer, r);
        }
        encode_fixed32(&mut self.buffer, self.restarts.len() as u32);
        self.finished = true;
        &self.buffer
    }
}

/// Length of the shared prefix of `a` and `b`.
fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::coding::decode_fixed32;

    #[test]
    fn prefix_len_basic() {
        assert_eq!(common_prefix_len(b"abcdef", b"abcxyz"), 3);
        assert_eq!(common_prefix_len(b"", b"x"), 0);
        assert_eq!(common_prefix_len(b"same", b"same"), 4);
    }

    #[test]
    fn finish_records_restart_count() {
        let mut b = BlockBuilder::new(2);
        b.add(b"aaa", b"1");
        b.add(b"aab", b"2");
        b.add(b"aac", b"3"); // triggers a new restart (interval = 2)
        let block = b.finish().to_vec();
        let num_restarts = decode_fixed32(&block[block.len() - 4..]).unwrap();
        assert_eq!(num_restarts, 2);
    }

    #[test]
    fn empty_before_add() {
        let b = BlockBuilder::new(16);
        assert!(b.is_empty());
    }
}
