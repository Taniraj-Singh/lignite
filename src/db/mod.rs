//! The embeddable database: the public surface that unifies the memtable, the
//! write-ahead log, the SSTable levels and compaction behind a key/value API.
//!
//! ## Write path
//!
//! [`Db::write`] stamps the batch with the next run of sequence numbers, appends
//! it to the write-ahead log, and applies it to the active memtable. When the
//! memtable exceeds the write buffer it is frozen and flushed into a level-0
//! SSTable.
//!
//! ## Read path
//!
//! A read resolves a user key at a sequence number (the latest, or a
//! [`Snapshot`]'s). It consults the active memtable, then any memtables awaiting
//! flush, and finally the SSTables. The tables are read by funnelling every
//! candidate file through a single [`MergingIterator`] and taking the newest
//! version of the key at or below the read's sequence number.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::batch::WriteBatch;
use crate::compaction::{compact_to_bottom, CompactionStats};
use crate::comparator::Comparator;
use crate::error::Result;
use crate::iterator::merging::MergingIterator;
use crate::iterator::InternalIterator;
use crate::key::{
    append_internal_key, InternalKeyComparator, LookupKey, ParsedInternalKey, SequenceNumber,
    ValueType, MAX_SEQUENCE_NUMBER, VALUE_TYPE_FOR_SEEK,
};
use crate::memtable::MemTable;
use crate::options::{Options, ReadOptions, WriteOptions};
use crate::snapshot::{Snapshot, SnapshotRegistry};
use crate::version::{build_table, VersionSet};
use crate::wal::LogWriter;

mod iter;
pub mod recovery;
pub mod stats;
pub use iter::DbIterator;
pub use recovery::{recover_memtable, Recovered};
pub use stats::{Stats, StatsSnapshot};

/// The mutable state guarded by the database lock.
struct DbCore {
    vset: VersionSet,
    mem: MemTable,
    imm: VecDeque<Arc<MemTable>>,
    log: LogWriter,
    icmp: InternalKeyComparator,
}

/// An embeddable key/value store.
///
/// A `Db` is cheap to clone-share via [`Arc`]; all handles funnel through the
/// same internal lock. This build keeps every structure in memory.
pub struct Db {
    core: Arc<Mutex<DbCore>>,
    snapshots: Arc<SnapshotRegistry>,
    user_cmp: Arc<dyn Comparator>,
    options: Options,
    stats: Arc<Stats>,
}

impl Db {
    /// Open (here, create) a database. `path` names the logical database; this
    /// build does not persist to that path.
    pub fn open(_path: impl AsRef<str>, options: Options) -> Result<Db> {
        let icmp = InternalKeyComparator::new(Arc::clone(&options.comparator));
        let user_cmp = Arc::clone(&options.comparator);
        let vset = VersionSet::new(options.clone());
        let core = DbCore {
            vset,
            mem: MemTable::new(icmp.clone()),
            imm: VecDeque::new(),
            log: LogWriter::new(),
            icmp,
        };
        Ok(Db {
            core: Arc::new(Mutex::new(core)),
            snapshots: SnapshotRegistry::new(),
            user_cmp,
            options,
            stats: Stats::new(),
        })
    }

    /// A snapshot of the engine's runtime counters.
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// Store `value` under `key`.
    pub fn put(&self, opts: &WriteOptions, key: &[u8], value: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.put(key, value);
        self.write(opts, batch)
    }

    /// Remove `key`.
    pub fn delete(&self, opts: &WriteOptions, key: &[u8]) -> Result<()> {
        let mut batch = WriteBatch::new();
        batch.delete(key);
        self.write(opts, batch)
    }

    /// Apply a batch atomically.
    pub fn write(&self, _opts: &WriteOptions, mut batch: WriteBatch) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut core = self.core.lock().unwrap();
        let base = core.vset.last_sequence() + 1;
        let count = batch.count() as u64;
        batch.set_sequence(base);

        // Durability: log the batch before applying it.
        core.log.add_record(batch.contents());
        self.stats.record_write(batch.approximate_size());

        batch.insert_into(&mut core.mem)?;
        core.vset.set_last_sequence(base + count - 1);

        if core.mem.approximate_memory_usage() >= self.options.write_buffer_size {
            let files = Self::flush_core(&mut core, &self.options)?;
            self.stats.record_flush(files as u64);
        }
        Ok(())
    }

    /// Look up `key`, returning its value or `None` if absent or deleted.
    pub fn get(&self, opts: &ReadOptions, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let result = self.get_inner(opts, key);
        if let Ok(value) = &result {
            self.stats.record_read(value.is_some());
        }
        result
    }

    fn get_inner(&self, opts: &ReadOptions, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let core = self.core.lock().unwrap();
        let seq = opts
            .snapshot
            .as_ref()
            .map(|s| s.sequence())
            .unwrap_or_else(|| core.vset.last_sequence());
        let lookup = LookupKey::new(key, seq);

        // 1. Active memtable.
        if let Some(res) = core.mem.get(&lookup) {
            return Self::map_memtable_result(res);
        }
        // 2. Memtables awaiting flush, newest first.
        for m in core.imm.iter().rev() {
            if let Some(res) = m.get(&lookup) {
                return Self::map_memtable_result(res);
            }
        }
        // 3. SSTables, merged.
        let version = core.vset.current().clone();
        let files = version.all_files();
        if files.is_empty() {
            return Ok(None);
        }
        let mut children: Vec<Box<dyn InternalIterator>> = Vec::with_capacity(files.len());
        for f in &files {
            let table = core.vset.get_table(f)?;
            children.push(Box::new(table.iter()));
        }
        let mut merge = MergingIterator::new(children, Arc::clone(&self.user_cmp));
        merge.seek(lookup.internal_key());
        if merge.valid() {
            if let Some(parsed) = ParsedInternalKey::decode(merge.key()) {
                if self.user_cmp.compare(parsed.user_key, key) == std::cmp::Ordering::Equal {
                    return match parsed.value_type {
                        ValueType::Value => Ok(Some(merge.value().to_vec())),
                        ValueType::Deletion => Ok(None),
                    };
                }
            }
        }
        merge.status()?;
        Ok(None)
    }

    fn map_memtable_result(res: Result<Vec<u8>>) -> Result<Option<Vec<u8>>> {
        match res {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Force the active memtable (and anything already frozen) to SSTables.
    ///
    /// Primarily a test and maintenance hook; normally flushing is driven by the
    /// write-buffer threshold.
    pub fn flush(&self) -> Result<()> {
        let mut core = self.core.lock().unwrap();
        let files = Self::flush_core(&mut core, &self.options)?;
        self.stats.record_flush(files as u64);
        Ok(())
    }

    fn flush_core(core: &mut DbCore, options: &Options) -> Result<usize> {
        if !core.mem.is_empty() {
            let fresh = MemTable::new(core.icmp.clone());
            let frozen = std::mem::replace(&mut core.mem, fresh);
            core.imm.push_back(Arc::new(frozen));
        }
        let mut files_created = 0usize;
        while let Some(m) = core.imm.pop_front() {
            let number = core.vset.new_file_number();
            let filter = core.vset.filter_policy();
            let mut it = m.iter();
            if let Some(meta) = build_table(number, &core.icmp, options, filter, &mut it)? {
                core.vset.add_file(0, Arc::new(meta));
                files_created += 1;
            }
        }
        Ok(files_created)
    }

    /// Merge every live SSTable into a single bottom-level run, discarding
    /// versions and tombstones no live snapshot can observe.
    pub fn compact(&self) -> Result<CompactionStats> {
        // Flush first so the compaction sees all data.
        self.flush()?;
        let smallest = self
            .snapshots
            .oldest()
            .unwrap_or(SequenceNumber::MAX);
        let mut core = self.core.lock().unwrap();
        let last = core.vset.last_sequence();
        let bound = smallest.min(last);
        let stats = compact_to_bottom(&mut core.vset, bound)?;
        self.stats.record_compaction(stats.output_files as u64);
        Ok(stats)
    }

    /// Capture a snapshot at the current sequence number.
    pub fn snapshot(&self) -> Snapshot {
        let core = self.core.lock().unwrap();
        self.snapshots.snapshot_at(core.vset.last_sequence())
    }

    /// The most recently assigned sequence number.
    pub fn latest_sequence(&self) -> SequenceNumber {
        self.core.lock().unwrap().vset.last_sequence()
    }

    /// Number of live SSTable files across all levels.
    pub fn live_file_count(&self) -> usize {
        self.core.lock().unwrap().vset.current().total_files()
    }

    /// Create a forward iterator over the whole key space at a snapshot.
    ///
    /// To present a consistent view the active memtable is frozen at creation, so
    /// data written afterwards is not observed by this iterator.
    pub fn new_iterator(&self, opts: &ReadOptions) -> Result<DbIterator> {
        let mut core = self.core.lock().unwrap();
        let seq = opts
            .snapshot
            .as_ref()
            .map(|s| s.sequence())
            .unwrap_or_else(|| core.vset.last_sequence());

        // Freeze the active memtable so the scan sees a stable set of sources.
        if !core.mem.is_empty() {
            let fresh = MemTable::new(core.icmp.clone());
            let frozen = std::mem::replace(&mut core.mem, fresh);
            core.imm.push_back(Arc::new(frozen));
        }

        let mut children: Vec<Box<dyn InternalIterator>> = Vec::new();
        for m in core.imm.iter() {
            children.push(Box::new(m.iter()));
        }
        let version = core.vset.current().clone();
        for f in &version.all_files() {
            let table = core.vset.get_table(f)?;
            children.push(Box::new(table.iter()));
        }

        let merge = MergingIterator::new(children, Arc::clone(&self.user_cmp));
        Ok(DbIterator::new(merge, Arc::clone(&self.user_cmp), seq))
    }

    /// Build a seek target that sorts before every version of `user_key`.
    pub(crate) fn scan_seek_key(user_key: &[u8]) -> Vec<u8> {
        let mut k = Vec::new();
        append_internal_key(&mut k, user_key, MAX_SEQUENCE_NUMBER, VALUE_TYPE_FOR_SEEK);
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open("t", Options::default()).unwrap()
    }

    #[test]
    fn put_get_delete_in_memtable() {
        let d = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        d.put(&wo, b"a", b"1").unwrap();
        d.put(&wo, b"b", b"2").unwrap();
        assert_eq!(d.get(&ro, b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(d.get(&ro, b"b").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(d.get(&ro, b"missing").unwrap(), None);
        d.delete(&wo, b"a").unwrap();
        assert_eq!(d.get(&ro, b"a").unwrap(), None);
    }

    #[test]
    fn read_from_sstable_after_flush() {
        let d = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        for i in 0..100u32 {
            d.put(&wo, format!("key{i:03}").as_bytes(), format!("v{i}").as_bytes())
                .unwrap();
        }
        d.flush().unwrap();
        assert!(d.live_file_count() >= 1);
        assert_eq!(d.get(&ro, b"key050").unwrap().as_deref(), Some(&b"v50"[..]));
        assert_eq!(d.get(&ro, b"key099").unwrap().as_deref(), Some(&b"v99"[..]));
        assert_eq!(d.get(&ro, b"key100").unwrap(), None);
    }

    #[test]
    fn snapshot_isolation() {
        let d = db();
        let wo = WriteOptions::default();
        d.put(&wo, b"k", b"v1").unwrap();
        let snap = d.snapshot();
        d.put(&wo, b"k", b"v2").unwrap();
        let ro_snap = ReadOptions {
            snapshot: Some(snap),
            ..Default::default()
        };
        assert_eq!(d.get(&ro_snap, b"k").unwrap().as_deref(), Some(&b"v1"[..]));
        let ro = ReadOptions::default();
        assert_eq!(d.get(&ro, b"k").unwrap().as_deref(), Some(&b"v2"[..]));
    }

    #[test]
    fn compaction_reduces_files_distinct_keys() {
        let d = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        d.put(&wo, b"a", b"1").unwrap();
        d.flush().unwrap();
        d.put(&wo, b"b", b"2").unwrap();
        d.flush().unwrap();
        d.put(&wo, b"c", b"3").unwrap();
        d.flush().unwrap();
        assert_eq!(d.live_file_count(), 3);
        let stats = d.compact().unwrap();
        assert_eq!(stats.input_files, 3);
        assert_eq!(d.live_file_count(), 1);
        // All keys still resolve after compaction.
        assert_eq!(d.get(&ro, b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(d.get(&ro, b"b").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(d.get(&ro, b"c").unwrap().as_deref(), Some(&b"3"[..]));
    }
}
