//! Robustness fuzz target for the full write-ahead-log recovery path.
//!
//! Where `wal_parse_fuzzer` stops at physical-record framing, this target drives
//! `recover_memtable`, which chains the whole recovery pipeline on untrusted
//! bytes: `LogReader` reassembles logical records, each is decoded as a
//! `WriteBatch`, and every batch is replayed into a memtable while its base
//! sequence number is threaded through. Any corrupt log — bad framing, a batch
//! shorter than its header, a record count that disagrees with the payload — must
//! resolve to a clean `Err`, never a panic, an over-read, or a hang.
//!
//! This deeper entry point was previously unfuzzed.
//!
//! Build/run: `cargo +nightly fuzz run wal_recover_fuzzer`

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use lignite::comparator::BytewiseComparator;
use lignite::db::recover_memtable;
use lignite::key::InternalKeyComparator;

fuzz_target!(|data: &[u8]| {
    let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
    // Verify checksums so the CRC path is exercised alongside the decoders.
    let _ = recover_memtable(data, icmp, true);
});
