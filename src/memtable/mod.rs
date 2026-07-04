//! The in-memory write buffer.
//!
//! A [`MemTable`] is the first landing place for a write once it has been logged.
//! It is an ordered map from internal key to value, backed by a [`skiplist`].
//! Records are never overwritten in place: a `put` and a later `delete` of the
//! same user key are two distinct internal keys (different sequence numbers), and
//! the skiplist's ordering guarantees the newest sits first among them.
//!
//! When a memtable's estimated footprint crosses the configured write-buffer
//! size the database freezes it (no further inserts) and schedules it for flush
//! into an SSTable. Frozen memtables are shared immutably through an `Arc`, which
//! is what lets a reader iterate one without blocking writers.

pub mod skiplist;

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::iterator::InternalIterator;
use crate::key::{
    append_internal_key, InternalKeyComparator, LookupKey, ParsedInternalKey, SequenceNumber,
    ValueType,
};
use crate::util::coding::{put_length_prefixed_slice, take_length_prefixed_slice};
use skiplist::{SkipList, SkipListIterator};

/// An ordered, in-memory collection of internal key/value records.
pub struct MemTable {
    table: SkipList,
    icmp: InternalKeyComparator,
    memory_usage: usize,
}

impl MemTable {
    /// Create an empty memtable ordered by `icmp`.
    pub fn new(icmp: InternalKeyComparator) -> Self {
        let table = SkipList::new(icmp.clone());
        MemTable {
            table,
            icmp,
            memory_usage: 0,
        }
    }

    /// The internal-key comparator this memtable is ordered by.
    pub fn comparator(&self) -> &InternalKeyComparator {
        &self.icmp
    }

    /// A rough count of the heap bytes this memtable occupies. The database
    /// compares this against `write_buffer_size` to decide when to flush.
    pub fn approximate_memory_usage(&self) -> usize {
        self.memory_usage
    }

    /// Whether the memtable holds no records.
    pub fn is_empty(&self) -> bool {
        self.table.len() == 0
    }

    /// Number of records (including tombstones and superseded versions).
    pub fn len(&self) -> usize {
        self.table.len()
    }

    /// Insert a record.
    ///
    /// The stored entry is encoded as
    /// `varint(internal_key_len) | internal_key | varint(value_len) | value`,
    /// where `internal_key = user_key || trailer`. Encoding the length prefixes
    /// lets the same skiplist entry serve both key comparison and value
    /// retrieval.
    pub fn add(&mut self, seq: SequenceNumber, t: ValueType, key: &[u8], value: &[u8]) {
        let internal_key_size = key.len() + 8;
        let mut entry = Vec::with_capacity(
            crate::util::coding::varint_length(internal_key_size as u64)
                + internal_key_size
                + crate::util::coding::varint_length(value.len() as u64)
                + value.len(),
        );
        crate::util::coding::put_varint32(&mut entry, internal_key_size as u32);
        append_internal_key(&mut entry, key, seq, t);
        put_length_prefixed_slice(&mut entry, value);
        self.memory_usage += entry.len() + std::mem::size_of::<usize>() * 4;
        self.table.insert(entry);
    }

    /// Look up `key.user_key()` as of `key`'s snapshot sequence.
    ///
    /// Returns:
    ///   * `Some(Ok(value))` if the newest visible record is a value;
    ///   * `Some(Err(Error::NotFound))` if it is a tombstone;
    ///   * `None` if no record for the user key is visible here (the caller
    ///     should consult older sources).
    pub fn get(&self, key: &LookupKey) -> Option<Result<Vec<u8>>> {
        let node = self.table.seek_ge(key.memtable_key());
        if node == 0 {
            return None;
        }
        // The skiplist stored the whole entry; decode its key and value.
        let entry = self.table.entry_at(node);
        let (internal_key, rest) = take_length_prefixed_slice(entry)?;
        let parsed = ParsedInternalKey::decode(internal_key)?;
        // Confirm the located record shares the requested user key.
        if self
            .icmp
            .user_comparator()
            .compare(parsed.user_key, key.user_key())
            != std::cmp::Ordering::Equal
        {
            return None;
        }
        match parsed.value_type {
            ValueType::Value => {
                let (value, _) = take_length_prefixed_slice(rest)?;
                Some(Ok(value.to_vec()))
            }
            ValueType::Deletion => Some(Err(Error::NotFound)),
        }
    }

    /// Build an iterator over the memtable's internal records.
    pub fn iter(self: &Arc<Self>) -> MemTableIter {
        MemTableIter {
            owner: Arc::clone(self),
            inner: self.table.iter(),
            cur_key: Vec::new(),
            cur_value: Vec::new(),
        }
    }
}

/// An [`InternalIterator`] over a (typically frozen) memtable.
///
/// Holds an `Arc` to its memtable so it can outlive the caller's borrow; the
/// skiplist it walks is only mutated while the memtable is active, and iterators
/// are built over frozen memtables.
pub struct MemTableIter {
    owner: Arc<MemTable>,
    inner: SkipListIterator,
    cur_key: Vec<u8>,
    cur_value: Vec<u8>,
}

impl MemTableIter {
    fn refresh(&mut self) {
        self.cur_key.clear();
        self.cur_value.clear();
        if !self.inner.valid() {
            return;
        }
        let entry = self.owner.table.entry_at(self.inner.node());
        if let Some((internal_key, rest)) = take_length_prefixed_slice(entry) {
            self.cur_key.extend_from_slice(internal_key);
            if let Some((value, _)) = take_length_prefixed_slice(rest) {
                self.cur_value.extend_from_slice(value);
            }
        }
    }

    /// Encode a bare internal key into the length-prefixed form the skiplist
    /// compares against, so a seek target from the merge layer lands correctly.
    fn seek_target(internal_key: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(internal_key.len() + 5);
        crate::util::coding::put_varint32(&mut buf, internal_key.len() as u32);
        buf.extend_from_slice(internal_key);
        buf
    }
}

impl InternalIterator for MemTableIter {
    fn valid(&self) -> bool {
        self.inner.valid()
    }

    fn seek_to_first(&mut self) {
        self.inner.seek_to_first(&self.owner.table);
        self.refresh();
    }

    fn seek_to_last(&mut self) {
        self.inner.seek_to_last(&self.owner.table);
        self.refresh();
    }

    fn seek(&mut self, target: &[u8]) {
        let encoded = Self::seek_target(target);
        self.inner.seek(&self.owner.table, &encoded);
        self.refresh();
    }

    fn next(&mut self) {
        self.inner.next(&self.owner.table);
        self.refresh();
    }

    fn prev(&mut self) {
        self.inner.prev(&self.owner.table);
        self.refresh();
    }

    fn key(&self) -> &[u8] {
        &self.cur_key
    }

    fn value(&self) -> &[u8] {
        &self.cur_value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;

    fn new_memtable() -> Arc<MemTable> {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        Arc::new(MemTable::new(icmp))
    }

    #[test]
    fn put_then_get() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        mt.add(1, ValueType::Value, b"foo", b"bar");
        let lk = LookupKey::new(b"foo", 100);
        assert_eq!(mt.get(&lk).unwrap().unwrap(), b"bar");
    }

    #[test]
    fn newest_version_wins() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        mt.add(1, ValueType::Value, b"k", b"old");
        mt.add(2, ValueType::Value, b"k", b"new");
        let lk = LookupKey::new(b"k", 100);
        assert_eq!(mt.get(&lk).unwrap().unwrap(), b"new");
    }

    #[test]
    fn snapshot_hides_newer_writes() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        mt.add(1, ValueType::Value, b"k", b"v1");
        mt.add(5, ValueType::Value, b"k", b"v5");
        // A read at snapshot 3 must not see the write at sequence 5.
        let lk = LookupKey::new(b"k", 3);
        assert_eq!(mt.get(&lk).unwrap().unwrap(), b"v1");
    }

    #[test]
    fn tombstone_reports_not_found() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        mt.add(1, ValueType::Value, b"k", b"v");
        mt.add(2, ValueType::Deletion, b"k", b"");
        let lk = LookupKey::new(b"k", 100);
        assert!(mt.get(&lk).unwrap().unwrap_err().is_not_found());
    }

    #[test]
    fn missing_key_is_none() {
        let mt = new_memtable();
        let lk = LookupKey::new(b"absent", 100);
        assert!(mt.get(&lk).is_none());
    }

    #[test]
    fn iterator_yields_sorted_internal_keys() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        mt.add(1, ValueType::Value, b"c", b"3");
        mt.add(1, ValueType::Value, b"a", b"1");
        mt.add(1, ValueType::Value, b"b", b"2");
        let mt = Arc::new(mt);
        let mut it = mt.iter();
        it.seek_to_first();
        let mut seen = Vec::new();
        while it.valid() {
            seen.push(crate::key::extract_user_key(it.key()).to_vec());
            it.next();
        }
        assert_eq!(seen, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);
    }
}
