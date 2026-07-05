#!/bin/bash -eu
# Compile the cargo-fuzz targets and copy the resulting binaries into $OUT.
#
# base-builder-rust ships cargo-fuzz and sets the sanitizer environment; a plain
# `cargo fuzz build` picks up the fuzz crate under ./fuzz.

cd "$SRC/lignite"

cargo fuzz build -O

FUZZ_TARGET_OUTPUT_DIR="$SRC/lignite/fuzz/target/x86_64-unknown-linux-gnu/release"

for target in db_differential_fuzzer sstable_parse_fuzzer wal_parse_fuzzer \
              block_codec_fuzzer archive_parse_fuzzer; do
    cp "$FUZZ_TARGET_OUTPUT_DIR/$target" "$OUT/"
done
