//! Key comparators.
//!
//! A [`Comparator`] defines the total order over *user* keys. The engine is
//! generic over this so callers can impose a domain-specific ordering, but the
//! default — and the only one wired into the public `Db` — is
//! [`BytewiseComparator`], which orders keys by unsigned lexicographic byte
//! comparison.
//!
//! The comparator also provides two optional "key shortening" hooks used by the
//! SSTable builder to keep index entries small. They are permitted to return any
//! key that sorts in the required gap; returning the input unchanged is always
//! correct, just less space-efficient.

use std::cmp::Ordering;

/// Defines a total order over byte-string keys.
pub trait Comparator: Send + Sync + std::fmt::Debug {
    /// Compare two user keys.
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering;

    /// A stable name for the ordering. Persisted in table metadata so that a
    /// database opened with a different comparator can be rejected.
    fn name(&self) -> &'static str;

    /// If `*start < limit`, replace `start` with a short key in
    /// `[start, limit)`. Used between the last key of one data block and the
    /// first key of the next.
    fn find_shortest_separator(&self, start: &mut Vec<u8>, limit: &[u8]);

    /// Replace `key` with a short key `>= key`. Used for the final block.
    fn find_short_successor(&self, key: &mut Vec<u8>);
}

/// Lexicographic ordering over raw bytes (unsigned).
#[derive(Debug, Default, Clone, Copy)]
pub struct BytewiseComparator;

impl Comparator for BytewiseComparator {
    #[inline]
    fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        a.cmp(b)
    }

    fn name(&self) -> &'static str {
        "lignite.BytewiseComparator"
    }

    fn find_shortest_separator(&self, start: &mut Vec<u8>, limit: &[u8]) {
        // Length of the shared prefix.
        let min_len = start.len().min(limit.len());
        let mut diff = 0;
        while diff < min_len && start[diff] == limit[diff] {
            diff += 1;
        }
        if diff >= min_len {
            // One is a prefix of the other; no separator shorter than `start`.
            return;
        }
        let byte = start[diff];
        if byte < 0xff && byte + 1 < limit[diff] {
            start[diff] += 1;
            start.truncate(diff + 1);
            debug_assert!(self.compare(start, limit) == Ordering::Less);
        }
    }

    fn find_short_successor(&self, key: &mut Vec<u8>) {
        // Find the first byte that can be incremented.
        for i in 0..key.len() {
            if key[i] != 0xff {
                key[i] += 1;
                key.truncate(i + 1);
                return;
            }
        }
        // `key` is all 0xff bytes; it has no short successor, leave as-is.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytewise_orders_unsigned() {
        let c = BytewiseComparator;
        assert_eq!(c.compare(b"a", b"b"), Ordering::Less);
        assert_eq!(c.compare(&[0xff], &[0x00]), Ordering::Greater);
        assert_eq!(c.compare(b"abc", b"abc"), Ordering::Equal);
        assert_eq!(c.compare(b"ab", b"abc"), Ordering::Less);
    }

    #[test]
    fn shortest_separator_between() {
        let c = BytewiseComparator;
        let mut s = b"helloworld".to_vec();
        c.find_shortest_separator(&mut s, b"hellozebra");
        // Must remain in [start, limit).
        assert!(c.compare(&s, b"helloworld") != Ordering::Less);
        assert!(c.compare(&s, b"hellozebra") == Ordering::Less);
        assert!(s.len() <= "helloworld".len());
    }

    #[test]
    fn short_successor_ge() {
        let c = BytewiseComparator;
        let mut k = b"abc".to_vec();
        c.find_short_successor(&mut k);
        assert!(c.compare(&k, b"abc") != Ordering::Less);
        assert!(k.len() <= 3);
    }

    #[test]
    fn short_successor_all_ff() {
        let c = BytewiseComparator;
        let mut k = vec![0xff, 0xff];
        c.find_short_successor(&mut k);
        assert_eq!(k, vec![0xff, 0xff]);
    }
}
