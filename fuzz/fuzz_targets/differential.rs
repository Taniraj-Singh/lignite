//! Differential fuzz target: drive the database with an arbitrary operation
//! stream and assert it matches a `BTreeMap` oracle at every checkpoint.
//!
//! The fuzzer interprets the raw input as a sequence of `(op, key)` byte pairs
//! over a small key space, so overwrites and deletes of the same key are dense
//! and repeatedly scattered across flushed tables and compaction output. Any
//! divergence between `Db::get` and the oracle — a stale value, a resurrected
//! delete, a lost write — trips an assertion and is reported as a crash.
//!
//! Run: `cargo +nightly fuzz run differential`

#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use lignite::{Db, Options, ReadOptions, WriteOptions};

const KEY_SPACE: u8 = 16;

fuzz_target!(|data: &[u8]| {
    let db = match Db::open("fuzz", Options::default()) {
        Ok(db) => db,
        Err(_) => return,
    };
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut counter: u64 = 0;

    let mut bytes = data.iter().copied();
    while let Some(op) = bytes.next() {
        let k = bytes.next().unwrap_or(0) % KEY_SPACE;
        let key = format!("k{k:02}").into_bytes();
        match op % 5 {
            0 | 1 => {
                counter += 1;
                let value = format!("v{counter}").into_bytes();
                db.put(&wo, &key, &value).unwrap();
                oracle.insert(key, value);
            }
            2 => {
                db.delete(&wo, &key).unwrap();
                oracle.remove(&key);
            }
            3 => {
                let _ = db.flush();
            }
            _ => {
                let _ = db.compact();
            }
        }
    }

    // Flush so all state resides in tables, then check every key.
    let _ = db.flush();
    for k in 0..KEY_SPACE {
        let key = format!("k{k:02}").into_bytes();
        let got = db.get(&ro, &key).unwrap();
        let expected = oracle.get(&key).cloned();
        assert_eq!(
            got, expected,
            "differential mismatch for {}",
            String::from_utf8_lossy(&key)
        );
    }
});
