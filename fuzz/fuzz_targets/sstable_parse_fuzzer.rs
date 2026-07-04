//! Robustness fuzz target for the SSTable reader.
//!
//! It feeds arbitrary bytes to `Table::open` and, if a table parses, iterates it
//! to completion (with a step budget). A correct reader must treat every input as
//! untrusted: malformed data yields a clean `Err`, never a panic, out-of-bounds
//! access, unbounded allocation or infinite loop.
//!
//! Build/run: `cargo +nightly fuzz run sstable_parse_fuzzer`

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use lignite::comparator::BytewiseComparator;
use lignite::key::InternalKeyComparator;
use lignite::sstable::Table;
use lignite::InternalIterator;

fuzz_target!(|data: &[u8]| {
    let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
    // `None` filter policy; verify checksums so the CRC path is exercised too.
    if let Ok(table) = Table::open(Arc::new(data.to_vec()), icmp, None, true) {
        let mut it = table.iter();
        it.seek_to_first();
        let mut budget: u32 = 200_000;
        while it.valid() && budget > 0 {
            let _ = it.key();
            let _ = it.value();
            it.next();
            budget -= 1;
        }
        let _ = it.status();
    }
});
