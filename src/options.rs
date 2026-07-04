//! Tunables controlling durability, memory use and file layout.

use crate::comparator::{BytewiseComparator, Comparator};
use std::sync::Arc;

/// Block compression algorithm recorded per SSTable block.
///
/// Only [`CompressionType::None`] is wired through the current builder; the other
/// variants exist so the on-disk format reserves their tag bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompressionType {
    /// Store blocks verbatim.
    None = 0x0,
    /// Snappy (reserved).
    Snappy = 0x1,
    /// Zstd (reserved).
    Zstd = 0x2,
}

impl CompressionType {
    pub(crate) fn from_u8(v: u8) -> Option<CompressionType> {
        match v {
            0x0 => Some(CompressionType::None),
            0x1 => Some(CompressionType::Snappy),
            0x2 => Some(CompressionType::Zstd),
            _ => None,
        }
    }
}

/// Database-wide configuration, supplied once at [`crate::Db::open`].
#[derive(Clone)]
pub struct Options {
    /// Create the database if it does not already exist.
    pub create_if_missing: bool,
    /// Fail to open if the database already exists.
    pub error_if_exists: bool,
    /// Verify block checksums on every read (costs CPU, catches bit-rot early).
    pub paranoid_checks: bool,
    /// Ordering over user keys.
    pub comparator: Arc<dyn Comparator>,
    /// Size in bytes at which the active memtable is frozen and flushed.
    pub write_buffer_size: usize,
    /// Approximate uncompressed size of a data block within an SSTable.
    pub block_size: usize,
    /// Restart-point interval for delta-encoded keys within a block.
    pub block_restart_interval: usize,
    /// Bits of bloom filter per key, or `0` to disable filters.
    pub bloom_bits_per_key: usize,
    /// Capacity, in bytes, of the shared block cache.
    pub block_cache_capacity: usize,
    /// Maximum number of files the engine keeps open at once.
    pub max_open_files: usize,
    /// Target size of a single SSTable produced by a flush or compaction.
    pub max_file_size: usize,
    /// Block compression algorithm.
    pub compression: CompressionType,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("create_if_missing", &self.create_if_missing)
            .field("error_if_exists", &self.error_if_exists)
            .field("paranoid_checks", &self.paranoid_checks)
            .field("comparator", &self.comparator.name())
            .field("write_buffer_size", &self.write_buffer_size)
            .field("block_size", &self.block_size)
            .field("block_restart_interval", &self.block_restart_interval)
            .field("bloom_bits_per_key", &self.bloom_bits_per_key)
            .field("block_cache_capacity", &self.block_cache_capacity)
            .field("max_open_files", &self.max_open_files)
            .field("max_file_size", &self.max_file_size)
            .field("compression", &self.compression)
            .finish()
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            create_if_missing: true,
            error_if_exists: false,
            paranoid_checks: false,
            comparator: Arc::new(BytewiseComparator),
            write_buffer_size: 4 << 20,   // 4 MiB
            block_size: 4 << 10,          // 4 KiB
            block_restart_interval: 16,
            bloom_bits_per_key: 10,
            block_cache_capacity: 8 << 20, // 8 MiB
            max_open_files: 1000,
            max_file_size: 2 << 20, // 2 MiB
            compression: CompressionType::None,
        }
    }
}

/// Per-read configuration.
#[derive(Clone)]
pub struct ReadOptions {
    /// Verify checksums of blocks touched by this read.
    pub verify_checksums: bool,
    /// Populate the block cache with blocks read (disable for full scans that
    /// would otherwise evict hot data).
    pub fill_cache: bool,
    /// Read as of a captured snapshot rather than the latest sequence number.
    pub snapshot: Option<crate::snapshot::Snapshot>,
}

impl Default for ReadOptions {
    fn default() -> Self {
        ReadOptions {
            verify_checksums: false,
            fill_cache: true,
            snapshot: None,
        }
    }
}

/// Per-write configuration.
#[derive(Clone, Debug)]
pub struct WriteOptions {
    /// Flush the write-ahead log to the operating system and request an fsync
    /// before the write is acknowledged. Durable but slow.
    pub sync: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        WriteOptions { sync: false }
    }
}
