//! A safe, index-based skiplist.
//!
//! Classic skiplist implementations thread raw pointers between heap nodes. To
//! stay within `#![forbid(unsafe_code)]` this variant stores every node in a
//! single `Vec` and links them by index. Index 0 is a permanent head sentinel;
//! a `next` pointer of 0 marks the end of a level's chain, and a cursor value of
//! 0 denotes an invalid position. Because the node vector only ever grows,
//! indices are stable for the lifetime of the list, so a cursor captured by an
//! iterator never dangles even as new nodes are appended.
//!
//! Entries are opaque byte strings ordered by the internal key embedded in their
//! length-prefixed header (see [`crate::memtable::MemTable::add`]). Duplicate
//! internal keys are permitted; the memtable never produces them because every
//! record carries a distinct sequence number.

use crate::key::InternalKeyComparator;
use crate::util::coding::take_length_prefixed_slice;
use crate::util::random::Random;
use std::cmp::Ordering;

const MAX_HEIGHT: usize = 12;
const BRANCHING: u32 = 4;
/// Sentinel used both for the head node index and the nil `next` pointer.
const NIL: usize = 0;

struct Node {
    entry: Vec<u8>,
    next: Vec<usize>,
}

/// An ordered multiset of byte-string entries with O(log n) search and insert.
pub struct SkipList {
    nodes: Vec<Node>,
    icmp: InternalKeyComparator,
    max_height: usize,
    rnd: Random,
    count: usize,
}

impl SkipList {
    /// Create an empty skiplist ordered by `icmp`.
    pub fn new(icmp: InternalKeyComparator) -> Self {
        let head = Node {
            entry: Vec::new(),
            next: vec![NIL; MAX_HEIGHT],
        };
        SkipList {
            nodes: vec![head],
            icmp,
            max_height: 1,
            rnd: Random::new(0xdead_beef),
            count: 0,
        }
    }

    /// Number of entries (excluding the head sentinel).
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The raw stored entry at `node`.
    pub fn entry_at(&self, node: usize) -> &[u8] {
        &self.nodes[node].entry
    }

    /// The internal key embedded in `node`'s entry.
    fn internal_key_of(&self, node: usize) -> &[u8] {
        take_length_prefixed_slice(&self.nodes[node].entry)
            .map(|(ik, _)| ik)
            .unwrap_or(&[])
    }

    /// True when `node` exists and its key sorts strictly before `target_ik`.
    fn key_is_after_node(&self, target_ik: &[u8], node: usize) -> bool {
        node != NIL && self.icmp.compare(self.internal_key_of(node), target_ik) == Ordering::Less
    }

    /// Find the first node whose key is `>= target_ik`, recording the immediate
    /// predecessor at each level into `prev` (used by insert).
    fn find_greater_or_equal(&self, target_ik: &[u8], prev: &mut [usize; MAX_HEIGHT]) -> usize {
        let mut x = NIL; // head
        let mut level = self.max_height - 1;
        loop {
            let next = self.nodes[x].next[level];
            if self.key_is_after_node(target_ik, next) {
                x = next;
            } else {
                prev[level] = x;
                if level == 0 {
                    return next;
                }
                level -= 1;
            }
        }
    }

    /// Find the last node whose key sorts strictly before `target_ik`, or the
    /// head sentinel (0) when none does.
    fn find_less_than(&self, target_ik: &[u8]) -> usize {
        let mut x = NIL;
        let mut level = self.max_height - 1;
        loop {
            let next = self.nodes[x].next[level];
            let next_before_target = next != NIL
                && self.icmp.compare(self.internal_key_of(next), target_ik) == Ordering::Less;
            if next_before_target {
                x = next;
            } else if level == 0 {
                return x;
            } else {
                level -= 1;
            }
        }
    }

    /// Find the last node in the list, or 0 when empty.
    fn find_last(&self) -> usize {
        let mut x = NIL;
        let mut level = self.max_height - 1;
        loop {
            let next = self.nodes[x].next[level];
            if next == NIL {
                if level == 0 {
                    return x;
                }
                level -= 1;
            } else {
                x = next;
            }
        }
    }

    fn random_height(&mut self) -> usize {
        let mut height = 1;
        while height < MAX_HEIGHT && self.rnd.one_in(BRANCHING) {
            height += 1;
        }
        height
    }

    /// Insert `entry`, ordered by its embedded internal key.
    pub fn insert(&mut self, entry: Vec<u8>) {
        let mut prev = [NIL; MAX_HEIGHT];
        {
            let ik = take_length_prefixed_slice(&entry)
                .map(|(ik, _)| ik.to_vec())
                .unwrap_or_default();
            self.find_greater_or_equal(&ik, &mut prev);
        }

        let height = self.random_height();
        if height > self.max_height {
            for slot in prev.iter_mut().take(height).skip(self.max_height) {
                *slot = NIL; // link new upper levels off the head
            }
            self.max_height = height;
        }

        let new_index = self.nodes.len();
        let mut next = Vec::with_capacity(height);
        for (level, &p) in prev.iter().enumerate().take(height) {
            next.push(self.nodes[p].next[level]);
        }
        self.nodes.push(Node { entry, next });
        for (level, &p) in prev.iter().enumerate().take(height) {
            self.nodes[p].next[level] = new_index;
        }
        self.count += 1;
    }

    /// Locate the first node whose key is `>= memtable_key`'s internal key.
    pub fn seek_ge(&self, memtable_key: &[u8]) -> usize {
        let target = take_length_prefixed_slice(memtable_key)
            .map(|(ik, _)| ik)
            .unwrap_or(memtable_key);
        let mut prev = [NIL; MAX_HEIGHT];
        self.find_greater_or_equal(target, &mut prev)
    }

    /// A fresh cursor positioned before the first entry.
    pub fn iter(&self) -> SkipListIterator {
        SkipListIterator { node: NIL }
    }

    fn next_of(&self, node: usize) -> usize {
        self.nodes[node].next[0]
    }

    fn first(&self) -> usize {
        self.nodes[NIL].next[0]
    }
}

/// A cursor into a [`SkipList`]. It stores only a node index; every navigation
/// call is handed the list so the cursor holds no borrow and can live inside an
/// `Arc`-owning iterator.
pub struct SkipListIterator {
    node: usize,
}

impl SkipListIterator {
    /// Whether the cursor is at a real node.
    pub fn valid(&self) -> bool {
        self.node != NIL
    }

    /// The current node index (meaningful only while [`valid`]).
    ///
    /// [`valid`]: SkipListIterator::valid
    pub fn node(&self) -> usize {
        self.node
    }

    /// Position at the first node.
    pub fn seek_to_first(&mut self, list: &SkipList) {
        self.node = list.first();
    }

    /// Position at the last node.
    pub fn seek_to_last(&mut self, list: &SkipList) {
        self.node = list.find_last();
    }

    /// Position at the first node whose internal key is `>= memtable_key`.
    pub fn seek(&mut self, list: &SkipList, memtable_key: &[u8]) {
        self.node = list.seek_ge(memtable_key);
    }

    /// Advance to the next node.
    pub fn next(&mut self, list: &SkipList) {
        if self.node != NIL {
            self.node = list.next_of(self.node);
        }
    }

    /// Retreat to the previous node, invalidating at the head.
    pub fn prev(&mut self, list: &SkipList) {
        if self.node == NIL {
            return;
        }
        let ik = list.internal_key_of(self.node).to_vec();
        self.node = list.find_less_than(&ik);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::{append_internal_key, ValueType};
    use crate::util::coding::put_varint32;
    use std::sync::Arc;

    fn icmp() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    fn entry(user: &[u8], seq: u64, value: &[u8]) -> Vec<u8> {
        let mut ik = Vec::new();
        append_internal_key(&mut ik, user, seq, ValueType::Value);
        let mut e = Vec::new();
        put_varint32(&mut e, ik.len() as u32);
        e.extend_from_slice(&ik);
        put_varint32(&mut e, value.len() as u32);
        e.extend_from_slice(value);
        e
    }

    fn user_of(list: &SkipList, node: usize) -> Vec<u8> {
        crate::key::extract_user_key(list.internal_key_of(node)).to_vec()
    }

    #[test]
    fn inserts_stay_sorted() {
        let mut l = SkipList::new(icmp());
        for &k in &[b"d", b"a", b"c", b"b"] {
            l.insert(entry(k, 1, b"v"));
        }
        assert_eq!(l.len(), 4);
        let mut it = l.iter();
        it.seek_to_first(&l);
        let mut seen = Vec::new();
        while it.valid() {
            seen.push(user_of(&l, it.node()));
            it.next(&l);
        }
        assert_eq!(seen, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]);
    }

    #[test]
    fn seek_finds_ge() {
        let mut l = SkipList::new(icmp());
        for &k in &[b"a", b"c", b"e"] {
            l.insert(entry(k, 1, b"v"));
        }
        let mut mkey = Vec::new();
        {
            let mut ik = Vec::new();
            append_internal_key(&mut ik, b"b", crate::key::MAX_SEQUENCE_NUMBER, ValueType::Value);
            put_varint32(&mut mkey, ik.len() as u32);
            mkey.extend_from_slice(&ik);
        }
        let mut it = l.iter();
        it.seek(&l, &mkey);
        assert!(it.valid());
        assert_eq!(user_of(&l, it.node()), b"c");
    }

    #[test]
    fn reverse_iteration() {
        let mut l = SkipList::new(icmp());
        for &k in &[b"a", b"b", b"c"] {
            l.insert(entry(k, 1, b"v"));
        }
        let mut it = l.iter();
        it.seek_to_last(&l);
        let mut seen = Vec::new();
        while it.valid() {
            seen.push(user_of(&l, it.node()));
            it.prev(&l);
        }
        assert_eq!(seen, vec![b"c".to_vec(), b"b".to_vec(), b"a".to_vec()]);
    }

    #[test]
    fn many_inserts_are_ordered() {
        let mut l = SkipList::new(icmp());
        let mut r = Random::new(1);
        let n = 2000u32;
        for _ in 0..n {
            let k = r.uniform(500);
            l.insert(entry(format!("{k:04}").as_bytes(), 1, b"v"));
        }
        assert_eq!(l.len() as u32, n);
        let mut it = l.iter();
        it.seek_to_first(&l);
        let mut last: Option<Vec<u8>> = None;
        let mut counted = 0;
        while it.valid() {
            let cur = user_of(&l, it.node());
            if let Some(prev) = &last {
                assert!(prev.as_slice() <= cur.as_slice());
            }
            last = Some(cur);
            counted += 1;
            it.next(&l);
        }
        assert_eq!(counted, n);
    }
}
