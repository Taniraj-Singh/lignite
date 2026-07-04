//! Behavioural coverage of the public iterator and the runtime stat counters,
//! staying within single-run scenarios so results are independent of multi-table
//! version resolution.

use lignite::{Db, LigniteIterator, Options, ReadOptions, WriteOptions};

fn open() -> Db {
    Db::open("iter", Options::default()).unwrap()
}

fn seed(db: &Db, keys: &[(&str, &str)]) {
    let wo = WriteOptions::default();
    for (k, v) in keys {
        db.put(&wo, k.as_bytes(), v.as_bytes()).unwrap();
    }
    db.flush().unwrap();
}

#[test]
fn scan_skips_deleted_keys() {
    let db = open();
    let wo = WriteOptions::default();
    db.put(&wo, b"a", b"1").unwrap();
    db.put(&wo, b"b", b"2").unwrap();
    db.put(&wo, b"c", b"3").unwrap();
    db.delete(&wo, b"b").unwrap();
    db.flush().unwrap(); // single table: a,c live; b tombstoned

    let pairs = db.new_iterator(&ReadOptions::default()).unwrap().collect_all();
    let keys: Vec<String> = pairs
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    assert_eq!(keys, vec!["a", "c"]);
}

#[test]
fn seek_and_walk() {
    let db = open();
    seed(&db, &[("k01", "a"), ("k03", "b"), ("k05", "c"), ("k07", "d")]);

    let mut it = db.new_iterator(&ReadOptions::default()).unwrap();
    it.seek(b"k04"); // absent; next is k05
    let mut got = Vec::new();
    while it.valid() {
        got.push(String::from_utf8_lossy(it.key()).into_owned());
        it.next();
    }
    assert_eq!(got, vec!["k05", "k07"]);
}

#[test]
fn seek_before_everything_lands_on_first() {
    let db = open();
    seed(&db, &[("m", "1"), ("n", "2")]);
    let mut it = db.new_iterator(&ReadOptions::default()).unwrap();
    it.seek(b"a");
    assert!(it.valid());
    assert_eq!(it.key(), b"m");
}

#[test]
fn snapshot_scan_ignores_later_writes() {
    let db = open();
    let wo = WriteOptions::default();
    db.put(&wo, b"a", b"1").unwrap();
    db.put(&wo, b"b", b"1").unwrap();
    db.flush().unwrap();

    let snap = db.snapshot();
    // Writes after the snapshot: these go to a new memtable, so the scan (which
    // reads at the snapshot) must not see them.
    db.put(&wo, b"c", b"2").unwrap();

    let ro = ReadOptions {
        snapshot: Some(snap),
        ..Default::default()
    };
    let pairs = db.new_iterator(&ro).unwrap().collect_all();
    let keys: Vec<String> = pairs
        .iter()
        .map(|(k, _)| String::from_utf8_lossy(k).into_owned())
        .collect();
    assert_eq!(keys, vec!["a", "b"]);
}

#[test]
fn stats_track_activity() {
    let db = open();
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();
    for i in 0..10u32 {
        db.put(&wo, format!("k{i}").as_bytes(), b"v").unwrap();
    }
    db.flush().unwrap();
    let _ = db.get(&ro, b"k0").unwrap(); // hit
    let _ = db.get(&ro, b"absent").unwrap(); // miss

    let s = db.stats();
    assert_eq!(s.writes, 10);
    assert!(s.write_bytes > 0);
    assert_eq!(s.reads, 2);
    assert_eq!(s.read_hits, 1);
    assert_eq!(s.flushes, 1);
    assert_eq!(s.files_created, 1);
}

#[test]
fn compaction_stats_recorded() {
    let db = open();
    let wo = WriteOptions::default();
    // Distinct keys per flush -> three non-conflicting tables.
    for i in 0..3u32 {
        db.put(&wo, format!("k{i}").as_bytes(), b"v").unwrap();
        db.flush().unwrap();
    }
    db.compact().unwrap();
    assert_eq!(db.stats().compactions, 1);
    assert_eq!(db.live_file_count(), 1);
}
