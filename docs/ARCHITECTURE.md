# Architecture

This document describes how `lignite` is put together: the data structures, how a
write becomes durable, how a read is resolved, and how compaction keeps the tree
healthy. It complements the module-level documentation in the source.

## The big picture

`lignite` is a log-structured merge-tree. Writes are cheap and append-only; the
cost of keeping data sorted is deferred to background compaction. The engine has
three tiers of storage:

1. **The memtable** — a sorted, in-memory structure that absorbs recent writes.
2. **Immutable memtables** — memtables that have filled up and are queued for
   flushing.
3. **SSTables** — immutable, sorted, on-disk (here, in-memory) files organised
   into levels.

Every mutation is tagged with a globally increasing **sequence number**, and a
read is performed as of some sequence number. This is the basis of the MVCC
model: a read never sees a write with a higher sequence number than its own.

## Keys: user keys and internal keys

Callers deal in **user keys** (arbitrary bytes). Internally every record is stored
under an **internal key**:

```
internal_key = user_key ++ fixed64_le( (sequence << 8) | value_type )
```

The trailing eight bytes pack the record's sequence number and a tag
distinguishing a live value from a deletion tombstone. Internal keys are ordered
first by user key (ascending, per the configured comparator) and then, for a
shared user key, so that the record with the **larger sequence number sorts
first**. That ordering is what lets a lookup for a user key walk its versions from
newest to oldest and stop at the first one visible to its snapshot.

See [`docs/FORMAT.md`](FORMAT.md) for the exact byte layouts.

## The write path

`Db::write` takes a `WriteBatch` (one or more puts/deletes) and:

1. assigns the batch a contiguous run of sequence numbers;
2. appends the batch's serialised bytes to the **write-ahead log** — this is the
   durability point;
3. applies each operation to the active memtable;
4. advances the database's last-used sequence number.

When the memtable's estimated size crosses `write_buffer_size` it is frozen (made
immutable) and a fresh memtable takes its place. Frozen memtables are flushed into
level-0 SSTables.

`put` and `delete` are thin wrappers that build a one-operation batch.

## The memtable

The memtable is a **skiplist** keyed by internal key. The implementation is
index-based — every node lives in a single growable arena and links are integer
indices rather than pointers — so it needs no `unsafe`. A stored entry is:

```
varint(internal_key_len) internal_key varint(value_len) value
```

Because records are never mutated in place, an overwrite or delete of a key is
simply a new entry with a higher sequence number; the skiplist ordering keeps the
newest one first among a key's versions.

## SSTables

A memtable flush and a compaction both produce an SSTable via `TableBuilder`. A
table is a stack of blocks:

- **data blocks** hold the records, delta-encoded against a periodic *restart*
  point so shared key prefixes are stored once;
- a **filter block** holds one bloom filter per ~2 KiB of data, so a point lookup
  can skip a block that certainly lacks the key;
- a **metaindex block** maps meta-block names to their locations;
- an **index block** maps each data block's maximum key to its handle;
- a fixed **footer** pins the metaindex and index and carries a magic number.

Every block is stored with a one-byte compression tag and a CRC32C, so corruption
is detected on read. Reading a table is a two-level iteration: walk the index
block, and for each entry read and iterate the referenced data block.

## Reads

A read resolves a user key at a sequence number (the latest, or a snapshot's). It
consults sources newest-first:

1. the active memtable;
2. each immutable memtable awaiting flush;
3. the SSTables.

For the tables, every candidate file contributes a child iterator to a single
**k-way merge**, and the merge yields the sorted union of all runs. The read seeks
to the lookup key and takes the newest version of the key at or below its
sequence number, returning the value or — for a tombstone — "not found".

Range scans use the same merge, wrapped in a layer that collapses each user key's
version chain down to the single value visible at the snapshot and skips deleted
keys.

## Snapshots

A `Snapshot` pins a sequence number and registers it with a live-snapshot
registry. Reads carrying the snapshot observe exactly the state at that sequence.
Compaction consults the oldest live snapshot so it never discards a version some
snapshot can still read.

## Versions and the version set

The set of live files is a `Version`: a list of files per level. The `VersionSet`
owns the current version, hands out file and sequence numbers, and caches parsed
table handles. Changes to the file set (a flush's new file, a compaction's
deletions and additions) are expressed as a `VersionEdit`; the on-disk engine
appends these to a MANIFEST log and replays them on open to reconstruct the
current version.

## Compaction

As level-0 accumulates overlapping files, reads get slower (every level-0 file may
hold the key). Compaction rewrites input files into fewer, larger,
non-overlapping outputs and drops records no snapshot can observe:

- a version superseded by a newer one that every snapshot can see;
- a deletion tombstone once it reaches the bottom level, where nothing older
  survives beneath it.

The **compaction picker** decides *what* to compact by scoring each level — level
0 by file count, deeper levels by total bytes against a geometrically growing
budget — and selecting a seed file plus the overlapping files at the next level.

## Iterators

All ordered sources implement one trait, `InternalIterator`, yielding internal
keys in ascending order. This uniformity is what makes composition trivial:

- `MergingIterator` — a k-way merge of arbitrary children (memtables + tables);
- `ConcatenatingIterator` — a cheap chain over the non-overlapping files of a
  single level;
- `EmptyIterator` — a placeholder yielding nothing.

The public `DbIterator` sits on top of a merge and presents plain user key/value
pairs at a snapshot.

## Safety and testing

The crate is `#![forbid(unsafe_code)]`. Each module carries unit tests; end-to-end
behaviour — persistence, snapshots, iteration, recovery and compaction — is
covered by the integration tests in [`tests/`](../tests).
