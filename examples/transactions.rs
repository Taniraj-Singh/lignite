//! Demonstrates optimistic transactions.
//!
//! Run with: `cargo run --example transactions`
//!
//! Two transactions race on the same key. The first reads it, then a second
//! transaction commits a change to it; when the first tries to commit, optimistic
//! validation detects that what it read has moved and rejects the commit. A retry
//! against a fresh snapshot then succeeds. A pair of transactions on disjoint keys
//! both commit without interference.

use lignite::{Db, Options, ReadOptions, Transaction, WriteOptions};

fn main() -> lignite::Result<()> {
    let db = Db::open("txn-demo", Options::default())?;
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"counter", b"0")?;

    println!("== a read-modify-write conflict ==");
    // t1 reads the counter, intending to increment it.
    let mut t1 = Transaction::begin(&db);
    let seen = t1.get(b"counter")?.unwrap();
    println!("  t1 read counter = {}", String::from_utf8_lossy(&seen));
    t1.put(b"counter", b"1");

    // Meanwhile t2 commits its own increment.
    let mut t2 = Transaction::begin(&db);
    t2.put(b"counter", b"100");
    t2.commit(&wo)?;
    println!("  t2 committed counter = 100");

    // t1's commit now fails: the value it read has changed underneath it.
    match t1.commit(&wo) {
        Ok(()) => println!("  t1 committed (unexpected)"),
        Err(e) => println!("  t1 commit rejected: {e}"),
    }
    println!(
        "  counter is now {}",
        String::from_utf8_lossy(&db.get(&ro, b"counter")?.unwrap())
    );

    println!("== retry against a fresh snapshot ==");
    let mut retry = Transaction::begin(&db);
    let current: u32 = String::from_utf8_lossy(&retry.get(b"counter")?.unwrap())
        .parse()
        .unwrap();
    retry.put(b"counter", (current + 1).to_string().as_bytes());
    retry.commit(&wo)?;
    println!(
        "  retry committed counter = {}",
        String::from_utf8_lossy(&db.get(&ro, b"counter")?.unwrap())
    );

    println!("== disjoint transactions never conflict ==");
    let mut a = Transaction::begin(&db);
    let mut b = Transaction::begin(&db);
    a.get(b"x")?;
    a.put(b"x", b"1");
    b.get(b"y")?;
    b.put(b"y", b"2");
    a.commit(&wo)?;
    b.commit(&wo)?;
    println!(
        "  x = {}, y = {}",
        String::from_utf8_lossy(&db.get(&ro, b"x")?.unwrap()),
        String::from_utf8_lossy(&db.get(&ro, b"y")?.unwrap())
    );

    println!("done.");
    Ok(())
}
