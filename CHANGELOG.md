# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow semantic versioning once it reaches 1.0.

## [0.4.0]

### Added

- **Block compression** (`compress` module): self-contained RLE and LZ77 codecs
  with hardened, bounds-checked decoders. Wired through the SSTable writer and
  reader behind the per-block compression tag and selectable via
  `Options.compression`; defaults to `None`, so existing images are unaffected.
  New `CompressionType::Rle` and `CompressionType::Lz` variants.
- **Optimistic transactions** (`txn` module): `Transaction` with a read-your-writes
  buffer over a snapshot and key-granular conflict validation at commit.
- **Archive backup/restore** (`archive` module): a portable, versioned, checksummed
  byte format, plus `Db::export_archive` / `Db::import_archive` for consistent
  logical backup and cross-instance transfer.
- **`lignite` command-line shell** (`src/bin/lignite.rs`): a line-oriented REPL and
  script runner over the engine.
- New examples: `compression`, `transactions`, `archive_backup`.
- New fuzz targets: `block_codec_fuzzer` and `archive_parse_fuzzer`, registered in
  the ClusterFuzzLite build.

## [0.3.2]

### Added

- `ConcatenatingIterator` for cheap iteration over a single level's
  non-overlapping files.
- `EmptyIterator` placeholder.
- Runtime `Stats` counters exposed via `Db::stats`.
- In-memory `Env` (`MemEnv`) storage abstraction.

### Changed

- The block builder no longer assumes raw-byte key ordering; it relies solely on
  the caller's comparator order and the shared-prefix optimisation.

## [0.3.0]

### Added

- Compaction picker with level scoring and next-level overlap expansion.
- MANIFEST `VersionEdit` encode/decode.
- Write-ahead-log recovery (`recover_memtable`).
- Snapshot registry consulted by compaction.

## [0.2.0]

### Added

- SSTable reader/writer: delta-encoded blocks, two-level index, filter block,
  footer, CRC32C-checked block trailers.
- Sharded LRU cache.
- Bloom filter policy.

## [0.1.0]

### Added

- Initial skeleton: internal-key encoding, skiplist memtable, write batches,
  write-ahead-log framing, the merging iterator and the `Db` facade.
