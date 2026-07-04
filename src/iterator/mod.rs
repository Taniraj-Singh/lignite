//! Iterator infrastructure.
//!
//! Every ordered source in the engine — a memtable, a single SSTable, a level of
//! SSTables, or the union of all of them — exposes the same cursor interface,
//! [`InternalIterator`], yielding *internal keys* (user key plus packed
//! sequence/type trailer) in ascending internal-key order. Composability is the
//! whole point: an [`merging::MergingIterator`] takes a vector of child
//! iterators and presents their k-way merge as just another `InternalIterator`.
//!
//! ## Positioning contract
//!
//! An iterator is either *valid* (positioned at a live entry, [`key`] and
//! [`value`] readable) or invalid (exhausted or never positioned). Every
//! positioning call establishes one or the other. Concretely:
//!
//! * [`seek_to_first`] / [`seek_to_last`] land on the extreme entries.
//! * [`seek`] lands on the first entry whose internal key is `>=` the target.
//! * [`next`] / [`prev`] advance; stepping past an end invalidates the cursor.
//!
//! Implementations copy the current entry into owned buffers on each positioning
//! call, so [`key`] and [`value`] borrow from the iterator itself and remain
//! valid until the next positioning call. This keeps the trait object-safe and
//! free of borrow entanglements at the cost of a copy per step.
//!
//! [`key`]: InternalIterator::key
//! [`value`]: InternalIterator::value
//! [`seek_to_first`]: InternalIterator::seek_to_first
//! [`seek_to_last`]: InternalIterator::seek_to_last
//! [`seek`]: InternalIterator::seek
//! [`next`]: InternalIterator::next
//! [`prev`]: InternalIterator::prev

pub mod concat;
pub mod empty;
pub mod merging;

pub use concat::ConcatenatingIterator;
pub use empty::EmptyIterator;

use crate::error::Result;

/// A bidirectional cursor over a sorted run of internal key/value pairs.
pub trait InternalIterator {
    /// Whether the cursor currently points at a valid entry.
    fn valid(&self) -> bool;

    /// Position at the first (smallest internal key) entry.
    fn seek_to_first(&mut self);

    /// Position at the last (largest internal key) entry.
    fn seek_to_last(&mut self);

    /// Position at the first entry whose internal key is `>= target`.
    ///
    /// `target` is a fully encoded internal key.
    fn seek(&mut self, target: &[u8]);

    /// Advance to the next entry, invalidating the cursor if none remains.
    fn next(&mut self);

    /// Retreat to the previous entry, invalidating the cursor if none remains.
    fn prev(&mut self);

    /// The current entry's internal key. Only meaningful while [`valid`].
    ///
    /// [`valid`]: InternalIterator::valid
    fn key(&self) -> &[u8];

    /// The current entry's value. Only meaningful while [`valid`].
    ///
    /// [`valid`]: InternalIterator::valid
    fn value(&self) -> &[u8];

    /// A deferred error observed during iteration, if any. A well-behaved
    /// iterator becomes invalid when it hits an error and surfaces it here.
    fn status(&self) -> Result<()> {
        Ok(())
    }
}

/// The public, user-facing iterator over *user* keys and values at a snapshot.
///
/// Unlike [`InternalIterator`], this hides the MVCC machinery: it collapses the
/// version chain of each user key down to the single version visible at the
/// read's snapshot, skips deleted keys, and yields plain user key/value pairs.
pub trait LigniteIterator {
    /// Whether the cursor points at a live, visible user entry.
    fn valid(&self) -> bool;
    /// Seek to the first visible user key `>= target`.
    fn seek(&mut self, target: &[u8]);
    /// Seek to the first visible user key.
    fn seek_to_first(&mut self);
    /// Advance to the next visible user key.
    fn next(&mut self);
    /// The current user key.
    fn key(&self) -> &[u8];
    /// The current value.
    fn value(&self) -> &[u8];
    /// Any deferred error.
    fn status(&self) -> Result<()>;
}
