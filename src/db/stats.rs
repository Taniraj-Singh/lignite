//! Lightweight runtime counters.
//!
//! The database maintains a handful of atomic counters so callers can observe
//! how much work it is doing — useful for tests, tuning and dashboards. The
//! counters are updated on the hot path with relaxed atomics, so reading them is
//! approximate under concurrency but never blocks a writer or reader.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Shared, atomically-updated engine counters.
#[derive(Default)]
pub struct Stats {
    writes: AtomicU64,
    write_bytes: AtomicU64,
    reads: AtomicU64,
    read_hits: AtomicU64,
    flushes: AtomicU64,
    compactions: AtomicU64,
    files_created: AtomicU64,
}

/// An immutable snapshot of [`Stats`] at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsSnapshot {
    /// Number of `write` calls applied.
    pub writes: u64,
    /// Total bytes of batch payload logged.
    pub write_bytes: u64,
    /// Number of `get` calls served.
    pub reads: u64,
    /// Reads that found a live value.
    pub read_hits: u64,
    /// Memtable flushes performed.
    pub flushes: u64,
    /// Compactions performed.
    pub compactions: u64,
    /// SSTable files created (by flush or compaction).
    pub files_created: u64,
}

impl Stats {
    /// Create a fresh, shareable counter set.
    pub fn new() -> Arc<Stats> {
        Arc::new(Stats::default())
    }

    pub(crate) fn record_write(&self, bytes: usize) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.write_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn record_read(&self, hit: bool) {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if hit {
            self.read_hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_flush(&self, files_created: u64) {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        self.files_created.fetch_add(files_created, Ordering::Relaxed);
    }

    pub(crate) fn record_compaction(&self, files_created: u64) {
        self.compactions.fetch_add(1, Ordering::Relaxed);
        self.files_created.fetch_add(files_created, Ordering::Relaxed);
    }

    /// Read all counters into a snapshot.
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            writes: self.writes.load(Ordering::Relaxed),
            write_bytes: self.write_bytes.load(Ordering::Relaxed),
            reads: self.reads.load(Ordering::Relaxed),
            read_hits: self.read_hits.load(Ordering::Relaxed),
            flushes: self.flushes.load(Ordering::Relaxed),
            compactions: self.compactions.load(Ordering::Relaxed),
            files_created: self.files_created.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate() {
        let s = Stats::new();
        s.record_write(10);
        s.record_write(20);
        s.record_read(true);
        s.record_read(false);
        s.record_flush(1);
        s.record_compaction(1);
        let snap = s.snapshot();
        assert_eq!(snap.writes, 2);
        assert_eq!(snap.write_bytes, 30);
        assert_eq!(snap.reads, 2);
        assert_eq!(snap.read_hits, 1);
        assert_eq!(snap.flushes, 1);
        assert_eq!(snap.compactions, 1);
        assert_eq!(snap.files_created, 2);
    }
}
