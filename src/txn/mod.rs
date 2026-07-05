//! Optimistic transactions layered over the engine's snapshots and write batches.
//!
//! A [`Transaction`] gives a caller a private, read-your-writes staging area on
//! top of a consistent read snapshot, and validates at commit time that nothing
//! it *read* changed underneath it before publishing everything it *wrote*. This
//! is *optimistic* concurrency control: transactions never take locks and never
//! block each other; instead a conflict is detected at commit and the loser is
//! asked to retry.
//!
//! ## Model
//!
//! On [`Transaction::begin`] the transaction captures a snapshot at the current
//! sequence number. Every [`Transaction::get`] first consults the local write
//! buffer (so a transaction sees its own uncommitted mutations) and otherwise
//! reads the database as of that snapshot, recording the observed value in a
//! *read set*. [`Transaction::put`] and [`Transaction::delete`] only mutate the
//! local buffer. [`Transaction::commit`] re-reads every key in the read set at the
//! latest sequence number; if any differs from what the transaction saw, the
//! commit fails with [`Error::InvalidArgument`] (a write–write / read–write
//! conflict) and no mutation is applied. Otherwise the buffered mutations are
//! written as a single atomic batch.
//!
//! The validation is precise at key granularity: two transactions that touch
//! disjoint key sets always both commit, and a transaction that only reads always
//! commits.

use std::collections::BTreeMap;

use crate::batch::WriteBatch;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::options::{ReadOptions, WriteOptions};
use crate::snapshot::Snapshot;

/// A staged mutation held in a transaction's private buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Staged {
    Put(Vec<u8>),
    Delete,
}

/// An in-flight optimistic transaction against a [`Db`].
///
/// The transaction borrows the database for its lifetime. Dropping it without
/// calling [`commit`](Self::commit) discards all staged writes.
pub struct Transaction<'a> {
    db: &'a Db,
    snapshot: Snapshot,
    writes: BTreeMap<Vec<u8>, Staged>,
    // Read set: key -> value observed at the snapshot (None = absent/deleted).
    reads: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl<'a> Transaction<'a> {
    /// Begin a transaction reading as of the current sequence number.
    pub fn begin(db: &'a Db) -> Self {
        Transaction {
            db,
            snapshot: db.snapshot(),
            writes: BTreeMap::new(),
            reads: BTreeMap::new(),
        }
    }

    /// The sequence number this transaction reads at.
    pub fn read_sequence(&self) -> u64 {
        self.snapshot.sequence()
    }

    /// Number of keys staged for writing.
    pub fn pending_writes(&self) -> usize {
        self.writes.len()
    }

    /// Read `key` within the transaction.
    ///
    /// Returns the transaction's own staged value if it has written `key`,
    /// otherwise the committed value visible at the transaction's snapshot. The
    /// observed value is recorded so commit can detect a conflicting change.
    pub fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(staged) = self.writes.get(key) {
            return Ok(match staged {
                Staged::Put(v) => Some(v.clone()),
                Staged::Delete => None,
            });
        }
        let ro = ReadOptions {
            snapshot: Some(self.snapshot.clone()),
            ..Default::default()
        };
        let value = self.db.get(&ro, key)?;
        // Record only the first observation of a key; a transaction's view of a
        // key it has not written is fixed at its snapshot.
        self.reads.entry(key.to_vec()).or_insert_with(|| value.clone());
        Ok(value)
    }

    /// Stage a put; visible to later reads in this transaction only.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.writes.insert(key.to_vec(), Staged::Put(value.to_vec()));
    }

    /// Stage a delete; visible to later reads in this transaction only.
    pub fn delete(&mut self, key: &[u8]) {
        self.writes.insert(key.to_vec(), Staged::Delete);
    }

    /// Discard all staged writes without touching the database.
    pub fn rollback(self) {
        // Dropping the transaction releases the snapshot and drops the buffers.
    }

    /// Validate the read set and, if consistent, publish the staged writes.
    ///
    /// Returns [`Error::InvalidArgument`] on conflict; the database is left
    /// unchanged in that case and the caller may retry with a fresh transaction.
    pub fn commit(self, opts: &WriteOptions) -> Result<()> {
        // A read-only transaction publishes nothing and so can never break
        // serializability: skip validation entirely.
        if self.writes.is_empty() {
            return Ok(());
        }

        // Optimistic validation: re-read each key in the read set at the latest
        // sequence and compare with what the transaction observed.
        let latest = ReadOptions::default();
        for (key, observed) in &self.reads {
            let current = self.db.get(&latest, key)?;
            if &current != observed {
                return Err(Error::invalid(format!(
                    "transaction conflict on key of length {}",
                    key.len()
                )));
            }
        }

        let mut batch = WriteBatch::new();
        for (key, staged) in &self.writes {
            match staged {
                Staged::Put(v) => batch.put(key, v),
                Staged::Delete => batch.delete(key),
            }
        }
        self.db.write(opts, batch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Options;

    fn db() -> Db {
        Db::open("txn-test", Options::default()).unwrap()
    }

    #[test]
    fn reads_own_writes() {
        let d = db();
        let mut tx = Transaction::begin(&d);
        tx.put(b"a", b"1");
        assert_eq!(tx.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
        tx.delete(b"a");
        assert_eq!(tx.get(b"a").unwrap(), None);
    }

    #[test]
    fn commit_publishes_writes() {
        let d = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        let mut tx = Transaction::begin(&d);
        tx.put(b"x", b"10");
        tx.put(b"y", b"20");
        // Uncommitted writes are invisible to the database.
        assert_eq!(d.get(&ro, b"x").unwrap(), None);
        tx.commit(&wo).unwrap();
        assert_eq!(d.get(&ro, b"x").unwrap().as_deref(), Some(&b"10"[..]));
        assert_eq!(d.get(&ro, b"y").unwrap().as_deref(), Some(&b"20"[..]));
    }

    #[test]
    fn rollback_discards_writes() {
        let d = db();
        let ro = ReadOptions::default();
        let mut tx = Transaction::begin(&d);
        tx.put(b"gone", b"value");
        tx.rollback();
        assert_eq!(d.get(&ro, b"gone").unwrap(), None);
    }

    #[test]
    fn disjoint_transactions_both_commit() {
        let d = db();
        let wo = WriteOptions::default();
        let ro = ReadOptions::default();
        let mut t1 = Transaction::begin(&d);
        let mut t2 = Transaction::begin(&d);
        t1.get(b"a").unwrap();
        t1.put(b"a", b"1");
        t2.get(b"b").unwrap();
        t2.put(b"b", b"2");
        t1.commit(&wo).unwrap();
        t2.commit(&wo).unwrap(); // touches a disjoint key: no conflict
        assert_eq!(d.get(&ro, b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(d.get(&ro, b"b").unwrap().as_deref(), Some(&b"2"[..]));
    }

    #[test]
    fn read_write_conflict_is_detected() {
        let d = db();
        let wo = WriteOptions::default();

        // t1 reads `k`, intending to write based on it.
        let mut t1 = Transaction::begin(&d);
        assert_eq!(t1.get(b"k").unwrap(), None);
        t1.put(b"k", b"from-t1");

        // A concurrent committed write changes `k` after t1's snapshot.
        let mut t2 = Transaction::begin(&d);
        t2.put(b"k", b"from-t2");
        t2.commit(&wo).unwrap();

        // t1 must now fail validation: what it read changed underneath it.
        let err = t1.commit(&wo).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    #[test]
    fn read_only_transaction_always_commits() {
        let d = db();
        let wo = WriteOptions::default();
        d.put(&wo, b"seed", b"v").unwrap();
        let mut t = Transaction::begin(&d);
        assert_eq!(t.get(b"seed").unwrap().as_deref(), Some(&b"v"[..]));
        // Even if the world moves on, a pure reader has nothing to publish.
        d.put(&wo, b"seed", b"v2").unwrap();
        t.commit(&wo).unwrap();
    }
}
