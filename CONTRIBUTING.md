# Contributing to lignite

Thanks for your interest in improving `lignite`.

## Development

```
cargo build
cargo test
cargo clippy --all-targets
cargo fmt
```

The crate is `#![forbid(unsafe_code)]`; contributions must stay within safe Rust.

## Guidelines

- **Tests.** Every behavioural change needs a test. Unit tests live in a
  `#[cfg(test)] mod tests` at the bottom of the module they cover; cross-cutting
  behaviour goes in an integration test under `tests/`.
- **Formats are contracts.** The on-disk encodings in [`docs/FORMAT.md`](docs/FORMAT.md)
  must not change without a format-version bump and a migration story. This
  includes the internal-key layout, the WAL framing, the SSTable block format and
  the MANIFEST `VersionEdit` tags.
- **Ordering is central to correctness.** If you touch anything that compares
  keys, cover it with tests and run the full suite; ordering code is easy to get
  subtly wrong.
- **No `unsafe`.** If you think you need it, open an issue first.
- **Keep modules focused.** Prefer a new small module over growing an existing one
  past its single responsibility.

## Commit messages

Write imperative, present-tense summaries ("add bloom filter policy", not "added").
Reference an issue number when one exists.

## Reporting bugs

Please include a minimal reproduction, the observed and expected behaviour, and
the `lignite` and `rustc` versions. A failing test is the ideal bug report.
