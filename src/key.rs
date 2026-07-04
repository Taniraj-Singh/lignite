//! Internal key encoding and the MVCC ordering it induces.
//!
//! Every user mutation is stored under an *internal key*:
//!
//! ```text
//! internal_key := user_key || fixed64_le( (sequence << 8) | value_type )
//! ```
//!
//! The trailing eight bytes pack a 56-bit monotonically increasing sequence
//! number together with an 8-bit [`ValueType`] tag distinguishing a live value
//! from a deletion tombstone.
//!
//! Internal keys sort by:
//!   1. ascending user key (per the configured [`Comparator`]); then
//!   2. **descending** packed trailer, so that for a single user key the newest
//!      version (largest sequence) sorts first.
//!
//! That "newest first within a key" rule is the linchpin of MVCC reads: a lookup
//! walks versions of a key in decreasing sequence order and stops at the first
//! one whose sequence is visible to its snapshot.

use crate::comparator::Comparator;
use crate::util::coding::{decode_fixed64, encode_fixed64, put_varint32};
use std::cmp::Ordering;
use std::sync::Arc;

/// Sequence numbers are 56-bit; the top eight bits of the trailer hold the tag.
pub type SequenceNumber = u64;

/// The largest representable sequence number.
pub const MAX_SEQUENCE_NUMBER: SequenceNumber = (1 << 56) - 1;

/// Number of trailing bytes an internal key adds to a user key.
pub const INTERNAL_KEY_TRAILER_LEN: usize = 8;

/// Distinguishes the kind of a stored record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ValueType {
    /// A tombstone: the key is deleted as of its sequence number.
    Deletion = 0x0,
    /// A live value.
    Value = 0x1,
}

impl ValueType {
    fn from_u8(v: u8) -> Option<ValueType> {
        match v {
            0x0 => Some(ValueType::Deletion),
            0x1 => Some(ValueType::Value),
            _ => None,
        }
    }
}

/// The [`ValueType`] used when constructing a lookup key. Using the largest tag
/// value guarantees a lookup key sorts *before* every real record sharing its
/// user key and sequence number (because the trailer sorts descending).
pub const VALUE_TYPE_FOR_SEEK: ValueType = ValueType::Value;

/// Combine a sequence number and value type into the packed trailer.
#[inline]
pub fn pack_sequence_and_type(seq: SequenceNumber, t: ValueType) -> u64 {
    debug_assert!(seq <= MAX_SEQUENCE_NUMBER);
    (seq << 8) | (t as u64)
}

/// Split a packed trailer back into its sequence number and value type.
#[inline]
pub fn unpack_sequence_and_type(packed: u64) -> (SequenceNumber, ValueType) {
    let seq = packed >> 8;
    let t = ValueType::from_u8((packed & 0xff) as u8).unwrap_or(ValueType::Value);
    (seq, t)
}

/// A borrowed, decoded view of an internal key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedInternalKey<'a> {
    /// The user-visible key bytes.
    pub user_key: &'a [u8],
    /// The sequence number at which this record was written.
    pub sequence: SequenceNumber,
    /// Whether this record is a value or a tombstone.
    pub value_type: ValueType,
}

impl<'a> ParsedInternalKey<'a> {
    /// Decode an internal key. Returns `None` if it is shorter than the trailer.
    pub fn decode(internal_key: &'a [u8]) -> Option<ParsedInternalKey<'a>> {
        if internal_key.len() < INTERNAL_KEY_TRAILER_LEN {
            return None;
        }
        let split = internal_key.len() - INTERNAL_KEY_TRAILER_LEN;
        let packed = decode_fixed64(&internal_key[split..])?;
        let (sequence, value_type) = unpack_sequence_and_type(packed);
        Some(ParsedInternalKey {
            user_key: &internal_key[..split],
            sequence,
            value_type,
        })
    }
}

/// Append the internal-key encoding of `(user_key, seq, t)` to `dst`.
pub fn append_internal_key(dst: &mut Vec<u8>, user_key: &[u8], seq: SequenceNumber, t: ValueType) {
    dst.extend_from_slice(user_key);
    encode_fixed64(dst, pack_sequence_and_type(seq, t));
}

/// Borrow the user-key portion of an already-encoded internal key.
#[inline]
pub fn extract_user_key(internal_key: &[u8]) -> &[u8] {
    debug_assert!(internal_key.len() >= INTERNAL_KEY_TRAILER_LEN);
    &internal_key[..internal_key.len() - INTERNAL_KEY_TRAILER_LEN]
}

/// Return the packed trailer of an already-encoded internal key.
#[inline]
pub fn extract_trailer(internal_key: &[u8]) -> u64 {
    debug_assert!(internal_key.len() >= INTERNAL_KEY_TRAILER_LEN);
    decode_fixed64(&internal_key[internal_key.len() - INTERNAL_KEY_TRAILER_LEN..]).unwrap_or(0)
}

/// Orders internal keys given a user-key [`Comparator`].
///
/// This comparator is the authoritative on-disk ordering: it is what the SSTable
/// builder uses to lay out records and what point lookups rely on. Range scans
/// that merge several runs derive their per-run ordering from it as well.
#[derive(Debug, Clone)]
pub struct InternalKeyComparator {
    user: Arc<dyn Comparator>,
}

impl InternalKeyComparator {
    /// Wrap a user comparator.
    pub fn new(user: Arc<dyn Comparator>) -> Self {
        InternalKeyComparator { user }
    }

    /// Access the underlying user comparator.
    pub fn user_comparator(&self) -> &Arc<dyn Comparator> {
        &self.user
    }

    /// Compare two fully-encoded internal keys.
    ///
    /// User keys compare ascending; ties break on the packed trailer in
    /// *descending* order, so a larger sequence number (newer write) is ordered
    /// before a smaller one for the same user key.
    pub fn compare(&self, a: &[u8], b: &[u8]) -> Ordering {
        let ua = extract_user_key(a);
        let ub = extract_user_key(b);
        match self.user.compare(ua, ub) {
            Ordering::Equal => {
                let ta = extract_trailer(a);
                let tb = extract_trailer(b);
                // Descending on the trailer: larger trailer sorts first.
                tb.cmp(&ta)
            }
            other => other,
        }
    }

    /// A name for persistence, derived from the wrapped user comparator.
    pub fn name(&self) -> String {
        format!("lignite.InternalKeyComparator({})", self.user.name())
    }
}

/// A key used to probe the memtable and SSTables for a user key at a snapshot.
///
/// It materialises two overlapping views into one heap buffer:
///   * `memtable_key` — varint-length-prefixed internal key (what the memtable
///     stores);
///   * `internal_key` — the bare internal key (what the tables compare against).
pub struct LookupKey {
    /// `[ varint(len) | user_key | trailer ]`
    data: Vec<u8>,
    /// Offset in `data` where the internal key (user_key||trailer) begins.
    internal_offset: usize,
}

impl LookupKey {
    /// Build a lookup key for `user_key` at snapshot `seq`.
    pub fn new(user_key: &[u8], seq: SequenceNumber) -> Self {
        let internal_len = user_key.len() + INTERNAL_KEY_TRAILER_LEN;
        let mut data = Vec::with_capacity(internal_len + 5);
        put_varint32(&mut data, internal_len as u32);
        let internal_offset = data.len();
        data.extend_from_slice(user_key);
        encode_fixed64(
            &mut data,
            pack_sequence_and_type(seq, VALUE_TYPE_FOR_SEEK),
        );
        LookupKey {
            data,
            internal_offset,
        }
    }

    /// The length-prefixed form the memtable is keyed on.
    pub fn memtable_key(&self) -> &[u8] {
        &self.data
    }

    /// The bare internal key.
    pub fn internal_key(&self) -> &[u8] {
        &self.data[self.internal_offset..]
    }

    /// The user key.
    pub fn user_key(&self) -> &[u8] {
        &self.data[self.internal_offset..self.data.len() - INTERNAL_KEY_TRAILER_LEN]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;

    fn ikc() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    #[test]
    fn pack_unpack_roundtrip() {
        for &seq in &[0u64, 1, 42, MAX_SEQUENCE_NUMBER] {
            for &t in &[ValueType::Value, ValueType::Deletion] {
                let packed = pack_sequence_and_type(seq, t);
                let (s2, t2) = unpack_sequence_and_type(packed);
                assert_eq!(s2, seq);
                assert_eq!(t2, t);
            }
        }
    }

    #[test]
    fn parse_internal_key() {
        let mut buf = Vec::new();
        append_internal_key(&mut buf, b"key", 99, ValueType::Value);
        let p = ParsedInternalKey::decode(&buf).unwrap();
        assert_eq!(p.user_key, b"key");
        assert_eq!(p.sequence, 99);
        assert_eq!(p.value_type, ValueType::Value);
        assert_eq!(extract_user_key(&buf), b"key");
    }

    #[test]
    fn newer_sequence_sorts_first_for_same_key() {
        let c = ikc();
        let mut older = Vec::new();
        append_internal_key(&mut older, b"k", 1, ValueType::Value);
        let mut newer = Vec::new();
        append_internal_key(&mut newer, b"k", 2, ValueType::Value);
        // Newer (seq=2) must order strictly before older (seq=1).
        assert_eq!(c.compare(&newer, &older), Ordering::Less);
        assert_eq!(c.compare(&older, &newer), Ordering::Greater);
    }

    #[test]
    fn user_key_dominates_sequence() {
        let c = ikc();
        let mut a = Vec::new();
        append_internal_key(&mut a, b"a", 1, ValueType::Value);
        let mut b = Vec::new();
        append_internal_key(&mut b, b"b", 999_999, ValueType::Value);
        assert_eq!(c.compare(&a, &b), Ordering::Less);
    }

    #[test]
    fn lookup_key_views_are_consistent() {
        let lk = LookupKey::new(b"apple", 7);
        assert_eq!(lk.user_key(), b"apple");
        let parsed = ParsedInternalKey::decode(lk.internal_key()).unwrap();
        assert_eq!(parsed.user_key, b"apple");
        assert_eq!(parsed.sequence, 7);
        // The memtable key is the internal key with a varint length prefix.
        assert!(lk.memtable_key().len() > lk.internal_key().len());
    }
}
