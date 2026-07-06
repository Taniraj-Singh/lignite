//! Write batches: the atomic, log-serialisable unit of mutation.
//!
//! A batch groups any number of puts and deletes so they become durable together
//! and are assigned a contiguous run of sequence numbers. Its byte layout doubles
//! as the write-ahead-log payload:
//!
//! ```text
//! header  := sequence(fixed64) count(fixed32)
//! record* := (kTypeValue key value) | (kTypeDeletion key)
//! ```
//!
//! where each key/value is a varint-length-prefixed slice. The first operation in
//! the batch takes the batch's base sequence number; the `i`-th takes
//! `sequence + i`.

use crate::error::{Error, Result};
use crate::key::{SequenceNumber, ValueType};
use crate::memtable::MemTable;
use crate::util::coding::{
    decode_fixed32, decode_fixed64, put_length_prefixed_slice, take_length_prefixed_slice,
};

const HEADER_SIZE: usize = 12;
const TAG_VALUE: u8 = 0x1;
const TAG_DELETION: u8 = 0x0;

/// An ordered set of puts and deletes applied atomically.
#[derive(Clone)]
pub struct WriteBatch {
    rep: Vec<u8>,
}

impl WriteBatch {
    /// Create an empty batch.
    pub fn new() -> Self {
        WriteBatch {
            rep: vec![0u8; HEADER_SIZE],
        }
    }

    /// Reset to empty.
    pub fn clear(&mut self) {
        self.rep.clear();
        self.rep.resize(HEADER_SIZE, 0);
    }

    /// Queue a put of `key` -> `value`.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.set_count(self.count() + 1);
        self.rep.push(TAG_VALUE);
        put_length_prefixed_slice(&mut self.rep, key);
        put_length_prefixed_slice(&mut self.rep, value);
    }

    /// Queue a deletion of `key`.
    pub fn delete(&mut self, key: &[u8]) {
        self.set_count(self.count() + 1);
        self.rep.push(TAG_DELETION);
        put_length_prefixed_slice(&mut self.rep, key);
    }

    /// Number of operations queued.
    pub fn count(&self) -> u32 {
        decode_fixed32(&self.rep[8..12]).unwrap_or(0)
    }

    /// Whether the batch has no operations.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    fn set_count(&mut self, n: u32) {
        let bytes = n.to_le_bytes();
        self.rep[8..12].copy_from_slice(&bytes);
    }

    /// The base sequence number assigned to this batch.
    pub fn sequence(&self) -> SequenceNumber {
        decode_fixed64(&self.rep[0..8]).unwrap_or(0)
    }

    /// Stamp the base sequence number.
    pub fn set_sequence(&mut self, seq: SequenceNumber) {
        let bytes = seq.to_le_bytes();
        self.rep[0..8].copy_from_slice(&bytes);
    }

    /// The serialised representation (also the WAL payload).
    pub fn contents(&self) -> &[u8] {
        &self.rep
    }

    /// Rebuild a batch from a previously serialised representation.
    pub fn from_contents(rep: Vec<u8>) -> Result<Self> {
        if rep.len() < HEADER_SIZE {
            return Err(Error::corruption("write batch shorter than header"));
        }
        Ok(WriteBatch { rep })
    }

    /// Estimated serialised size in bytes.
    pub fn approximate_size(&self) -> usize {
        self.rep.len()
    }

    /// Append every operation in `other` to this batch.
    pub fn append(&mut self, other: &WriteBatch) {
        let extra = other.count();
        self.set_count(self.count() + extra);
        self.rep.extend_from_slice(&other.rep[HEADER_SIZE..]);
    }

    /// Apply this batch to `memtable`, assigning sequence numbers starting at the
    /// batch's base sequence.
    pub fn insert_into(&self, memtable: &mut MemTable) -> Result<()> {
        let mut seq = self.sequence();
        let mut rest = &self.rep[HEADER_SIZE..];
        let mut applied = 0u32;
        while !rest.is_empty() {
            let tag = rest[0];
            rest = &rest[1..];
            match tag {
                TAG_VALUE => {
                    let (key, r1) = take_length_prefixed_slice(rest)
                        .ok_or_else(|| Error::corruption("bad batch put key"))?;
                    let (value, r2) = take_length_prefixed_slice(r1)
                        .ok_or_else(|| Error::corruption("bad batch put value"))?;
                    memtable.add(seq, ValueType::Value, key, value);
                    rest = r2;
                }
                TAG_DELETION => {
                    let (key, r1) = take_length_prefixed_slice(rest)
                        .ok_or_else(|| Error::corruption("bad batch delete key"))?;
                    memtable.add(seq, ValueType::Deletion, key, b"");
                    rest = r1;
                }
                other => {
                    return Err(Error::corruption(format!("unknown batch tag {other}")));
                }
            }
            seq = seq
                .checked_add(1)
                .ok_or_else(|| Error::corruption("batch sequence overflow"))?;
            applied += 1;
        }
        if applied != self.count() {
            return Err(Error::corruption("batch count mismatch"));
        }
        Ok(())
    }
}

impl Default for WriteBatch {
    fn default() -> Self {
        WriteBatch::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::{InternalKeyComparator, LookupKey};
    use std::sync::Arc;

    #[test]
    fn count_tracks_operations() {
        let mut b = WriteBatch::new();
        assert_eq!(b.count(), 0);
        b.put(b"a", b"1");
        b.put(b"b", b"2");
        b.delete(b"a");
        assert_eq!(b.count(), 3);
    }

    #[test]
    fn insert_assigns_sequential_sequences() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        let mut b = WriteBatch::new();
        b.set_sequence(100);
        b.put(b"k", b"first");
        b.put(b"k", b"second");
        b.insert_into(&mut mt).unwrap();
        // The second put (sequence 101) must win.
        let lk = LookupKey::new(b"k", 1000);
        assert_eq!(mt.get(&lk).unwrap().unwrap(), b"second");
        // A read at sequence 100 sees only the first.
        let lk100 = LookupKey::new(b"k", 100);
        assert_eq!(mt.get(&lk100).unwrap().unwrap(), b"first");
    }

    #[test]
    fn append_merges_batches() {
        let mut a = WriteBatch::new();
        a.put(b"x", b"1");
        let mut b = WriteBatch::new();
        b.put(b"y", b"2");
        b.delete(b"z");
        a.append(&b);
        assert_eq!(a.count(), 3);
    }

    #[test]
    fn serialise_roundtrip() {
        let mut a = WriteBatch::new();
        a.set_sequence(42);
        a.put(b"hello", b"world");
        a.delete(b"gone");
        let bytes = a.contents().to_vec();
        let b = WriteBatch::from_contents(bytes).unwrap();
        assert_eq!(b.count(), 2);
        assert_eq!(b.sequence(), 42);
    }
}
