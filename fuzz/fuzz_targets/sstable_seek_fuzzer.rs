//! Robustness fuzz target for the SSTable seek and filter paths.
//!
//! `sstable_parse_fuzzer` only drives `seek_to_first` + `next`, so the
//! binary-search seek path (`BlockIter::seek` / `restart_key`) and the bloom
//! `FilterBlockReader` are never exercised on hostile input. This target fills
//! that gap: it splits the input into a seek target and a table image, opens the
//! table *with* a filter policy (reaching the metaindex and filter-block reader),
//! then seeks to an arbitrary target and scans. Every offset the reader trusts —
//! restart-array entries, filter offsets, block handles — comes from attacker
//! bytes and must stay bounds-checked: a clean `Err` or empty result, never a
//! panic, an out-of-bounds access, or a hang.
//!
//! Build/run: `cargo +nightly fuzz run sstable_seek_fuzzer`

#![no_main]

use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use lignite::comparator::BytewiseComparator;
use lignite::filter::{BloomFilterPolicy, FilterPolicy};
use lignite::key::InternalKeyComparator;
use lignite::sstable::Table;
use lignite::InternalIterator;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte selects how many of the following bytes form the seek target;
    // the remainder is the table image.
    let tlen = (data[0] as usize).min(data.len() - 1);
    let (target, table_bytes) = data[1..].split_at(tlen);

    let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
    let policy: Arc<dyn FilterPolicy> = Arc::new(BloomFilterPolicy::new(10));

    if let Ok(table) = Table::open(Arc::new(table_bytes.to_vec()), icmp, Some(policy), true) {
        // Filter path on an attacker-controlled offset/key.
        let _ = table.filter_may_match(0, target);

        let mut it = table.iter();
        it.seek(target); // binary search over restart array -> restart_key
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
