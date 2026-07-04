//! Read snapshots.
//!
//! A [`Snapshot`] pins a sequence number. A read carrying a snapshot observes
//! exactly the state produced by writes with sequence numbers `<=` the pinned
//! value, regardless of concurrent writes. The database keeps the set of live
//! snapshots so that compaction never drops a version some snapshot can still
//! observe.

use crate::key::SequenceNumber;
use std::sync::{Arc, Mutex};

/// A handle to a point-in-time view of the database.
///
/// Cloning a snapshot is cheap and shares the same underlying registration.
/// The pinned sequence number is released when the last clone is dropped.
#[derive(Clone)]
pub struct Snapshot {
    inner: Arc<SnapshotInner>,
}

struct SnapshotInner {
    sequence: SequenceNumber,
    registry: Option<Arc<SnapshotRegistry>>,
}

impl Snapshot {
    /// The sequence number this snapshot observes at.
    pub fn sequence(&self) -> SequenceNumber {
        self.inner.sequence
    }
}

impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snapshot")
            .field("sequence", &self.inner.sequence)
            .finish()
    }
}

impl Drop for SnapshotInner {
    fn drop(&mut self) {
        if let Some(registry) = &self.registry {
            registry.release(self.sequence);
        }
    }
}

/// Tracks the multiset of sequence numbers pinned by live snapshots.
///
/// The engine consults [`SnapshotRegistry::oldest`] during compaction: any
/// version whose sequence number is at or above the oldest live snapshot must be
/// preserved, and duplicate versions of a key straddling that boundary cannot be
/// collapsed.
pub struct SnapshotRegistry {
    // Ordered multiset keyed by sequence number -> reference count.
    live: Mutex<std::collections::BTreeMap<SequenceNumber, usize>>,
}

impl SnapshotRegistry {
    /// Create an empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(SnapshotRegistry {
            live: Mutex::new(std::collections::BTreeMap::new()),
        })
    }

    /// Register a snapshot at `sequence` and return a handle.
    pub fn snapshot_at(self: &Arc<Self>, sequence: SequenceNumber) -> Snapshot {
        {
            let mut live = self.live.lock().unwrap();
            *live.entry(sequence).or_insert(0) += 1;
        }
        Snapshot {
            inner: Arc::new(SnapshotInner {
                sequence,
                registry: Some(Arc::clone(self)),
            }),
        }
    }

    /// The oldest sequence number still pinned by any live snapshot, if any.
    pub fn oldest(&self) -> Option<SequenceNumber> {
        let live = self.live.lock().unwrap();
        live.keys().next().copied()
    }

    /// Number of distinct pinned sequence numbers.
    pub fn len(&self) -> usize {
        self.live.lock().unwrap().len()
    }

    /// True if no snapshots are live.
    pub fn is_empty(&self) -> bool {
        self.live.lock().unwrap().is_empty()
    }

    fn release(&self, sequence: SequenceNumber) {
        let mut live = self.live.lock().unwrap();
        if let Some(count) = live.get_mut(&sequence) {
            *count -= 1;
            if *count == 0 {
                live.remove(&sequence);
            }
        }
    }
}

/// Construct a detached snapshot not tied to any registry (used in tests and by
/// reads that synthesise an implicit snapshot at the latest sequence number).
pub fn detached(sequence: SequenceNumber) -> Snapshot {
    Snapshot {
        inner: Arc::new(SnapshotInner {
            sequence,
            registry: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oldest_tracks_live_set() {
        let reg = SnapshotRegistry::new();
        assert!(reg.oldest().is_none());
        let s1 = reg.snapshot_at(10);
        let s2 = reg.snapshot_at(20);
        assert_eq!(reg.oldest(), Some(10));
        drop(s1);
        assert_eq!(reg.oldest(), Some(20));
        drop(s2);
        assert!(reg.is_empty());
    }

    #[test]
    fn duplicate_sequences_refcount() {
        let reg = SnapshotRegistry::new();
        let a = reg.snapshot_at(5);
        let b = reg.snapshot_at(5);
        assert_eq!(reg.len(), 1);
        drop(a);
        assert_eq!(reg.oldest(), Some(5));
        drop(b);
        assert!(reg.is_empty());
    }

    #[test]
    fn detached_has_no_registry() {
        let s = detached(99);
        assert_eq!(s.sequence(), 99);
    }
}
