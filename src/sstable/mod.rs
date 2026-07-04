//! Sorted String Tables (SSTables): the immutable, on-disk representation of a
//! sorted run of internal key/value records.
//!
//! A table is written once by [`TableBuilder`] — from a flush of a memtable or
//! the output of a compaction — and thereafter only read. Internally it is a
//! stack of blocks (see [`format`]): delta-encoded [`block`]s of records, a
//! [`filter_block`] of per-block bloom filters, a metaindex, an index mapping
//! each block's maximum key to its location, and a fixed footer.

pub mod block;
pub mod block_builder;
pub mod filter_block;
pub mod format;
pub mod table;
pub mod table_builder;

pub use block::{Block, BlockIter};
pub use format::{BlockHandle, Footer};
pub use table::{read_block, Table, TableIter};
pub use table_builder::{TableBuildOptions, TableBuilder};
