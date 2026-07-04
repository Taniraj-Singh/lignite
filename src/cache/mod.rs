//! A sharded, capacity-bounded least-recently-used cache.
//!
//! The engine uses it as a *table cache*: parsed [`Table`](crate::sstable::Table)
//! handles are expensive to reconstruct (they re-read and validate index and
//! filter blocks), so recently-touched tables are retained and evicted in LRU
//! order once the configured charge budget is exceeded. The cache is generic, so
//! it can equally back a block cache keyed by `(file, offset)`.
//!
//! Concurrency is handled by sharding: keys are hashed to one of several
//! independently-locked shards, so unrelated lookups rarely contend.

pub mod lru;

pub use lru::ShardedCache;
