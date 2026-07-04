//! Constructive SSTable roundtrip fuzz target.
//!
//! Given an arbitrary set of key/value pairs, it builds a valid SSTable and then
//! reads every key back through the table reader, asserting the values survive
//! the block/index/filter encoding intact. This exercises delta encoding, restart
//! points, the two-level index and the filter block over adversarially-shaped key
//! distributions (long shared prefixes, empty values, wildly varying lengths).
//!
//! Run: `cargo +nightly fuzz run table_roundtrip`

#![no_main]

use std::collections::BTreeMap;
use std::sync::Arc;

use libfuzzer_sys::fuzz_target;
use lignite::comparator::BytewiseComparator;
use lignite::filter::{BloomFilterPolicy, FilterPolicy};
use lignite::key::{append_internal_key, extract_user_key, InternalKeyComparator, ValueType};
use lignite::options::CompressionType;
use lignite::sstable::{Table, TableBuildOptions, TableBuilder};
use lignite::InternalIterator;

fuzz_target!(|pairs: Vec<(Vec<u8>, Vec<u8>)>| {
    // Deduplicate and sort by user key so the builder receives ascending keys.
    let mut map: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for (k, v) in pairs {
        if !k.is_empty() {
            map.insert(k, v);
        }
    }
    if map.is_empty() {
        return;
    }

    let icmp = InternalKeyComparator::new(Arc::new(BytewiseComparator));
    let policy: Arc<dyn FilterPolicy> = Arc::new(BloomFilterPolicy::new(10));
    let opts = TableBuildOptions {
        block_size: 64, // small blocks => many data blocks and index entries
        block_restart_interval: 4,
        compression: CompressionType::None,
        filter_policy: Some(Arc::clone(&policy)),
        icmp: icmp.clone(),
    };

    let mut builder = TableBuilder::new(opts);
    for (k, v) in &map {
        let mut ik = Vec::new();
        append_internal_key(&mut ik, k, 1, ValueType::Value);
        builder.add(&ik, v);
    }
    let image = Arc::new(builder.finish());

    let table = Table::open(image, icmp, Some(policy), true).expect("valid table must open");

    // Full scan must reproduce exactly the sorted map.
    let mut it = table.iter();
    it.seek_to_first();
    let mut seen = 0usize;
    for (k, v) in &map {
        assert!(it.valid(), "scan ended early");
        assert_eq!(extract_user_key(it.key()), k.as_slice());
        assert_eq!(it.value(), v.as_slice());
        it.next();
        seen += 1;
    }
    assert!(!it.valid(), "scan yielded extra entries");
    assert_eq!(seen, map.len());
});
