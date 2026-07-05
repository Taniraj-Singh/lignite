//! Demonstrates logical backup and restore through the archive format.
//!
//! Run with: `cargo run --example archive_backup`
//!
//! A database is populated and exported to a single self-describing archive byte
//! string (checksummed, optionally compressed). A brand-new database then imports
//! that archive and is shown to hold exactly the same data. Because the export is
//! taken at a snapshot, it captures a consistent point-in-time view.

use lignite::archive::{ArchiveReader, ArchiveRecord};
use lignite::{Db, Options, ReadOptions, WriteOptions};

fn main() -> lignite::Result<()> {
    let source = Db::open("archive-src", Options::default())?;
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    println!("== populate the source database ==");
    let rows = [
        ("city:oslo", "population=700000"),
        ("city:kyoto", "population=1460000"),
        ("city:cairo", "population=9500000"),
        ("city:lima", "population=8900000"),
    ];
    for (k, v) in rows {
        source.put(&wo, k.as_bytes(), v.as_bytes())?;
    }

    println!("== export to a compressed archive ==");
    let archive = source.export_archive(&ro, true)?;
    println!("  archive is {} bytes", archive.len());

    // The archive is a plain byte string: it can be inspected without a database.
    let records = ArchiveReader::parse(&archive)?;
    println!("  it holds {} records:", records.len());
    for r in &records {
        match r {
            ArchiveRecord::Put(k, v) => println!(
                "    put {} = {}",
                String::from_utf8_lossy(k),
                String::from_utf8_lossy(v)
            ),
            ArchiveRecord::Delete(k) => println!("    del {}", String::from_utf8_lossy(k)),
        }
    }

    println!("== restore into a fresh database ==");
    let restored = Db::open("archive-dst", Options::default())?;
    let n = restored.import_archive(&wo, &archive)?;
    println!("  imported {n} records");

    println!("== verify the restore ==");
    for (k, expected) in rows {
        let got = restored.get(&ro, k.as_bytes())?;
        let ok = got.as_deref() == Some(expected.as_bytes());
        println!("  {k}: {}", if ok { "match" } else { "MISMATCH" });
        assert!(ok);
    }

    println!("== a corrupted archive is rejected, not misread ==");
    let mut damaged = archive.clone();
    let mid = damaged.len() / 2;
    damaged[mid] ^= 0xff;
    match ArchiveReader::parse(&damaged) {
        Ok(_) => println!("  parsed (unexpected)"),
        Err(e) => println!("  rejected: {e}"),
    }

    println!("done.");
    Ok(())
}
