# Proof-of-concept crash inputs

These files are crashing inputs for the fuzz targets under
[`../fuzz/`](../fuzz/). Feed one to its target to reproduce the crash:

```
cargo +nightly fuzz run db_differential_fuzzer poc/db_differential_fuzzer_crash
```

or, once the ClusterFuzzLite image is built, run the target binary directly on
the file.

## `db_differential_fuzzer_crash`

- **Target:** `db_differential_fuzzer`
- **Size:** 8 bytes (`00 00 03 00 00 00 03 00`)
- **Effect:** the harness performs `put k00` → `flush` → `put k00` (new value) →
  `flush`, then reads `k00` back. The two writes now live in two separate
  persisted tables; the read returns the **older** value instead of the newer
  one, so the differential assertion against the oracle fails and the process
  aborts.

This is the minimal reproduction: two versions of one key in two tables plus one
read. Longer inputs surface the same class of divergence through deletes,
interleaved keys and compaction.
