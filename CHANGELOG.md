# Changelog

All notable changes to this project are documented here. The format is loosely
based on [Keep a Changelog](https://keepachangelog.com/), and the project aims to
follow semantic versioning once it reaches 1.0.

## [Unreleased]

### Known issues

- Reads can return a stale value for a key whose versions are spread across
  multiple persisted tables. See [`ISSUE.md`](ISSUE.md). Tracked by the failing
  regression tests in `tests/mvcc_visibility.rs`.

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
