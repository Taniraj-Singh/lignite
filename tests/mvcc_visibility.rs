//! End-to-end behavioural tests for multi-version visibility across sorted runs.
//!
//! These exercise the contract every LSM engine must uphold: once a key has been
//! written more than once — and those writes have landed in *different* sorted
//! runs (separate flushed tables, or a table plus a later overwrite) — a read
//! must observe the **newest** version at or below its snapshot, and a deletion
//! must not be undone by an older surviving value.
//!
//! Each scenario writes a key, flushes it to an SSTable, then writes the same key
//! again and flushes once more, producing two overlapping level-0 tables that
//! both contain the key. A correct engine returns the second (newer) value; a
//! delete leaves the key absent.

use lignite::{Db, Options, ReadOptions, WriteOptions};

fn open() -> Db {
    Db::open("mvcc-test", Options::default()).unwrap()
}

/// An overwrite that lands in a newer table must shadow the value in the older
/// table.
#[test]
fn overwrite_is_visible_across_flushed_tables() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"account:42", b"balance=100").unwrap();
    db.flush().unwrap(); // table A holds account:42 -> balance=100

    db.put(&wo, b"account:42", b"balance=250").unwrap();
    db.flush().unwrap(); // table B holds account:42 -> balance=250

    assert_eq!(db.live_file_count(), 2, "expected two overlapping tables");
    assert_eq!(
        db.get(&ro, b"account:42").unwrap().as_deref(),
        Some(&b"balance=250"[..]),
        "the newer value must win once both versions live in separate tables"
    );
}

/// A deletion flushed on top of a value must not be undone by the older value in
/// a previously-flushed table.
#[test]
fn deletion_is_not_resurrected_by_older_table() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"session:abc", b"active").unwrap();
    db.flush().unwrap(); // older table: session:abc -> active

    db.delete(&wo, b"session:abc").unwrap();
    db.flush().unwrap(); // newer table: session:abc -> <tombstone>

    assert_eq!(
        db.get(&ro, b"session:abc").unwrap(),
        None,
        "a deleted key must stay deleted, not resurrect its older value"
    );
}

/// With many overwrites across many tables, the very latest value must be read.
#[test]
fn latest_of_many_overwrites_is_returned() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    for version in 0..10u32 {
        db.put(&wo, b"counter", format!("v{version}").as_bytes())
            .unwrap();
        db.flush().unwrap();
    }
    assert_eq!(db.live_file_count(), 10);
    assert_eq!(
        db.get(&ro, b"counter").unwrap().as_deref(),
        Some(&b"v9"[..]),
        "the newest of ten flushed versions must be returned"
    );
}

/// A range scan must reflect the newest version of every key, even when versions
/// are spread across overlapping tables.
#[test]
fn scan_reflects_latest_versions() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"a", b"1").unwrap();
    db.put(&wo, b"b", b"1").unwrap();
    db.flush().unwrap(); // table 1: a=1, b=1

    db.put(&wo, b"a", b"2").unwrap();
    db.put(&wo, b"c", b"2").unwrap();
    db.flush().unwrap(); // table 2: a=2, c=2

    let pairs = db.new_iterator(&ro).unwrap().collect_all();
    let decoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| {
            (
                String::from_utf8_lossy(k).into_owned(),
                String::from_utf8_lossy(v).into_owned(),
            )
        })
        .collect();

    assert_eq!(
        decoded,
        vec![
            ("a".to_string(), "2".to_string()),
            ("b".to_string(), "1".to_string()),
            ("c".to_string(), "2".to_string()),
        ],
        "the scan must surface a=2 (newer), not a=1 (older)"
    );
}

/// The same hazard, surfaced through compaction: folding the overlapping tables
/// together must keep the newest value, not the oldest.
#[test]
fn compaction_keeps_newest_version() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"doc", b"draft").unwrap();
    db.flush().unwrap();
    db.put(&wo, b"doc", b"final").unwrap();
    db.flush().unwrap();

    db.compact().unwrap();

    assert_eq!(
        db.get(&ro, b"doc").unwrap().as_deref(),
        Some(&b"final"[..]),
        "compaction must retain the newest version of a key"
    );
}
