//! Compaction: reclaiming space and restoring read efficiency.
//!
//! As flushes accumulate overlapping level-0 files, a point read must merge more
//! and more sorted runs. Compaction rewrites a set of input files into fewer,
//! larger, non-overlapping output files, discarding records that no live snapshot
//! can observe: superseded versions of a key and — at the bottom level, where no
//! older data can exist beneath them — deletion tombstones.
//!
//! The routine here performs a *full* compaction, folding every live file into a
//! single run at the bottom level. It streams its inputs through the shared
//! [`MergingIterator`] and, walking the merged stream, keeps the first version it
//! encounters of each user key and drops the versions that fall at or below the
//! smallest live snapshot.

pub mod picker;

pub use picker::{Compaction, CompactionPicker, L0_COMPACTION_TRIGGER};

use std::sync::Arc;

use crate::error::Result;
use crate::iterator::merging::MergingIterator;
use crate::iterator::InternalIterator;
use crate::key::{ParsedInternalKey, SequenceNumber, ValueType, MAX_SEQUENCE_NUMBER};
use crate::options::CompressionType;
use crate::sstable::table_builder::{TableBuildOptions, TableBuilder};
use crate::version::{FileMetaData, VersionSet, NUM_LEVELS};

/// Outcome of a compaction run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionStats {
    /// Files consumed as input.
    pub input_files: usize,
    /// Files produced as output.
    pub output_files: usize,
    /// Level the outputs were written to.
    pub output_level: usize,
    /// Records written to the output.
    pub records_written: usize,
    /// Records dropped as unobservable.
    pub records_dropped: usize,
}

/// Fold every live file into a single run at the bottom level.
///
/// `smallest_snapshot` is the oldest sequence number any live snapshot can still
/// observe; versions at or below it that are shadowed by a newer version, and
/// bottom-level tombstones at or below it, are dropped.
pub fn compact_to_bottom(
    vset: &mut VersionSet,
    smallest_snapshot: SequenceNumber,
) -> Result<CompactionStats> {
    let version = vset.current().clone();

    let mut removed: Vec<(usize, u64)> = Vec::new();
    let mut children: Vec<Box<dyn InternalIterator>> = Vec::new();
    for level in 0..NUM_LEVELS {
        for f in &version.files[level] {
            removed.push((level, f.number));
            let table = vset.get_table(f)?;
            children.push(Box::new(table.iter()));
        }
    }

    if children.is_empty() {
        return Ok(CompactionStats {
            input_files: 0,
            output_files: 0,
            output_level: NUM_LEVELS - 1,
            records_written: 0,
            records_dropped: 0,
        });
    }

    let output_level = NUM_LEVELS - 1;
    let user_cmp = Arc::clone(vset.icmp().user_comparator());
    let icmp = vset.icmp().clone();
    let filter_policy = vset.filter_policy();
    let block_size = vset.options().block_size;
    let restart = vset.options().block_restart_interval;
    let number = vset.new_file_number();

    let mut merge = MergingIterator::new(children, user_cmp);
    merge.seek_to_first();

    let mut builder: Option<TableBuilder> = None;
    let mut smallest: Option<Vec<u8>> = None;
    let mut largest: Vec<u8> = Vec::new();
    let mut last_user_key: Option<Vec<u8>> = None;
    let mut last_seq_for_key = MAX_SEQUENCE_NUMBER;
    let mut written = 0usize;
    let mut dropped = 0usize;

    while merge.valid() {
        let ikey = merge.key().to_vec();
        let value = merge.value().to_vec();
        let parsed = ParsedInternalKey::decode(&ikey)
            .ok_or_else(|| crate::error::Error::corruption("compaction saw bad internal key"))?;

        let fresh_user_key = last_user_key.as_deref() != Some(parsed.user_key);
        if fresh_user_key {
            last_user_key = Some(parsed.user_key.to_vec());
            last_seq_for_key = MAX_SEQUENCE_NUMBER;
        }

        let mut drop = false;
        if last_seq_for_key <= smallest_snapshot {
            // A newer version of this user key was already emitted and is visible
            // to every snapshot, so this older one can never be read.
            drop = true;
        } else if parsed.value_type == ValueType::Deletion && parsed.sequence <= smallest_snapshot {
            // Bottom level: nothing older exists beneath, so the tombstone has
            // nothing left to hide and can be discarded outright.
            drop = true;
        }
        last_seq_for_key = parsed.sequence;

        if drop {
            dropped += 1;
        } else {
            if builder.is_none() {
                builder = Some(TableBuilder::new(TableBuildOptions {
                    block_size,
                    block_restart_interval: restart,
                    compression: CompressionType::None,
                    filter_policy: filter_policy.clone(),
                    icmp: icmp.clone(),
                }));
            }
            let b = builder.as_mut().unwrap();
            if smallest.is_none() {
                smallest = Some(ikey.clone());
            }
            largest.clear();
            largest.extend_from_slice(&ikey);
            b.add(&ikey, &value);
            written += 1;
        }

        merge.next();
    }
    merge.status()?;

    let mut outputs: Vec<Arc<FileMetaData>> = Vec::new();
    if let Some(b) = builder {
        let file_size = b.file_size();
        let blob = b.finish();
        outputs.push(Arc::new(FileMetaData {
            number,
            file_size,
            smallest: smallest.unwrap_or_default(),
            largest,
            blob: Arc::new(blob),
        }));
    }

    let output_files = outputs.len();
    vset.apply_compaction(&removed, output_level, outputs);

    Ok(CompactionStats {
        input_files: removed.len(),
        output_files,
        output_level,
        records_written: written,
        records_dropped: dropped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::append_internal_key;
    use crate::options::Options;
    use crate::version::build_table;

    fn ik(user: &[u8], seq: u64, t: ValueType) -> Vec<u8> {
        let mut k = Vec::new();
        append_internal_key(&mut k, user, seq, t);
        k
    }

    struct Src {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        pos: usize,
        started: bool,
    }
    impl InternalIterator for Src {
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

    /// Compaction over files with entirely distinct keys just concatenates them.
    #[test]
    fn full_compaction_preserves_distinct_keys() {
        let options = Options::default();
        let mut vset = VersionSet::new(options.clone());
        let icmp = vset.icmp().clone();

        // Two files, disjoint key ranges.
        let mut e1 = Src {
            entries: vec![(ik(b"a", 5, ValueType::Value), b"1".to_vec()),
                          (ik(b"b", 5, ValueType::Value), b"2".to_vec())],
            pos: 0,
            started: false,
        };
        let n1 = vset.new_file_number();
        let f1 = build_table(n1, &icmp, &options, None, &mut e1).unwrap().unwrap();
        vset.add_file(0, Arc::new(f1));

        let mut e2 = Src {
            entries: vec![(ik(b"c", 6, ValueType::Value), b"3".to_vec()),
                          (ik(b"d", 6, ValueType::Value), b"4".to_vec())],
            pos: 0,
            started: false,
        };
        let n2 = vset.new_file_number();
        let f2 = build_table(n2, &icmp, &options, None, &mut e2).unwrap().unwrap();
        vset.add_file(0, Arc::new(f2));

        // Bound above every sequence used, but below MAX so first versions are kept.
        let stats = compact_to_bottom(&mut vset, 1_000).unwrap();
        assert_eq!(stats.input_files, 2);
        assert_eq!(stats.output_files, 1);
        assert_eq!(stats.records_written, 4);
        assert_eq!(stats.records_dropped, 0);
        // The single output file now lives at the bottom level.
        assert_eq!(vset.current().level_files(NUM_LEVELS - 1), 1);
        assert_eq!(vset.current().level_files(0), 0);
    }
}
