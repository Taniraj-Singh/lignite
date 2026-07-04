//! A small end-to-end demonstration of the `lignite` API.
//!
//! Run with: `cargo run --example kv_demo`
//!
//! It exercises the write path, snapshots, range iteration, flushing to SSTables
//! and manual compaction, printing what it observes — a simple walk through the
//! public surface.

use lignite::{Db, LigniteIterator, Options, ReadOptions, WriteOptions};

fn main() -> lignite::Result<()> {
    let options = Options {
        // A tiny write buffer so the demo actually spills to SSTables.
        write_buffer_size: 4 * 1024,
        ..Options::default()
    };
    let db = Db::open("demo", options)?;
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    println!("== populating ==");
    let fruits = [
        ("apple", "red"),
        ("banana", "yellow"),
        ("cherry", "crimson"),
        ("date", "brown"),
        ("elderberry", "purple"),
    ];
    for (k, v) in fruits {
        db.put(&wo, k.as_bytes(), v.as_bytes())?;
        println!("  put {k} = {v}");
    }

    println!("== point lookups ==");
    for k in ["apple", "date", "missing"] {
        match db.get(&ro, k.as_bytes())? {
            Some(v) => println!("  {k} -> {}", String::from_utf8_lossy(&v)),
            None => println!("  {k} -> <absent>"),
        }
    }

    println!("== snapshot isolation ==");
    let snap = db.snapshot();
    db.put(&wo, b"apple", b"green")?;
    let snap_ro = ReadOptions {
        snapshot: Some(snap),
        ..Default::default()
    };
    println!(
        "  apple @snapshot = {}",
        String::from_utf8_lossy(&db.get(&snap_ro, b"apple")?.unwrap())
    );
    println!(
        "  apple @latest   = {}",
        String::from_utf8_lossy(&db.get(&ro, b"apple")?.unwrap())
    );

    println!("== delete a key ==");
    db.delete(&wo, b"banana")?;

    println!("== flush to SSTables ==");
    db.flush()?;
    println!("  live files: {}", db.live_file_count());

    println!("== range scan ==");
    let mut it = db.new_iterator(&ro)?;
    it.seek_to_first();
    while it.valid() {
        println!(
            "  {} = {}",
            String::from_utf8_lossy(it.key()),
            String::from_utf8_lossy(it.value())
        );
        it.next();
    }

    println!("== compact ==");
    let stats = db.compact()?;
    println!(
        "  compacted {} input files -> {} output file(s); {} records kept, {} dropped",
        stats.input_files, stats.output_files, stats.records_written, stats.records_dropped
    );
    println!(
        "  banana after delete+compact: {:?}",
        db.get(&ro, b"banana")?.map(|v| String::from_utf8_lossy(&v).into_owned())
    );

    println!("done.");
    Ok(())
}
