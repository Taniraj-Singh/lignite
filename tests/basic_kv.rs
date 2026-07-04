//! Broad functional coverage of the public key/value API that does not depend on
//! multi-table version resolution: single-run reads, memtable semantics,
//! snapshots, iteration and a randomised oracle comparison.

use std::collections::BTreeMap;

use lignite::{Db, LigniteIterator, Options, ReadOptions, WriteOptions};
use lignite::util::Random;

fn open() -> Db {
    Db::open("basic", Options::default()).unwrap()
}

fn wo() -> WriteOptions {
    WriteOptions::default()
}
fn ro() -> ReadOptions {
    ReadOptions::default()
}

#[test]
fn get_missing_returns_none() {
    let db = open();
    assert_eq!(db.get(&ro(), b"nope").unwrap(), None);
}

#[test]
fn put_and_overwrite_in_memtable() {
    let db = open();
    db.put(&wo(), b"k", b"one").unwrap();
    db.put(&wo(), b"k", b"two").unwrap();
    db.put(&wo(), b"k", b"three").unwrap();
    assert_eq!(db.get(&ro(), b"k").unwrap().as_deref(), Some(&b"three"[..]));
}

#[test]
fn overwrite_then_single_flush_reads_latest() {
    // Several versions of one key, flushed together into ONE table, still
    // resolves to the newest version (single sorted run).
    let db = open();
    for v in 0..20u32 {
        db.put(&wo(), b"key", format!("v{v}").as_bytes()).unwrap();
    }
    db.flush().unwrap();
    assert_eq!(db.live_file_count(), 1);
    assert_eq!(db.get(&ro(), b"key").unwrap().as_deref(), Some(&b"v19"[..]));
}

#[test]
fn delete_then_flush_is_absent() {
    let db = open();
    db.put(&wo(), b"k", b"v").unwrap();
    db.delete(&wo(), b"k").unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(&ro(), b"k").unwrap(), None);
}

#[test]
fn many_distinct_keys_across_several_flushes() {
    // Distinct keys per flush never collide, so multi-table reads are fine.
    let db = open();
    for batch in 0..5u32 {
        for i in 0..50u32 {
            let key = format!("b{batch}-k{i:03}");
            db.put(&wo(), key.as_bytes(), format!("{batch}:{i}").as_bytes())
                .unwrap();
        }
        db.flush().unwrap();
    }
    assert_eq!(db.live_file_count(), 5);
    assert_eq!(db.get(&ro(), b"b0-k000").unwrap().as_deref(), Some(&b"0:0"[..]));
    assert_eq!(db.get(&ro(), b"b4-k049").unwrap().as_deref(), Some(&b"4:49"[..]));
    assert_eq!(db.get(&ro(), b"b2-k025").unwrap().as_deref(), Some(&b"2:25"[..]));
    assert_eq!(db.get(&ro(), b"absent").unwrap(), None);
}

#[test]
fn snapshot_sees_old_state() {
    let db = open();
    db.put(&wo(), b"x", b"before").unwrap();
    let snap = db.snapshot();
    db.put(&wo(), b"x", b"after").unwrap();
    db.delete(&wo(), b"y").unwrap();

    let snap_ro = ReadOptions {
        snapshot: Some(snap),
        ..Default::default()
    };
    assert_eq!(db.get(&snap_ro, b"x").unwrap().as_deref(), Some(&b"before"[..]));
    assert_eq!(db.get(&ro(), b"x").unwrap().as_deref(), Some(&b"after"[..]));
}

#[test]
fn iterator_over_single_run_is_sorted_and_complete() {
    let db = open();
    let keys = ["delta", "alpha", "charlie", "bravo", "echo"];
    for (i, k) in keys.iter().enumerate() {
        db.put(&wo(), k.as_bytes(), format!("{i}").as_bytes()).unwrap();
    }
    db.flush().unwrap();

    let pairs = db.new_iterator(&ro()).unwrap().collect_all();
    let got: Vec<String> = pairs
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    assert_eq!(got, vec!["alpha", "bravo", "charlie", "delta", "echo"]);
}

#[test]
fn iterator_seek_positions_correctly() {
    let db = open();
    for i in (0..100u32).step_by(2) {
        db.put(&wo(), format!("k{i:03}").as_bytes(), b"v").unwrap();
    }
    db.flush().unwrap();

    let mut it = db.new_iterator(&ro()).unwrap();
    it.seek(b"k051"); // not present; next present key is k052
    assert!(it.valid());
    assert_eq!(it.key(), b"k052");
}

#[test]
fn empty_iterator_is_invalid() {
    let db = open();
    let mut it = db.new_iterator(&ro()).unwrap();
    it.seek_to_first();
    assert!(!it.valid());
}

/// Compare a workload against an in-memory `BTreeMap` oracle. Writes go through a
/// single flush at the end, so every key resides in exactly one table and reads
/// exercise the full memtable → SSTable path without crossing runs.
#[test]
fn randomised_oracle_single_flush() {
    let db = open();
    let mut oracle: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
    let mut rng = Random::new(0x51ce);

    for step in 0..4000u32 {
        let key = format!("key-{:04}", rng.uniform(600)).into_bytes();
        if rng.one_in(5) {
            db.delete(&wo(), &key).unwrap();
            oracle.insert(key, None);
        } else {
            let value = format!("value-{step}").into_bytes();
            db.put(&wo(), &key, &value).unwrap();
            oracle.insert(key, Some(value));
        }
    }

    // Verify in the memtable first.
    for (k, expected) in &oracle {
        assert_eq!(db.get(&ro(), k).unwrap().as_deref(), expected.as_deref());
    }

    // Flush everything into one table and verify again.
    db.flush().unwrap();
    assert_eq!(db.live_file_count(), 1);
    for (k, expected) in &oracle {
        assert_eq!(
            db.get(&ro(), k).unwrap().as_deref(),
            expected.as_deref(),
            "mismatch for key {:?} after flush",
            String::from_utf8_lossy(k)
        );
    }
}

#[test]
fn large_values_roundtrip() {
    let db = open();
    let big = vec![0x5au8; 200_000];
    db.put(&wo(), b"blob", &big).unwrap();
    db.flush().unwrap();
    assert_eq!(db.get(&ro(), b"blob").unwrap().unwrap(), big);
}

#[test]
fn write_batch_is_atomic_unit() {
    use lignite::WriteBatch;
    let db = open();
    let mut batch = WriteBatch::new();
    batch.put(b"a", b"1");
    batch.put(b"b", b"2");
    batch.delete(b"c");
    db.write(&wo(), batch).unwrap();
    assert_eq!(db.get(&ro(), b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(db.get(&ro(), b"b").unwrap().as_deref(), Some(&b"2"[..]));
    assert_eq!(db.get(&ro(), b"c").unwrap(), None);
}
