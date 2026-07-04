//! A concatenating iterator over a sequence of non-overlapping sorted runs.
//!
//! Levels `>= 1` of the tree hold files whose key ranges do not overlap and are
//! stored in ascending key order. Such a level can be iterated far more cheaply
//! than through the general [`MergingIterator`](super::merging::MergingIterator):
//! at any moment exactly one child is active, so stepping is O(1) rather than
//! O(children). This iterator captures that structure — it walks its children in
//! order, moving to the next only when the current one is exhausted.
//!
//! The children must genuinely be non-overlapping and pre-sorted; the iterator
//! trusts that contract to make a seek land in the single child that can contain
//! the target.

use crate::error::Result;
use crate::iterator::InternalIterator;

/// Chains non-overlapping, ascending child iterators into one stream.
pub struct ConcatenatingIterator {
    children: Vec<Box<dyn InternalIterator>>,
    current: Option<usize>,
}

impl ConcatenatingIterator {
    /// Build over `children`, which must be sorted and non-overlapping.
    pub fn new(children: Vec<Box<dyn InternalIterator>>) -> Self {
        ConcatenatingIterator {
            children,
            current: None,
        }
    }

    /// Number of underlying runs.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// From `start`, find the first child that has a valid position, seeking each
    /// to its first entry. Returns the index or `None` if all are empty.
    fn skip_forward_from(&mut self, start: usize) -> Option<usize> {
        let mut i = start;
        while i < self.children.len() {
            self.children[i].seek_to_first();
            if self.children[i].valid() {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    fn skip_backward_from(&mut self, start: isize) -> Option<usize> {
        let mut i = start;
        while i >= 0 {
            let idx = i as usize;
            self.children[idx].seek_to_last();
            if self.children[idx].valid() {
                return Some(idx);
            }
            i -= 1;
        }
        None
    }
}

impl InternalIterator for ConcatenatingIterator {
    fn valid(&self) -> bool {
        match self.current {
            Some(i) => self.children[i].valid(),
            None => false,
        }
    }

    fn seek_to_first(&mut self) {
        self.current = self.skip_forward_from(0);
    }

    fn seek_to_last(&mut self) {
        let last = self.children.len() as isize - 1;
        self.current = self.skip_backward_from(last);
    }

    fn seek(&mut self, target: &[u8]) {
        // The first child holding any key >= target is the correct one.
        for i in 0..self.children.len() {
            self.children[i].seek(target);
            if self.children[i].valid() {
                self.current = Some(i);
                return;
            }
        }
        self.current = None;
    }

    fn next(&mut self) {
        let current = match self.current {
            Some(c) => c,
            None => return,
        };
        self.children[current].next();
        if !self.children[current].valid() {
            self.current = self.skip_forward_from(current + 1);
        }
    }

    fn prev(&mut self) {
        let current = match self.current {
            Some(c) => c,
            None => return,
        };
        self.children[current].prev();
        if !self.children[current].valid() && current > 0 {
            self.current = self.skip_backward_from(current as isize - 1);
        } else if !self.children[current].valid() {
            self.current = None;
        }
    }

    fn key(&self) -> &[u8] {
        let i = self.current.expect("key() on invalid ConcatenatingIterator");
        self.children[i].key()
    }

    fn value(&self) -> &[u8] {
        let i = self.current.expect("value() on invalid ConcatenatingIterator");
        self.children[i].value()
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
    use crate::key::{append_internal_key, extract_user_key, InternalKeyComparator, ValueType};
    use std::cmp::Ordering;
    use std::sync::Arc;

    struct VecIter {
        entries: Vec<(Vec<u8>, Vec<u8>)>,
        pos: usize,
        valid: bool,
        icmp: InternalKeyComparator,
    }
    impl VecIter {
        fn boxed(entries: Vec<(Vec<u8>, Vec<u8>)>) -> Box<dyn InternalIterator> {
            Box::new(VecIter {
                entries,
                pos: 0,
                valid: false,
                icmp: InternalKeyComparator::new(Arc::new(BytewiseComparator)),
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

    fn ik(user: &[u8]) -> Vec<u8> {
        let mut k = Vec::new();
        append_internal_key(&mut k, user, 1, ValueType::Value);
        k
    }

    fn make() -> ConcatenatingIterator {
        let a = VecIter::boxed(vec![(ik(b"a"), b"1".to_vec()), (ik(b"b"), b"2".to_vec())]);
        let b = VecIter::boxed(vec![(ik(b"m"), b"3".to_vec()), (ik(b"n"), b"4".to_vec())]);
        let c = VecIter::boxed(vec![(ik(b"x"), b"5".to_vec())]);
        ConcatenatingIterator::new(vec![a, b, c])
    }

    #[test]
    fn forward_chains_children() {
        let mut it = make();
        it.seek_to_first();
        let mut users = Vec::new();
        while it.valid() {
            users.push(extract_user_key(it.key()).to_vec());
            it.next();
        }
        assert_eq!(
            users,
            vec![b"a".to_vec(), b"b".to_vec(), b"m".to_vec(), b"n".to_vec(), b"x".to_vec()]
        );
    }

    #[test]
    fn seek_finds_correct_child() {
        let mut it = make();
        it.seek(&ik(b"c")); // between child A and child B -> lands on m
        assert!(it.valid());
        assert_eq!(extract_user_key(it.key()), b"m");
        it.seek(&ik(b"n"));
        assert_eq!(extract_user_key(it.key()), b"n");
        it.seek(&ik(b"zzz"));
        assert!(!it.valid());
    }

    #[test]
    fn reverse_chains_children() {
        let mut it = make();
        it.seek_to_last();
        let mut users = Vec::new();
        while it.valid() {
            users.push(extract_user_key(it.key()).to_vec());
            it.prev();
        }
        assert_eq!(
            users,
            vec![b"x".to_vec(), b"n".to_vec(), b"m".to_vec(), b"b".to_vec(), b"a".to_vec()]
        );
    }

    #[test]
    fn empty_children_skipped() {
        let a = VecIter::boxed(vec![]);
        let b = VecIter::boxed(vec![(ik(b"only"), b"1".to_vec())]);
        let c = VecIter::boxed(vec![]);
        let mut it = ConcatenatingIterator::new(vec![a, b, c]);
        it.seek_to_first();
        assert!(it.valid());
        assert_eq!(extract_user_key(it.key()), b"only");
        it.next();
        assert!(!it.valid());
    }
}
