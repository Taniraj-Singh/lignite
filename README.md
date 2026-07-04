# lignite

`lignite` is an embeddable, single-node key/value storage engine written in safe
Rust (`#![forbid(unsafe_code)]`). It is built around a **log-structured merge-tree
(LSM)** in the LevelDB/RocksDB tradition, with a multi-version concurrency-control
(MVCC) read model layered on top.

It is a compact but complete implementation of the moving parts a real LSM engine
needs: a write-ahead log, an in-memory memtable, immutable sorted string tables
(SSTables) with block indexes and bloom filters, a sharded block/table cache,
snapshots, multi-way merge iteration, and background compaction driven by a
level-scoring picker.

> **Scope of this build.** The engine currently runs entirely in memory: SSTable
> images, the write-ahead log and version metadata are kept as in-process
> buffers rather than written to disk. The on-disk representation is fully
> specified and its codecs (WAL framing, SSTable format, MANIFEST `VersionEdit`)
> are implemented and tested; wiring them to a real `Env` is the next step. See
> [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Features

- **Ordered key/value store** with arbitrary byte keys and values.
- **Atomic write batches** — puts and deletes applied together and logged as one
  record.
- **MVCC snapshots** — a read observes a consistent point-in-time view; concurrent
  writes are invisible to an existing snapshot.
- **Range iteration** — forward scans that collapse each key's version chain to
  the value visible at the read's snapshot and skip deletions.
- **SSTables** — delta-encoded data blocks, a two-level index, per-block CRC32C
  checksums, and configurable bloom filters.
- **Compaction** — merges overlapping runs, drops superseded versions, and
  discards tombstones once they reach the bottom level.
- **Pluggable comparator and filter policy**.

## Quick start

```rust
use lignite::{Db, Options, ReadOptions, WriteOptions};

fn main() -> lignite::Result<()> {
    let db = Db::open("mydb", Options::default())?;
    let wo = WriteOptions::default();
    let ro = ReadOptions::default();

    db.put(&wo, b"alpha", b"1")?;
    db.put(&wo, b"beta", b"2")?;
    db.delete(&wo, b"alpha")?;

    assert_eq!(db.get(&ro, b"alpha")?, None);
    assert_eq!(db.get(&ro, b"beta")?.as_deref(), Some(&b"2"[..]));
    Ok(())
}
```

Run the worked examples:

```
cargo run --example kv_demo
cargo run --example range_scan
```

## Architecture at a glance

```
        put / delete / write(batch)
                   │
                   ▼
           ┌───────────────┐   append   ┌──────────────────┐
           │  write-ahead  │◀───────────│   Db::write      │
           │     log       │            └──────────────────┘
           └───────────────┘                   │ apply
                                                ▼
                                        ┌───────────────┐
                                        │   memtable    │  (skiplist)
                                        └───────────────┘
                                                │ flush (when full)
                                                ▼
     level 0   [sst] [sst] [sst]  ← overlapping, newest-first
     level 1   [ sst ][ sst ][ sst ]        ┐
     level 2   [  sst  ][  sst  ] ...        │ non-overlapping, sorted
       ...                                   ┘   (produced by compaction)
```

A read merges the memtable, any memtables awaiting flush, and the relevant
SSTables through a single k-way merge, returning the newest version of the key at
or below the read's sequence number.

## Module map

| Module | Responsibility |
|---|---|
| `util` | integer coding, CRC32C, hashing, PRNG |
| `key` | internal-key encoding and the MVCC ordering it induces |
| `comparator` | user-key ordering |
| `memtable` | skiplist-backed in-memory write buffer |
| `wal` | write-ahead log record framing, writer and reader |
| `sstable` | block/table builders and readers, footer, filter block |
| `filter` | bloom filter policy |
| `cache` | sharded LRU table/block cache |
| `iterator` | the `InternalIterator` trait, merge/concat/empty iterators |
| `version` | file metadata, versions, the version set, MANIFEST edits |
| `compaction` | compaction execution and the level-scoring picker |
| `batch` | write-batch encoding |
| `snapshot` | snapshot handles and the live-snapshot registry |
| `env` | storage environment abstraction (in-memory implementation) |
| `db` | the public `Db` facade tying it all together |

## Building and testing

```
cargo build
cargo test
```

The unit tests live alongside each module; end-to-end behaviour is covered by the
integration tests under [`tests/`](tests/).

### Differential and fuzz testing

`tests/differential_fuzz.rs` runs deterministic, fixed-seed campaigns that drive
the database with random puts/deletes/flushes/compactions and check every read
and full scan against an in-memory `BTreeMap` oracle. These run as part of
`cargo test`.

A [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) harness lives in
[`fuzz/`](fuzz/) (a standalone crate, so it does not affect `cargo build`). It
requires a nightly toolchain:

```
cargo install cargo-fuzz
cargo +nightly fuzz run differential      # differential oracle over an op stream
cargo +nightly fuzz run table_roundtrip   # SSTable build/read roundtrip
```

## Configuration

`Options` exposes the usual tunables — `write_buffer_size`, `block_size`,
`block_restart_interval`, `bloom_bits_per_key`, `block_cache_capacity`,
`max_file_size` and the `comparator`. See the `options` module for defaults.

## License

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
