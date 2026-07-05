//! # lignite
//!
//! `lignite` is an embeddable, single-node key/value storage engine built around a
//! log-structured merge-tree (LSM). Its architecture follows the LevelDB/RocksDB
//! family: writes land first in a write-ahead log and an in-memory memtable, are
//! flushed into immutable sorted string tables once the memtable fills, and are
//! periodically reorganised by background compaction into a layered on-disk
//! structure managed by a version set.
//!
//! Reads are served through a multi-version concurrency-control (MVCC) model:
//! every mutation is tagged with a monotonically increasing sequence number, and a
//! read observes only the mutations visible at (or before) the sequence number
//! captured by its snapshot. Point lookups and range scans are resolved by merging
//! any number of sorted runs through the merging iterator.

#![forbid(unsafe_code)]
#![allow(clippy::module_inception)]
#![allow(clippy::new_without_default)]

pub mod archive;
pub mod batch;
pub mod cache;
pub mod comparator;
pub mod compaction;
pub mod compress;
pub mod db;
pub mod env;
pub mod error;
pub mod filter;
pub mod iterator;
pub mod key;
pub mod memtable;
pub mod options;
pub mod snapshot;
pub mod sstable;
pub mod txn;
pub mod util;
pub mod version;
pub mod wal;

pub use batch::WriteBatch;
pub use comparator::{BytewiseComparator, Comparator};
pub use db::Db;
pub use error::{Error, Result};
pub use iterator::{InternalIterator, LigniteIterator};
pub use options::{CompressionType, Options, ReadOptions, WriteOptions};
pub use snapshot::Snapshot;
pub use txn::Transaction;
