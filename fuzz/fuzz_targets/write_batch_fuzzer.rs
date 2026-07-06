//! Robustness fuzz target for the write-batch record decoder.
//!
//! A `WriteBatch`'s serialised form doubles as the write-ahead-log payload, so it
//! is reconstructed from untrusted bytes on recovery. `insert_into` walks the
//! record stream — a header plus a sequence of tag / length-prefixed-key
//! (/ value) records — and replays it into a memtable, assigning a sequence
//! number per operation. A malformed header, an unknown tag, a lying length, or a
//! count that disagrees with the records present must all yield a clean `Err`,
//! never a panic or an over-read.
//!
//! This entry point was previously unfuzzed.
//!
//! Build/run: `cargo +nightly fuzz run write_batch_fuzzer`

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use lignite::comparator::BytewiseComparator;
use lignite::key::InternalKeyComparator;
use lignite::memtable::MemTable;
use lignite::WriteBatch;

fuzz_target!(|data: &[u8]| {
    if let Ok(batch) = WriteBatch::from_contents(data.to_vec()) {
        let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
        let mut mt = MemTable::new(icmp);
        // Replaying exercises the tag/varint record parser and the per-record
        // sequence-number assignment. A corrupt batch must surface as `Err`.
        let _ = batch.insert_into(&mut mt);
    }
});
