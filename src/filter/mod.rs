//! Filter policies.
//!
//! A [`FilterPolicy`] summarises the set of keys in an SSTable data block into a
//! compact bit array that can answer "is this key *definitely absent*?" without
//! touching the block. A negative answer lets a point lookup skip an I/O; a
//! positive answer may be a false positive, so the block is still consulted.
//! Filters trade a few bits per key for far fewer disk reads on missing keys.

pub mod bloom;

pub use bloom::BloomFilterPolicy;

/// Strategy for building and querying per-block key filters.
pub trait FilterPolicy: Send + Sync + std::fmt::Debug {
    /// A stable name persisted alongside the filter data.
    fn name(&self) -> &'static str;

    /// Build a filter summarising `keys` (each a user key) and append its bytes
    /// to `dst`.
    fn create_filter(&self, keys: &[&[u8]], dst: &mut Vec<u8>);

    /// Return `false` only if `key` is guaranteed absent from the set `filter`
    /// was built over. A `true` result may be a false positive.
    fn key_may_match(&self, key: &[u8], filter: &[u8]) -> bool;
}
