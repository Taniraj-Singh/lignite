//! Additional proof-of-concept regressions for cross-run version resolution.
//!
//! Each test constructs a scenario in which a key's versions are deliberately
//! spread across multiple persisted tables (or a table plus a tombstone in a
//! later table) and asserts the value contract a correct engine must uphold: the
//! newest version at or below the read point wins, and deletions are durable.
//!
//! These complement `tests/mvcc_visibility.rs` with interleaved multi-key
//! workloads, three-way table overlaps, compaction interactions and
//! snapshot-crossing reads, so the failure is visible from many angles rather
//! than a single shape.

use lignite::{Db, LigniteIterator, Options, ReadOptions, WriteOptions};

fn open() -> Db {
    Db::open("poc", Options::default()).unwrap()
}

/// Three tables each carrying a newer version of the same key: the last wins.
#[test]
fn three_tables_last_version_wins() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();
    for v in ["one", "two", "three"] {
        db.put(&wo, b"k", v.as_bytes()).unwrap();
        db.flush().unwrap();
    }
    assert_eq!(db.live_file_count(), 3);
    assert_eq!(db.get(&ro, b"k").unwrap().as_deref(), Some(&b"three"[..]));
}

/// Several keys, each overwritten in a different later table, interleaved.
#[test]
fn interleaved_multikey_overwrites() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    // Table 1: original values.
    db.put(&wo, b"alpha", b"a1").unwrap();
    db.put(&wo, b"bravo", b"b1").unwrap();
    db.put(&wo, b"charlie", b"c1").unwrap();
    db.flush().unwrap();

    // Table 2: overwrite alpha and charlie.
    db.put(&wo, b"alpha", b"a2").unwrap();
    db.put(&wo, b"charlie", b"c2").unwrap();
    db.flush().unwrap();

    // Table 3: overwrite bravo.
    db.put(&wo, b"bravo", b"b2").unwrap();
    db.flush().unwrap();

    assert_eq!(db.get(&ro, b"alpha").unwrap().as_deref(), Some(&b"a2"[..]));
    assert_eq!(db.get(&ro, b"bravo").unwrap().as_deref(), Some(&b"b2"[..]));
    assert_eq!(db.get(&ro, b"charlie").unwrap().as_deref(), Some(&b"c2"[..]));
}

/// A value, a delete, then a re-insert, each in its own table: the final
/// re-insert must be visible (not the delete, not the original).
#[test]
fn write_delete_rewrite_across_tables() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"k", b"first").unwrap();
    db.flush().unwrap();
    db.delete(&wo, b"k").unwrap();
    db.flush().unwrap();
    db.put(&wo, b"k", b"second").unwrap();
    db.flush().unwrap();

    assert_eq!(db.get(&ro, b"k").unwrap().as_deref(), Some(&b"second"[..]));
}

/// After overwrites are spread across tables, a full compaction must fold them to
/// the newest value; reads afterwards must be correct.
#[test]
fn compaction_after_scattered_overwrites() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    for round in 0..6u32 {
        db.put(&wo, b"hot", format!("gen{round}").as_bytes()).unwrap();
        db.put(&wo, b"cold", format!("c{round}").as_bytes()).unwrap();
        db.flush().unwrap();
    }
    db.compact().unwrap();
    assert_eq!(db.get(&ro, b"hot").unwrap().as_deref(), Some(&b"gen5"[..]));
    assert_eq!(db.get(&ro, b"cold").unwrap().as_deref(), Some(&b"c5"[..]));
}

/// A scan across three overlapping tables must reflect the newest version of
/// every key and omit deleted keys.
#[test]
fn scan_across_three_tables() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"a", b"a-old").unwrap();
    db.put(&wo, b"b", b"b-old").unwrap();
    db.put(&wo, b"c", b"c-old").unwrap();
    db.flush().unwrap();

    db.put(&wo, b"a", b"a-new").unwrap();
    db.delete(&wo, b"b").unwrap();
    db.flush().unwrap();

    db.put(&wo, b"d", b"d-new").unwrap();
    db.flush().unwrap();

    let pairs: Vec<(String, String)> = db
        .new_iterator(&ro)
        .unwrap()
        .collect_all()
        .into_iter()
        .map(|(k, v)| {
            (
                String::from_utf8_lossy(&k).into_owned(),
                String::from_utf8_lossy(&v).into_owned(),
            )
        })
        .collect();

    assert_eq!(
        pairs,
        vec![
            ("a".to_string(), "a-new".to_string()),
            ("c".to_string(), "c-old".to_string()),
            ("d".to_string(), "d-new".to_string()),
        ]
    );
}

/// A snapshot taken between two flushed writes must see the older value, while a
/// latest read sees the newer — both resolved across tables.
#[test]
fn snapshot_crosses_tables() {
    let db = open();
    let wo = WriteOptions::default();

    db.put(&wo, b"k", b"snapshotted").unwrap();
    db.flush().unwrap();
    let snap = db.snapshot();
    db.put(&wo, b"k", b"after").unwrap();
    db.flush().unwrap();

    let snap_ro = ReadOptions {
        snapshot: Some(snap),
        ..Default::default()
    };
    assert_eq!(db.get(&snap_ro, b"k").unwrap().as_deref(), Some(&b"snapshotted"[..]));
    let ro = ReadOptions::default();
    assert_eq!(db.get(&ro, b"k").unwrap().as_deref(), Some(&b"after"[..]));
}

/// Iterating forward after seeking into the middle of a multi-table key range
/// must still yield newest versions in order.
#[test]
fn seek_then_scan_multi_table() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    for i in 0..10u32 {
        db.put(&wo, format!("k{i:02}").as_bytes(), format!("old{i}").as_bytes()).unwrap();
    }
    db.flush().unwrap();
    // Overwrite the even keys into a second table.
    for i in (0..10u32).step_by(2) {
        db.put(&wo, format!("k{i:02}").as_bytes(), format!("new{i}").as_bytes()).unwrap();
    }
    db.flush().unwrap();

    let mut it = db.new_iterator(&ro).unwrap();
    it.seek(b"k04");
    let mut got = Vec::new();
    while it.valid() {
        got.push((
            String::from_utf8_lossy(it.key()).into_owned(),
            String::from_utf8_lossy(it.value()).into_owned(),
        ));
        it.next();
    }
    // k04 (even -> new), k05 (odd -> old), k06 (new), ...
    assert_eq!(got[0], ("k04".to_string(), "new4".to_string()));
    assert_eq!(got[1], ("k05".to_string(), "old5".to_string()));
    assert_eq!(got[2], ("k06".to_string(), "new6".to_string()));
}
