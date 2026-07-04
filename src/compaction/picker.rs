//! Choosing what to compact.
//!
//! The picker inspects a [`Version`] and decides whether — and where — a
//! compaction is worthwhile, using the classic size/count heuristics:
//!
//! * **Level 0** is scored by *file count*, because its files overlap and every
//!   one must be consulted on a read; too many level-0 files slow every lookup.
//! * **Levels ≥ 1** are scored by *total bytes* against a per-level budget that
//!   grows geometrically, so deeper levels hold exponentially more data.
//!
//! The level with the highest score `>= 1.0` is compacted. Having chosen a level,
//! the picker selects a seed file and expands the selection to the set of files
//! at the next level whose key ranges overlap it, so the rewrite produces
//! non-overlapping output. All of this is pure computation over file metadata; it
//! performs no I/O.

use std::sync::Arc;

use crate::key::InternalKeyComparator;
use crate::version::{FileMetaData, Version, NUM_LEVELS};

/// Level-0 is compacted once it holds this many files.
pub const L0_COMPACTION_TRIGGER: usize = 4;

/// Base byte budget for level 1; each deeper level multiplies by 10.
pub const LEVEL1_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// The maximum total bytes a level may hold before it wants compacting.
pub fn max_bytes_for_level(level: usize) -> u64 {
    // Levels 0 and 1 share the base budget; each level beyond multiplies by 10.
    let mut result = LEVEL1_MAX_BYTES;
    let mut l = level;
    while l > 1 {
        result = result.saturating_mul(10);
        l -= 1;
    }
    result
}

/// The total encoded size of all files at `level`.
pub fn total_file_size(files: &[Arc<FileMetaData>]) -> u64 {
    files.iter().map(|f| f.file_size).sum()
}

/// A description of a chosen compaction.
#[derive(Debug, Clone)]
pub struct Compaction {
    /// The level being compacted.
    pub level: usize,
    /// Files selected from `level`.
    pub inputs: Vec<Arc<FileMetaData>>,
    /// Overlapping files pulled in from `level + 1`.
    pub overlap: Vec<Arc<FileMetaData>>,
    /// The score that motivated the compaction.
    pub score: f64,
}

impl Compaction {
    /// Level the outputs are written to.
    pub fn output_level(&self) -> usize {
        (self.level + 1).min(NUM_LEVELS - 1)
    }

    /// Total files consumed (both levels).
    pub fn input_file_count(&self) -> usize {
        self.inputs.len() + self.overlap.len()
    }
}

/// Scores each level and picks one to compact.
pub struct CompactionPicker {
    icmp: InternalKeyComparator,
}

impl CompactionPicker {
    /// Create a picker using `icmp` for key-range overlap tests.
    pub fn new(icmp: InternalKeyComparator) -> Self {
        CompactionPicker { icmp }
    }

    /// Compute the compaction score for `level` in `version`.
    ///
    /// A score `>= 1.0` means the level exceeds its budget.
    pub fn score_level(&self, version: &Version, level: usize) -> f64 {
        if level == 0 {
            version.level_files(0) as f64 / L0_COMPACTION_TRIGGER as f64
        } else {
            total_file_size(&version.files[level]) as f64 / max_bytes_for_level(level) as f64
        }
    }

    /// The level with the greatest score, and that score.
    pub fn pick_level(&self, version: &Version) -> (usize, f64) {
        let mut best_level = 0;
        let mut best_score = 0.0f64;
        for level in 0..NUM_LEVELS - 1 {
            let score = self.score_level(version, level);
            if score > best_score {
                best_score = score;
                best_level = level;
            }
        }
        (best_level, best_score)
    }

    /// Decide on a compaction for `version`, or `None` if none is warranted.
    pub fn pick_compaction(&self, version: &Version) -> Option<Compaction> {
        let (level, score) = self.pick_level(version);
        if score < 1.0 {
            return None;
        }

        // Seed: the whole of level 0 (its files overlap), or the largest file at
        // a deeper level.
        let inputs: Vec<Arc<FileMetaData>> = if level == 0 {
            version.files[0].clone()
        } else {
            let seed = version.files[level]
                .iter()
                .max_by_key(|f| f.file_size)
                .cloned()?;
            vec![seed]
        };

        let (smallest, largest) = self.range_of(&inputs)?;
        let overlap = self.overlapping_files(version, level + 1, &smallest, &largest);

        Some(Compaction {
            level,
            inputs,
            overlap,
            score,
        })
    }

    /// The `(smallest, largest)` internal-key span covering `files`.
    pub fn range_of(&self, files: &[Arc<FileMetaData>]) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut smallest: Option<&[u8]> = None;
        let mut largest: Option<&[u8]> = None;
        for f in files {
            if smallest.map(|s| self.icmp.compare(&f.smallest, s).is_lt()).unwrap_or(true) {
                smallest = Some(&f.smallest);
            }
            if largest.map(|l| self.icmp.compare(&f.largest, l).is_gt()).unwrap_or(true) {
                largest = Some(&f.largest);
            }
        }
        Some((smallest?.to_vec(), largest?.to_vec()))
    }

    /// Files at `level` whose key range intersects `[smallest, largest]`.
    pub fn overlapping_files(
        &self,
        version: &Version,
        level: usize,
        smallest: &[u8],
        largest: &[u8],
    ) -> Vec<Arc<FileMetaData>> {
        if level >= NUM_LEVELS {
            return Vec::new();
        }
        version.files[level]
            .iter()
            .filter(|f| {
                // Disjoint iff f.largest < smallest OR f.smallest > largest.
                let before = self.icmp.compare(&f.largest, smallest).is_lt();
                let after = self.icmp.compare(&f.smallest, largest).is_gt();
                !(before || after)
            })
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::{append_internal_key, ValueType};
    use std::sync::Arc;

    fn icmp() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    fn ik(user: &[u8]) -> Vec<u8> {
        let mut k = Vec::new();
        append_internal_key(&mut k, user, 1, ValueType::Value);
        k
    }

    fn file(number: u64, size: u64, smallest: &[u8], largest: &[u8]) -> Arc<FileMetaData> {
        Arc::new(FileMetaData {
            number,
            file_size: size,
            smallest: ik(smallest),
            largest: ik(largest),
            blob: Arc::new(Vec::new()),
        })
    }

    #[test]
    fn level_budgets_grow_geometrically() {
        assert_eq!(max_bytes_for_level(1), LEVEL1_MAX_BYTES);
        assert_eq!(max_bytes_for_level(2), LEVEL1_MAX_BYTES * 10);
        assert_eq!(max_bytes_for_level(3), LEVEL1_MAX_BYTES * 100);
    }

    #[test]
    fn level0_scored_by_count() {
        let picker = CompactionPicker::new(icmp());
        let mut v = Version::empty();
        for i in 0..L0_COMPACTION_TRIGGER {
            v.files[0].push(file(i as u64, 1, b"a", b"b"));
        }
        assert!((picker.score_level(&v, 0) - 1.0).abs() < 1e-9);
        assert!(picker.pick_compaction(&v).is_some());
    }

    #[test]
    fn no_compaction_when_under_budget() {
        let picker = CompactionPicker::new(icmp());
        let mut v = Version::empty();
        v.files[0].push(file(1, 1, b"a", b"b"));
        assert!(picker.pick_compaction(&v).is_none());
    }

    #[test]
    fn overlap_detection() {
        let picker = CompactionPicker::new(icmp());
        let mut v = Version::empty();
        v.files[1].push(file(10, 1, b"a", b"e"));
        v.files[1].push(file(11, 1, b"f", b"k"));
        v.files[1].push(file(12, 1, b"p", b"z"));
        // Range [d, g] overlaps files 10 (a..e) and 11 (f..k) but not 12.
        let hits = picker.overlapping_files(&v, 1, &ik(b"d"), &ik(b"g"));
        let numbers: Vec<u64> = hits.iter().map(|f| f.number).collect();
        assert_eq!(numbers, vec![10, 11]);
    }

    #[test]
    fn range_of_spans_all_inputs() {
        let picker = CompactionPicker::new(icmp());
        let files = vec![file(1, 1, b"c", b"m"), file(2, 1, b"a", b"f"), file(3, 1, b"k", b"z")];
        let (s, l) = picker.range_of(&files).unwrap();
        assert_eq!(crate::key::extract_user_key(&s), b"a");
        assert_eq!(crate::key::extract_user_key(&l), b"z");
    }

    #[test]
    fn deep_level_scored_by_bytes() {
        let picker = CompactionPicker::new(icmp());
        let mut v = Version::empty();
        // Two files at level 1 whose combined size exceeds the level-1 budget.
        v.files[1].push(file(1, LEVEL1_MAX_BYTES, b"a", b"m"));
        v.files[1].push(file(2, LEVEL1_MAX_BYTES, b"n", b"z"));
        assert!(picker.score_level(&v, 1) >= 1.0);
        let c = picker.pick_compaction(&v).unwrap();
        assert_eq!(c.level, 1);
        assert_eq!(c.output_level(), 2);
    }
}
