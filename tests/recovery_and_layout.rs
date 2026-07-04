//! Integration coverage for the write-ahead-log recovery path, the MANIFEST
//! version-edit codec and the compaction picker's decisions. These components
//! are exercised through their public interfaces without depending on
//! multi-table version resolution.

use std::sync::Arc;

use lignite::batch::WriteBatch;
use lignite::comparator::BytewiseComparator;
use lignite::compaction::picker::{CompactionPicker, L0_COMPACTION_TRIGGER};
use lignite::db::recover_memtable;
use lignite::key::{append_internal_key, InternalKeyComparator, LookupKey, ValueType};
use lignite::version::{FileMetaData, Version, VersionEdit};
use lignite::wal::LogWriter;

fn icmp() -> InternalKeyComparator {
    InternalKeyComparator::new(Arc::new(BytewiseComparator))
}

fn ik(user: &[u8]) -> Vec<u8> {
    let mut k = Vec::new();
    append_internal_key(&mut k, user, 1, ValueType::Value);
    k
}

fn file(number: u64, size: u64, lo: &[u8], hi: &[u8]) -> Arc<FileMetaData> {
    Arc::new(FileMetaData {
        number,
        file_size: size,
        smallest: ik(lo),
        largest: ik(hi),
        blob: Arc::new(Vec::new()),
    })
}

#[test]
fn wal_recovery_rebuilds_memtable() {
    // Build a log the way the engine's write path does.
    let mut w = LogWriter::new();
    let mut seq = 1u64;
    for i in 0..100u32 {
        let mut b = WriteBatch::new();
        b.set_sequence(seq);
        b.put(format!("k{i:03}").as_bytes(), format!("v{i}").as_bytes());
        seq += b.count() as u64;
        w.add_record(b.contents());
    }
    // Overwrite one key in a later record.
    let mut b = WriteBatch::new();
    b.set_sequence(seq);
    b.put(b"k050", b"updated");
    w.add_record(b.contents());

    let log = w.into_bytes();
    let recovered = recover_memtable(&log, icmp(), true).unwrap();
    assert_eq!(recovered.batches, 101);

    let lk = LookupKey::new(b"k050", u64::MAX >> 8);
    assert_eq!(recovered.memtable.get(&lk).unwrap().unwrap(), b"updated");
    let lk0 = LookupKey::new(b"k000", u64::MAX >> 8);
    assert_eq!(recovered.memtable.get(&lk0).unwrap().unwrap(), b"v0");
}

#[test]
fn version_edit_survives_serialisation() {
    let mut edit = VersionEdit::new();
    edit.set_comparator("lignite.BytewiseComparator");
    edit.set_last_sequence(12345);
    edit.set_next_file_number(88);
    edit.add_file(0, 10, 2048, ik(b"aaa"), ik(b"mmm"));
    edit.add_file(1, 11, 4096, ik(b"n"), ik(b"z"));
    edit.delete_file(0, 4);

    let round = VersionEdit::decode(&edit.encode()).unwrap();
    assert_eq!(round.comparator.as_deref(), Some("lignite.BytewiseComparator"));
    assert_eq!(round.last_sequence, Some(12345));
    assert_eq!(round.next_file_number, Some(88));
    assert_eq!(round.new_files.len(), 2);
    assert!(round.deleted_files.contains(&(0, 4)));
}

#[test]
fn picker_triggers_on_l0_file_count() {
    let picker = CompactionPicker::new(icmp());
    let mut v = Version::empty();
    // Below the trigger: no compaction.
    for i in 0..(L0_COMPACTION_TRIGGER - 1) {
        v.files[0].push(file(i as u64, 1024, b"a", b"z"));
    }
    assert!(picker.pick_compaction(&v).is_none());

    // At the trigger: a compaction is proposed at level 0.
    v.files[0].push(file(99, 1024, b"a", b"z"));
    let c = picker.pick_compaction(&v).expect("expected a compaction");
    assert_eq!(c.level, 0);
    assert_eq!(c.output_level(), 1);
    assert_eq!(c.inputs.len(), L0_COMPACTION_TRIGGER);
}

#[test]
fn picker_expands_to_overlapping_next_level_files() {
    let picker = CompactionPicker::new(icmp());
    let mut v = Version::empty();
    // Fill level 0 to the trigger with a wide key range.
    for i in 0..L0_COMPACTION_TRIGGER {
        v.files[0].push(file(i as u64, 1024, b"c", b"h"));
    }
    // Level 1 files, some overlapping [c, h], some not.
    v.files[1].push(file(50, 1, b"a", b"b")); // before
    v.files[1].push(file(51, 1, b"d", b"e")); // overlaps
    v.files[1].push(file(52, 1, b"g", b"k")); // overlaps
    v.files[1].push(file(53, 1, b"x", b"z")); // after

    let c = picker.pick_compaction(&v).unwrap();
    let overlap_numbers: Vec<u64> = c.overlap.iter().map(|f| f.number).collect();
    assert_eq!(overlap_numbers, vec![51, 52]);
    assert_eq!(c.input_file_count(), L0_COMPACTION_TRIGGER + 2);
}
