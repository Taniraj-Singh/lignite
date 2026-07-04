//! A deterministic differential fuzzer.
//!
//! It drives the database with a pseudo-random stream of puts, deletes, flushes
//! and compactions while maintaining a `BTreeMap` **oracle** of the expected
//! latest state. Periodically it checks that every key resolves through
//! `Db::get` exactly as the oracle says, and that a full range scan reproduces
//! the oracle's live entries in order.
//!
//! The workload deliberately uses a *small* key space with frequent flushes, so
//! the same key's versions are constantly scattered across multiple persisted
//! tables — precisely the situation a correct engine must resolve to the newest
//! version. Values are globally unique (`v{counter}`), so a read that returns a
//! superseded value is caught immediately.
//!
//! The RNG is a fixed-seed xorshift, so every run is byte-for-byte reproducible;
//! the seeds are enumerated below. These checks are part of the correctness
//! contract: they must pass for a correct engine.

use std::collections::BTreeMap;

use lignite::{Db, Options, ReadOptions, WriteOptions};

/// A tiny deterministic xorshift64* generator (no external deps, reproducible).
struct XorShift64 {
    state: u64,
}
impl XorShift64 {
    fn new(seed: u64) -> Self {
        XorShift64 {
            state: seed | 1, // never zero
        }
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

fn key_for(k: u64) -> Vec<u8> {
    format!("key-{k:03}").into_bytes()
}

/// Run one deterministic campaign and assert the engine matches the oracle
/// throughout.
fn run_campaign(seed: u64, steps: usize, key_space: u64) {
    let db = Db::open("fuzz", Options::default()).unwrap();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();
    let mut rng = XorShift64::new(seed);

    // oracle[key] = Some(value) if live, absent if never written or deleted.
    let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut counter: u64 = 0;

    for step in 0..steps {
        let roll = rng.below(100);
        if roll < 62 {
            // put
            let key = key_for(rng.below(key_space));
            counter += 1;
            let value = format!("v{counter:08}").into_bytes();
            db.put(&wo, &key, &value).unwrap();
            oracle.insert(key, value);
        } else if roll < 82 {
            // delete
            let key = key_for(rng.below(key_space));
            db.delete(&wo, &key).unwrap();
            oracle.remove(&key);
        } else if roll < 97 {
            // flush the active memtable into a fresh table
            db.flush().unwrap();
        } else {
            // full compaction
            db.compact().unwrap();
        }

        // Periodically cross-check the whole key space and a scan.
        if step % 40 == 0 {
            verify_points(&db, &ro, &oracle, key_space, seed, step);
        }
    }

    // Final, exhaustive verification (after a flush, so everything is in tables).
    db.flush().unwrap();
    verify_points(&db, &ro, &oracle, key_space, seed, steps);
    verify_scan(&db, &ro, &oracle, seed);
}

fn verify_points(
    db: &Db,
    ro: &ReadOptions,
    oracle: &BTreeMap<Vec<u8>, Vec<u8>>,
    key_space: u64,
    seed: u64,
    step: usize,
) {
    for k in 0..key_space {
        let key = key_for(k);
        let got = db.get(ro, &key).unwrap();
        let expected = oracle.get(&key).cloned();
        assert_eq!(
            got,
            expected,
            "point mismatch (seed={seed}, step={step}) for key {}: engine returned {:?}, oracle has {:?}",
            String::from_utf8_lossy(&key),
            got.as_deref().map(String::from_utf8_lossy),
            expected.as_deref().map(String::from_utf8_lossy),
        );
    }
}

fn verify_scan(db: &Db, ro: &ReadOptions, oracle: &BTreeMap<Vec<u8>, Vec<u8>>, seed: u64) {
    let scanned = db.new_iterator(ro).unwrap().collect_all();
    let expected: Vec<(Vec<u8>, Vec<u8>)> =
        oracle.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(
        scanned.len(),
        expected.len(),
        "scan length mismatch (seed={seed}): got {}, expected {}",
        scanned.len(),
        expected.len()
    );
    for (i, (got, want)) in scanned.iter().zip(&expected).enumerate() {
        assert_eq!(
            got, want,
            "scan mismatch (seed={seed}) at index {i}: got ({:?},{:?}), expected ({:?},{:?})",
            String::from_utf8_lossy(&got.0),
            String::from_utf8_lossy(&got.1),
            String::from_utf8_lossy(&want.0),
            String::from_utf8_lossy(&want.1),
        );
    }
}

#[test]
fn differential_small_keyspace_seed_1() {
    run_campaign(0x1111_2222_3333_4444, 1500, 24);
}

#[test]
fn differential_small_keyspace_seed_2() {
    run_campaign(0xdead_beef_cafe_babe, 1500, 24);
}

#[test]
fn differential_tiny_keyspace_high_collision() {
    // Very small key space => nearly every write is an overwrite across tables.
    run_campaign(0x0123_4567_89ab_cdef, 2000, 8);
}

#[test]
fn differential_wider_keyspace() {
    run_campaign(0xa5a5_5a5a_f0f0_0f0f, 2500, 64);
}
