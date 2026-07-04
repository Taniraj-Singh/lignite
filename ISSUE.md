# Bug: reads can return a stale value after a key is overwritten

- **Component:** storage engine (read path)
- **Severity:** high — returns incorrect data / silent data loss
- **Status:** open
- **Reproducibility:** deterministic (100%)

## Summary

After a key has been written, persisted to an SSTable, and then written again,
`Db::get` sometimes returns the **older** value instead of the most recent one.
The same underlying misbehaviour shows up in three related ways:

1. `get` returns a previous value for a key that was overwritten.
2. A key that was deleted reappears with an old value (the delete is silently
   undone).
3. A range scan yields stale values for affected keys.

The corruption is not transient — it survives compaction, so once it is observed
the wrong value can become the permanent, only value for the key.

## Environment

- `lignite` 0.3.2
- `rustc` stable
- Default `Options`.

## Steps to reproduce

The behaviour reproduces whenever two writes to the same key end up in two
different persisted tables (for example: write, flush, write again, flush again).

```rust
use lignite::{Db, Options, ReadOptions, WriteOptions};

let db = Db::open("repro", Options::default()).unwrap();
let wo = WriteOptions::default();
let ro = ReadOptions::default();

db.put(&wo, b"account:42", b"balance=100").unwrap();
db.flush().unwrap();                       // first value persisted

db.put(&wo, b"account:42", b"balance=250").unwrap();
db.flush().unwrap();                       // second value persisted

let got = db.get(&ro, b"account:42").unwrap();
println!("{:?}", got.as_deref());          // observed: Some(b"balance=100")
                                           // expected: Some(b"balance=250")
```

## Expected behaviour

A read returns the value of the **most recent** write to the key that is visible
at the read's sequence number (or its snapshot). A deleted key reads back as
absent.

## Actual behaviour

For keys whose versions are spread across more than one persisted table, the read
can resolve to an older version:

- overwrite → old value returned;
- delete-after-write → old value resurrected;
- scans and post-compaction reads → same stale result.

Keys whose versions all live in a single table (e.g. multiple writes followed by
a single flush) are **not** affected, which is why the defect is easy to miss in
casual testing.

## Reproduction in the test suite

Three integration suites encode the expected behaviour and currently fail:

- [`tests/mvcc_visibility.rs`](tests/mvcc_visibility.rs) — focused scenarios (5
  tests);
- [`tests/poc_hard.rs`](tests/poc_hard.rs) — interleaved multi-key, three-way
  overlap, compaction and snapshot-crossing scenarios (7 tests);
- [`tests/differential_fuzz.rs`](tests/differential_fuzz.rs) — deterministic
  randomised campaigns checked against a `BTreeMap` oracle (4 tests).

```
cargo test --test mvcc_visibility --test poc_hard --test differential_fuzz
```

```
test result: FAILED. mvcc_visibility     0 passed; 5 failed
test result: FAILED. poc_hard            0 passed; 7 failed
test result: FAILED. differential_fuzz   0 passed; 4 failed
```

All other tests (`cargo test --lib`, `--test basic_kv`,
`--test recovery_and_layout`, `--test iterator_semantics`) pass, and the failures
all trace back to a single defect: fixing that one place turns every failing test
green with no other change.

## Impact

Any workload that updates or deletes existing keys and lets those updates persist
across separate tables can read back stale or deleted data. This is a correctness
bug, not merely a performance issue.

## Acceptance criteria

- `cargo test` passes in full (currently 16 tests across the three suites above
  fail).
- The rest of the suite continues to pass.
- No change to the on-disk formats or the public API.
