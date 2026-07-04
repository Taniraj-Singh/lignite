//! Versions: the catalogue of live SSTables organised into levels.
//!
//! A [`Version`] is an immutable snapshot of which files exist at each level. The
//! [`VersionSet`] owns the current version, hands out monotonic file and sequence
//! numbers, and caches parsed [`Table`] handles so repeated reads of a file do
//! not re-parse its index and filter blocks.
//!
//! Level 0 is special: its files come straight from memtable flushes and may have
//! overlapping key ranges, so a read must consider all of them. Levels `>= 1` are
//! produced by compaction and hold non-overlapping files sorted by key. Because
//! reads here funnel every candidate file through the merging iterator, the
//! ordering guarantees of that iterator are what make a multi-file, multi-version
//! read return the correct value.

pub mod version_edit;

pub use version_edit::{EditFile, VersionEdit};

use std::sync::Arc;

use crate::error::Result;
use crate::filter::{BloomFilterPolicy, FilterPolicy};
use crate::iterator::InternalIterator;
use crate::key::{extract_user_key, InternalKeyComparator, SequenceNumber};
use crate::options::{CompressionType, Options};
use crate::cache::ShardedCache;
use crate::sstable::table::Table;
use crate::sstable::table_builder::{TableBuildOptions, TableBuilder};

/// Number of levels in the tree.
pub const NUM_LEVELS: usize = 7;

/// Metadata describing one on-disk (here, in-memory) SSTable.
#[derive(Clone)]
pub struct FileMetaData {
    /// Unique, monotonically assigned file number.
    pub number: u64,
    /// Encoded size of the table image in bytes.
    pub file_size: u64,
    /// Smallest internal key in the file.
    pub smallest: Vec<u8>,
    /// Largest internal key in the file.
    pub largest: Vec<u8>,
    /// The table image itself.
    pub blob: Arc<Vec<u8>>,
}

impl std::fmt::Debug for FileMetaData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileMetaData")
            .field("number", &self.number)
            .field("file_size", &self.file_size)
            .field("smallest_user", &String::from_utf8_lossy(extract_user_key(&self.smallest)))
            .field("largest_user", &String::from_utf8_lossy(extract_user_key(&self.largest)))
            .finish()
    }
}

/// An immutable snapshot of the files present at each level.
#[derive(Clone, Default)]
pub struct Version {
    /// `files[level]` lists the tables at that level.
    pub files: Vec<Vec<Arc<FileMetaData>>>,
}

impl Version {
    /// A version with all levels empty.
    pub fn empty() -> Self {
        Version {
            files: vec![Vec::new(); NUM_LEVELS],
        }
    }

    /// Total number of files across all levels.
    pub fn total_files(&self) -> usize {
        self.files.iter().map(|l| l.len()).sum()
    }

    /// Number of files at `level`.
    pub fn level_files(&self, level: usize) -> usize {
        self.files.get(level).map(|l| l.len()).unwrap_or(0)
    }

    /// Every file, ordered so that newer data is encountered first: level-0
    /// files newest-first, then each deeper level in key order. The merging
    /// iterator re-sorts by key regardless, but this ordering documents intent.
    pub fn all_files(&self) -> Vec<Arc<FileMetaData>> {
        let mut out = Vec::new();
        // Level 0: newest (highest file number) first.
        let mut level0 = self.files[0].clone();
        level0.sort_by_key(|f| std::cmp::Reverse(f.number));
        out.extend(level0);
        for level in 1..NUM_LEVELS {
            out.extend(self.files[level].iter().cloned());
        }
        out
    }
}

/// Owns the current [`Version`] and allocates file/sequence numbers.
pub struct VersionSet {
    icmp: InternalKeyComparator,
    options: Options,
    filter_policy: Option<Arc<dyn FilterPolicy>>,
    next_file_number: u64,
    last_sequence: SequenceNumber,
    current: Version,
    table_cache: ShardedCache<u64, Arc<Table>>,
}

impl VersionSet {
    /// Create a version set with an empty current version.
    pub fn new(options: Options) -> Self {
        let icmp = InternalKeyComparator::new(Arc::clone(&options.comparator));
        let filter_policy: Option<Arc<dyn FilterPolicy>> = if options.bloom_bits_per_key > 0 {
            Some(Arc::new(BloomFilterPolicy::new(options.bloom_bits_per_key)))
        } else {
            None
        };
        let table_cache = ShardedCache::new(options.max_open_files.max(16), 4);
        VersionSet {
            icmp,
            options,
            filter_policy,
            next_file_number: 1,
            last_sequence: 0,
            current: Version::empty(),
            table_cache,
        }
    }

    /// The internal-key comparator.
    pub fn icmp(&self) -> &InternalKeyComparator {
        &self.icmp
    }

    /// The database options in effect.
    pub fn options(&self) -> &Options {
        &self.options
    }

    /// The filter policy in effect, if any.
    pub fn filter_policy(&self) -> Option<Arc<dyn FilterPolicy>> {
        self.filter_policy.clone()
    }

    /// Allocate a fresh file number.
    pub fn new_file_number(&mut self) -> u64 {
        let n = self.next_file_number;
        self.next_file_number += 1;
        n
    }

    /// The most recently assigned sequence number.
    pub fn last_sequence(&self) -> SequenceNumber {
        self.last_sequence
    }

    /// Advance the last-used sequence number.
    pub fn set_last_sequence(&mut self, seq: SequenceNumber) {
        debug_assert!(seq >= self.last_sequence);
        self.last_sequence = seq;
    }

    /// The current version.
    pub fn current(&self) -> &Version {
        &self.current
    }

    /// Register a freshly-written file at `level`, producing a new version.
    pub fn add_file(&mut self, level: usize, meta: Arc<FileMetaData>) {
        let mut next = self.current.clone();
        next.files[level].push(meta);
        if level > 0 {
            next.files[level].sort_by(|a, b| self.icmp.compare(&a.smallest, &b.smallest));
        }
        self.current = next;
    }

    /// Replace `inputs` (a list of `(level, file_number)`) with `outputs` at
    /// `output_level`, producing a new version. Used by compaction.
    pub fn apply_compaction(
        &mut self,
        removed: &[(usize, u64)],
        output_level: usize,
        outputs: Vec<Arc<FileMetaData>>,
    ) {
        let mut next = self.current.clone();
        for (level, number) in removed {
            next.files[*level].retain(|f| f.number != *number);
            self.table_cache.remove(number);
        }
        for out in outputs {
            next.files[output_level].push(out);
        }
        if output_level > 0 {
            next.files[output_level].sort_by(|a, b| self.icmp.compare(&a.smallest, &b.smallest));
        }
        self.current = next;
    }

    /// Open (and cache) the parsed table for `meta`.
    pub fn get_table(&self, meta: &FileMetaData) -> Result<Arc<Table>> {
        if let Some(t) = self.table_cache.get(&meta.number) {
            return Ok(t);
        }
        let table = Table::open(
            Arc::clone(&meta.blob),
            self.icmp.clone(),
            self.filter_policy.clone(),
            self.options.paranoid_checks,
        )?;
        let arc = Arc::new(table);
        self.table_cache.insert(meta.number, Arc::clone(&arc), 1);
        Ok(arc)
    }
}

/// Build an SSTable from the records produced by `iter` (which must yield
/// internal keys in ascending order). Returns `None` if the iterator is empty.
pub fn build_table(
    number: u64,
    icmp: &InternalKeyComparator,
    options: &Options,
    filter_policy: Option<Arc<dyn FilterPolicy>>,
    iter: &mut dyn InternalIterator,
) -> Result<Option<FileMetaData>> {
    iter.seek_to_first();
    if !iter.valid() {
        return Ok(None);
    }

    let build_opts = TableBuildOptions {
        block_size: options.block_size,
        block_restart_interval: options.block_restart_interval,
        compression: CompressionType::None,
        filter_policy,
        icmp: icmp.clone(),
    };
    let mut builder = TableBuilder::new(build_opts);

    let mut smallest: Option<Vec<u8>> = None;
    let mut largest = Vec::new();
    while iter.valid() {
        let key = iter.key();
        if smallest.is_none() {
            smallest = Some(key.to_vec());
        }
        largest.clear();
        largest.extend_from_slice(key);
        builder.add(key, iter.value());
        iter.next();
    }
    iter.status()?;

    let file_size = builder.file_size();
    let blob = builder.finish();
    Ok(Some(FileMetaData {
        number,
        file_size,
        smallest: smallest.unwrap_or_default(),
        largest,
        blob: Arc::new(blob),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::{append_internal_key, ValueType};

    struct OneShot {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        pos: usize,
        started: bool,
    }
    impl InternalIterator for OneShot {
        fn valid(&self) -> bool {
            self.started && self.pos < self.entries.len()
        }
        fn seek_to_first(&mut self) {
            self.started = true;
            self.pos = 0;
        }
        fn seek_to_last(&mut self) {}
        fn seek(&mut self, _t: &[u8]) {}
        fn next(&mut self) {
            self.pos += 1;
        }
        fn prev(&mut self) {}
        fn key(&self) -> &[u8] {
            &self.entries[self.pos].0
        }
        fn value(&self) -> &[u8] {
            &self.entries[self.pos].1
        }
    }

    #[test]
    fn build_table_captures_bounds() {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let options = Options::default();
        let mut entries = Vec::new();
        for u in [b"a".as_slice(), b"m", b"z"] {
            let mut ik = Vec::new();
            append_internal_key(&mut ik, u, 1, ValueType::Value);
            entries.push((ik, b"v".to_vec()));
        }
        let mut it = OneShot { entries, pos: 0, started: false };
        let meta = build_table(7, &icmp, &options, None, &mut it).unwrap().unwrap();
        assert_eq!(meta.number, 7);
        assert_eq!(extract_user_key(&meta.smallest), b"a");
        assert_eq!(extract_user_key(&meta.largest), b"z");
    }

    #[test]
    fn version_set_hands_out_numbers() {
        let mut vs = VersionSet::new(Options::default());
        assert_eq!(vs.new_file_number(), 1);
        assert_eq!(vs.new_file_number(), 2);
        vs.set_last_sequence(10);
        assert_eq!(vs.last_sequence(), 10);
        assert_eq!(vs.current().total_files(), 0);
    }
}
