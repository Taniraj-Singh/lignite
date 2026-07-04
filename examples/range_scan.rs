//! Demonstrates range iteration and seeking over a larger dataset.
//!
//! Run with: `cargo run --example range_scan`

use lignite::{Db, LigniteIterator, Options, ReadOptions, WriteOptions};

fn main() -> lignite::Result<()> {
    let db = Db::open("range-demo", Options::default())?;
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    // Insert 1,000 zero-padded keys and flush them into a single sorted table.
    for i in 0..1_000u32 {
        db.put(&wo, format!("row:{i:05}").as_bytes(), format!("payload-{i}").as_bytes())?;
    }
    db.flush()?;
    println!("inserted 1000 rows across {} file(s)", db.live_file_count());

    // Full scan: count and sanity-check ordering.
    let mut it = db.new_iterator(&ro)?;
    it.seek_to_first();
    let mut count = 0usize;
    let mut prev: Option<Vec<u8>> = None;
    while it.valid() {
        if let Some(p) = &prev {
            assert!(p.as_slice() < it.key(), "scan must be strictly increasing");
        }
        prev = Some(it.key().to_vec());
        count += 1;
        it.next();
    }
    println!("full scan visited {count} rows in order");

    // Seek into the middle of the range and read the next few rows.
    let mut it = db.new_iterator(&ro)?;
    it.seek(b"row:00500");
    println!("scan starting at row:00500:");
    for _ in 0..5 {
        if !it.valid() {
            break;
        }
        println!(
            "  {} = {}",
            String::from_utf8_lossy(it.key()),
            String::from_utf8_lossy(it.value())
        );
        it.next();
    }

    Ok(())
}
