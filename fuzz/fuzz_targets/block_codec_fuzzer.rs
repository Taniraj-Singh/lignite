//! Robustness and round-trip fuzz target for the block compression codecs.
//!
//! Two invariants are checked on every input:
//!
//! 1. **Decompression is safe on arbitrary bytes.** The raw input is handed
//!    directly to each codec's decoder with a fixed output bound. A correct
//!    decoder returns a clean `Err` on malformed data — never a panic, an
//!    out-of-bounds read, an unbounded allocation, or a non-terminating loop.
//!
//! 2. **Compression round-trips.** The input is treated as a block, compressed
//!    with each codec, and decompressed again; the result must equal the original.
//!
//! Build/run: `cargo +nightly fuzz run block_codec_fuzzer`

#![no_main]

use libfuzzer_sys::fuzz_target;
use lignite::compress::{lz, rle};

const BOUND: usize = 8 << 20; // 8 MiB output ceiling for the decoders

fuzz_target!(|data: &[u8]| {
    // (1) Decoders must tolerate arbitrary input.
    let _ = rle::decompress(data, BOUND);
    let _ = lz::decompress(data, BOUND);

    // (2) Compression must round-trip for any input.
    let rle_packed = rle::compress(data);
    let rle_back = rle::decompress(&rle_packed, BOUND).expect("rle roundtrip decodes");
    assert_eq!(rle_back, data, "rle round-trip mismatch");

    let lz_packed = lz::compress(data);
    let lz_back = lz::decompress(&lz_packed, BOUND).expect("lz roundtrip decodes");
    assert_eq!(lz_back, data, "lz round-trip mismatch");
});
