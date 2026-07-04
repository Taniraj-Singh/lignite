//! Recovery: rebuilding in-memory state from the write-ahead log.
//!
//! After a restart the engine replays each logged [`WriteBatch`] into a fresh
//! memtable, in order, so that acknowledged-but-unflushed writes are not lost.
//! Because a batch records its own base sequence number, replay restores the
//! exact sequence numbering the writes originally received, preserving MVCC
//! ordering across the crash.
//!
//! The on-disk environment drives this on open; the routine itself is pure and
//! independently tested, taking the raw log bytes and producing a memtable plus
//! the highest sequence number it observed.

use crate::batch::WriteBatch;
use crate::error::Result;
use crate::key::{InternalKeyComparator, SequenceNumber};
use crate::memtable::MemTable;
use crate::wal::LogReader;

/// The result of replaying a log segment.
pub struct Recovered {
    /// A memtable populated with every replayed record.
    pub memtable: MemTable,
    /// The largest sequence number applied (0 if the log was empty).
    pub max_sequence: SequenceNumber,
    /// Number of batches replayed.
    pub batches: usize,
}

/// Replay `log_bytes` into a new memtable ordered by `icmp`.
///
/// `verify_checksums` toggles CRC verification of each physical log record. A
/// corrupt record aborts recovery with [`crate::Error::Corruption`].
pub fn recover_memtable(
    log_bytes: &[u8],
    icmp: InternalKeyComparator,
    verify_checksums: bool,
) -> Result<Recovered> {
    let mut memtable = MemTable::new(icmp);
    let mut reader = LogReader::new(log_bytes, verify_checksums);
    let mut max_sequence = 0;
    let mut batches = 0;

    while let Some(record) = reader.read_record() {
        let payload = record?;
        let batch = WriteBatch::from_contents(payload)?;
        if batch.count() == 0 {
            continue;
        }
        let base = batch.sequence();
        let last = base + batch.count() as u64 - 1;
        batch.insert_into(&mut memtable)?;
        if last > max_sequence {
            max_sequence = last;
        }
        batches += 1;
    }

    Ok(Recovered {
        memtable,
        max_sequence,
        batches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comparator::BytewiseComparator;
    use crate::key::LookupKey;
    use crate::wal::LogWriter;
    use std::sync::Arc;

    fn icmp() -> InternalKeyComparator {
        InternalKeyComparator::new(Arc::new(BytewiseComparator))
    }

    /// Build a log the way the write path does: each batch stamped with its base
    /// sequence, then appended as one record.
    fn make_log(ops: &[(SequenceNumber, WriteBatch)]) -> Vec<u8> {
        let mut w = LogWriter::new();
        for (seq, batch) in ops {
            let mut b = batch.clone();
            b.set_sequence(*seq);
            w.add_record(b.contents());
        }
        w.into_bytes()
    }

    #[test]
    fn replays_records_in_order() {
        let mut b1 = WriteBatch::new();
        b1.put(b"k", b"first");
        let mut b2 = WriteBatch::new();
        b2.put(b"k", b"second");
        b2.put(b"other", b"x");

        // b1 gets sequence 1, b2 gets 2..3.
        let log = make_log(&[(1, b1), (2, b2)]);
        let recovered = recover_memtable(&log, icmp(), true).unwrap();

        assert_eq!(recovered.batches, 2);
        assert_eq!(recovered.max_sequence, 3);
        // The later write to "k" must win.
        let lk = LookupKey::new(b"k", 1000);
        assert_eq!(recovered.memtable.get(&lk).unwrap().unwrap(), b"second");
        let lk_other = LookupKey::new(b"other", 1000);
        assert_eq!(recovered.memtable.get(&lk_other).unwrap().unwrap(), b"x");
    }

    #[test]
    fn empty_log_recovers_empty_memtable() {
        let recovered = recover_memtable(&[], icmp(), true).unwrap();
        assert_eq!(recovered.batches, 0);
        assert_eq!(recovered.max_sequence, 0);
        assert!(recovered.memtable.is_empty());
    }

    #[test]
    fn deletion_is_replayed() {
        let mut b1 = WriteBatch::new();
        b1.put(b"gone", b"here");
        let mut b2 = WriteBatch::new();
        b2.delete(b"gone");
        let log = make_log(&[(1, b1), (2, b2)]);
        let recovered = recover_memtable(&log, icmp(), true).unwrap();
        let lk = LookupKey::new(b"gone", 1000);
        assert!(recovered.memtable.get(&lk).unwrap().unwrap_err().is_not_found());
    }

    #[test]
    fn corrupt_log_is_rejected() {
        let mut b = WriteBatch::new();
        b.put(b"key", b"value-with-enough-bytes-to-corrupt");
        let mut log = make_log(&[(1, b)]);
        // Flip a byte inside the payload.
        let mid = log.len() / 2;
        log[mid] ^= 0xff;
        assert!(recover_memtable(&log, icmp(), true).is_err());
    }
}
