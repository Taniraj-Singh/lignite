//! A k-way merge presented as a single [`InternalIterator`].
//!
//! The merging iterator is the seam where all of the engine's sorted runs come
//! together. A read builds one child iterator per source that could hold a
//! relevant version of a key — the mutable memtable, each immutable memtable
//! awaiting flush, and every SSTable selected from the version's levels — and
//! wraps them here. Compaction uses the very same machinery to stream the union
//! of its input tables in sorted order.
//!
//! The merge itself follows the LevelDB design: rather than a binary heap it
//! keeps the children individually positioned and, on each step, performs a
//! linear scan to select the extreme child for the current direction. For the
//! small fan-in an LSM read actually produces (a handful of memtables plus one
//! table per level) the linear scan is cheaper than heap bookkeeping and keeps
//! the direction-reversal logic straightforward.

use std::cmp::Ordering;
use std::sync::Arc;

use crate::comparator::Comparator;
use crate::error::Result;
use crate::iterator::InternalIterator;
use crate::key::{extract_trailer, extract_user_key};

/// Which way the cursor is currently travelling. The children are only
/// guaranteed to be consistently positioned for the active direction, so a
/// change of direction re-seeks the non-current children before proceeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Reverse,
}

/// Merges several child [`InternalIterator`]s into one sorted stream.
pub struct MergingIterator {
    children: Vec<Box<dyn InternalIterator>>,
    user_cmp: Arc<dyn Comparator>,
    current: Option<usize>,
    direction: Direction,
}

impl MergingIterator {
    /// Build a merging iterator over `children`, ordering user keys with
    /// `user_cmp`. An empty child set yields a permanently-invalid iterator.
    pub fn new(children: Vec<Box<dyn InternalIterator>>, user_cmp: Arc<dyn Comparator>) -> Self {
        MergingIterator {
            children,
            user_cmp,
            current: None,
            direction: Direction::Forward,
        }
    }

    /// Number of child iterators being merged.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Total order over two internal keys as used by the merge.
    ///
    /// The user-key portion is delegated to the configured comparator; the
    /// eight-byte packed trailer resolves records that share a user key.
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        match self.user_cmp.compare(extract_user_key(a), extract_user_key(b)) {
            Ordering::Equal => extract_trailer(a).cmp(&extract_trailer(b)),
            other => other,
        }
    }

    /// Select the child holding the smallest key as the new current child.
    fn find_smallest(&mut self) {
        let mut smallest: Option<usize> = None;
        for i in 0..self.children.len() {
            if !self.children[i].valid() {
                continue;
            }
            match smallest {
                None => smallest = Some(i),
                Some(s) => {
                    let key_i = self.children[i].key();
                    let key_s = self.children[s].key();
                    if self.compare(key_i, key_s) == Ordering::Less {
                        smallest = Some(i);
                    }
                }
            }
        }
        self.current = smallest;
    }

    /// Select the child holding the largest key as the new current child.
    fn find_largest(&mut self) {
        let mut largest: Option<usize> = None;
        for i in (0..self.children.len()).rev() {
            if !self.children[i].valid() {
                continue;
            }
            match largest {
                None => largest = Some(i),
                Some(l) => {
                    let key_i = self.children[i].key();
                    let key_l = self.children[l].key();
                    if self.compare(key_i, key_l) == Ordering::Greater {
                        largest = Some(i);
                    }
                }
            }
        }
        self.current = largest;
    }
}

impl InternalIterator for MergingIterator {
    fn valid(&self) -> bool {
        self.current.is_some()
    }

    fn seek_to_first(&mut self) {
        for child in &mut self.children {
            child.seek_to_first();
        }
        self.direction = Direction::Forward;
        self.find_smallest();
    }

    fn seek_to_last(&mut self) {
        for child in &mut self.children {
            child.seek_to_last();
        }
        self.direction = Direction::Reverse;
        self.find_largest();
    }

    fn seek(&mut self, target: &[u8]) {
        for child in &mut self.children {
            child.seek(target);
        }
        self.direction = Direction::Forward;
        self.find_smallest();
    }

    fn next(&mut self) {
        let current = match self.current {
            Some(c) => c,
            None => return,
        };

        // To step forward, every other child must be positioned strictly after
        // the current key. If we were travelling in reverse this is not the
        // case, so re-anchor the others just past the current key first.
        if self.direction != Direction::Forward {
            let current_key = self.children[current].key().to_vec();
            for i in 0..self.children.len() {
                if i == current {
                    continue;
                }
                self.children[i].seek(&current_key);
                if self.children[i].valid()
                    && self.compare(&current_key, self.children[i].key()) == Ordering::Equal
                {
                    self.children[i].next();
                }
            }
            self.direction = Direction::Forward;
        }

        self.children[current].next();
        self.find_smallest();
    }

    fn prev(&mut self) {
        let current = match self.current {
            Some(c) => c,
            None => return,
        };

        // Symmetric to `next`: to step backward, every other child must sit
        // strictly before the current key.
        if self.direction != Direction::Reverse {
            let current_key = self.children[current].key().to_vec();
            for i in 0..self.children.len() {
                if i == current {
                    continue;
                }
                self.children[i].seek(&current_key);
                if self.children[i].valid() {
                    self.children[i].prev();
                } else {
                    self.children[i].seek_to_last();
                }
            }
            self.direction = Direction::Reverse;
        }

        self.children[current].prev();
        self.find_largest();
    }

    fn key(&self) -> &[u8] {
        let c = self.current.expect("key() on invalid MergingIterator");
        self.children[c].key()
    }

    fn value(&self) -> &[u8] {
        let c = self.current.expect("value() on invalid MergingIterator");
        self.children[c].value()
    }

    fn status(&self) -> Result<()> {
        for child in &self.children {
            child.status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::{append_internal_key, ValueType};

    /// A trivial in-memory iterator over pre-sorted (internal key, value) pairs.
    struct VecIter {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        pos: usize,
        valid: bool,
        icmp: crate::key::InternalKeyComparator,
    }

    impl VecIter {
        fn new(mut entries: Vec<(Vec<u8>, Vec<u8>)>) -> Box<dyn InternalIterator> {
            let icmp = crate::key::InternalKeyComparator::new(Arc::new(BytewiseComparator));
            entries.sort_by(|a, b| icmp.compare(&a.0, &b.0));
            Box::new(VecIter {
                entries,
                pos: 0,
                valid: false,
                icmp,
            })
        }
    }

    impl InternalIterator for VecIter {
        fn valid(&self) -> bool {
            self.valid && self.pos < self.entries.len()
        }
        fn seek_to_first(&mut self) {
            self.pos = 0;
            self.valid = !self.entries.is_empty();
        }
        fn seek_to_last(&mut self) {
            if self.entries.is_empty() {
                self.valid = false;
            } else {
                self.pos = self.entries.len() - 1;
                self.valid = true;
            }
        }
        fn seek(&mut self, target: &[u8]) {
            self.pos = self
                .entries
                .partition_point(|(k, _)| self.icmp.compare(k, target) == Ordering::Less);
            self.valid = self.pos < self.entries.len();
        }
        fn next(&mut self) {
            self.pos += 1;
            if self.pos >= self.entries.len() {
                self.valid = false;
            }
        }
        fn prev(&mut self) {
            if self.pos == 0 {
                self.valid = false;
            } else {
                self.pos -= 1;
            }
        }
        fn key(&self) -> &[u8] {
            &self.entries[self.pos].0
        }
        fn value(&self) -> &[u8] {
            &self.entries[self.pos].1
        }
    }

    fn ikey(user: &[u8], seq: u64) -> Vec<u8> {
        let mut k = Vec::new();
        append_internal_key(&mut k, user, seq, ValueType::Value);
        k
    }

    fn collect_user_keys(it: &mut MergingIterator) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        it.seek_to_first();
        while it.valid() {
            out.push(extract_user_key(it.key()).to_vec());
            it.next();
        }
        out
    }

    #[test]
    fn merges_disjoint_runs_in_order() {
        let a = VecIter::new(vec![(ikey(b"a", 5), b"1".to_vec()), (ikey(b"c", 5), b"3".to_vec())]);
        let b = VecIter::new(vec![(ikey(b"b", 5), b"2".to_vec()), (ikey(b"d", 5), b"4".to_vec())]);
        let mut m = MergingIterator::new(vec![a, b], Arc::new(BytewiseComparator));
        let keys = collect_user_keys(&mut m);
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn seek_positions_on_first_ge() {
        let a = VecIter::new(vec![(ikey(b"a", 1), b"x".to_vec()), (ikey(b"e", 1), b"y".to_vec())]);
        let b = VecIter::new(vec![(ikey(b"c", 1), b"z".to_vec())]);
        let mut m = MergingIterator::new(vec![a, b], Arc::new(BytewiseComparator));
        m.seek(&ikey(b"b", 1));
        assert!(m.valid());
        assert_eq!(extract_user_key(m.key()), b"c");
    }

    #[test]
    fn empty_merge_is_invalid() {
        let mut m = MergingIterator::new(vec![], Arc::new(BytewiseComparator));
        m.seek_to_first();
        assert!(!m.valid());
    }

    #[test]
    fn single_child_passthrough() {
        let a = VecIter::new(vec![
            (ikey(b"k1", 9), b"v1".to_vec()),
            (ikey(b"k2", 9), b"v2".to_vec()),
            (ikey(b"k3", 9), b"v3".to_vec()),
        ]);
        let mut m = MergingIterator::new(vec![a], Arc::new(BytewiseComparator));
        let keys = collect_user_keys(&mut m);
        assert_eq!(keys.len(), 3);
    }
}
