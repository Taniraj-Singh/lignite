//! A byte-oriented run-length codec.
//!
//! Run-length encoding is the cheapest useful block compressor: it collapses a
//! run of one repeated byte into a short header, and passes incompressible spans
//! through as *literal* runs. It earns its keep on the kind of data an SSTable
//! actually holds — long shared key prefixes that the block builder has already
//! zeroed out, padding, and low-entropy values — while never inflating a block by
//! more than a few percent in the worst case.
//!
//! ## Wire format
//!
//! The compressed stream is a sequence of *runs*. Each run begins with a varint
//! header whose least-significant bit selects the run kind and whose remaining
//! bits carry a length:
//!
//! ```text
//! header := varint( (count << 1) | kind )
//! run    := header  payload
//! ```
//!
//! * `kind = 0` (**literal**): `count` raw bytes follow and are emitted verbatim.
//! * `kind = 1` (**repeat**): one byte follows and is emitted `count` times.
//!
//! `count` is always at least one, so the empty run is unrepresentable and a
//! decoder can treat `count == 0` as corruption. The decoder never trusts a
//! length: every payload read is bounds-checked against the remaining input and
//! every emission is bounds-checked against `max_output`, so an arbitrary byte
//! string decodes to either a bounded buffer or a clean [`Error::Corruption`] —
//! never a panic or an unbounded allocation.

use crate::error::{Error, Result};
use crate::util::coding::{decode_varint64, put_varint64};

/// A run of at least this many identical bytes is worth encoding as a repeat.
///
/// Below this a repeat header (one or two bytes plus the value byte) would not
/// beat simply copying the bytes into a literal run.
const MIN_REPEAT: usize = 3;

/// Compress `src` with run-length encoding.
///
/// The result round-trips through [`decompress`] for any input. It is never more
/// than a small constant factor larger than `src`; callers that require a strict
/// size win should compare lengths and fall back to storing the block verbatim.
pub fn compress(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 2 + 8);
    let mut i = 0;
    // Start of the pending literal run that has not yet been flushed.
    let mut literal_start = 0;

    while i < src.len() {
        let run_len = repeat_run_length(src, i);
        if run_len >= MIN_REPEAT {
            flush_literals(&mut out, &src[literal_start..i]);
            emit_repeat(&mut out, src[i], run_len);
            i += run_len;
            literal_start = i;
        } else {
            i += 1;
        }
    }
    flush_literals(&mut out, &src[literal_start..]);
    out
}

/// Length of the run of bytes equal to `src[at]` starting at `at`.
fn repeat_run_length(src: &[u8], at: usize) -> usize {
    let b = src[at];
    let mut j = at + 1;
    while j < src.len() && src[j] == b {
        j += 1;
    }
    j - at
}

/// Emit a repeat run, splitting runs longer than a single header can address.
fn emit_repeat(out: &mut Vec<u8>, byte: u8, mut count: usize) {
    while count > 0 {
        let chunk = count.min(MAX_RUN);
        put_varint64(out, ((chunk as u64) << 1) | 1);
        out.push(byte);
        count -= chunk;
    }
}

/// Flush a literal span as one or more literal runs.
fn flush_literals(out: &mut Vec<u8>, literals: &[u8]) {
    let mut rest = literals;
    while !rest.is_empty() {
        let chunk = rest.len().min(MAX_RUN);
        put_varint64(out, (chunk as u64) << 1);
        out.extend_from_slice(&rest[..chunk]);
        rest = &rest[chunk..];
    }
}

/// Upper bound on a single run's `count`, keeping headers to five varint bytes
/// and giving the decoder a cheap sanity ceiling.
const MAX_RUN: usize = 1 << 24;

/// Decompress an RLE stream, refusing to produce more than `max_output` bytes.
///
/// Returns [`Error::Corruption`] if the stream is truncated, contains a
/// zero-length run, or would expand past `max_output`.
pub fn decompress(src: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        let (header, tail) =
            decode_varint64(rest).ok_or_else(|| Error::corruption("rle: truncated header"))?;
        let kind = header & 1;
        let count = (header >> 1) as usize;
        if count == 0 {
            return Err(Error::corruption("rle: zero-length run"));
        }
        if out.len().saturating_add(count) > max_output {
            return Err(Error::corruption("rle: output exceeds bound"));
        }
        if kind == 1 {
            // Repeat run: one value byte follows.
            let byte = *tail.first().ok_or_else(|| Error::corruption("rle: missing repeat byte"))?;
            out.resize(out.len() + count, byte);
            rest = &tail[1..];
        } else {
            // Literal run: `count` bytes follow.
            if tail.len() < count {
                return Err(Error::corruption("rle: truncated literal run"));
            }
            out.extend_from_slice(&tail[..count]);
            rest = &tail[count..];
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 1 << 20;

    fn roundtrip(bytes: &[u8]) {
        let packed = compress(bytes);
        let back = decompress(&packed, CAP).expect("decompress");
        assert_eq!(back, bytes);
    }

    #[test]
    fn roundtrips_empty() {
        roundtrip(b"");
    }

    #[test]
    fn roundtrips_single_byte() {
        roundtrip(b"x");
    }

    #[test]
    fn roundtrips_long_repeat() {
        let data = vec![0xabu8; 5000];
        roundtrip(&data);
        // A 5000-byte run must compress dramatically.
        assert!(compress(&data).len() < 32);
    }

    #[test]
    fn roundtrips_literals() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        roundtrip(&data);
    }

    #[test]
    fn roundtrips_mixed() {
        let mut data = Vec::new();
        data.extend_from_slice(b"header----");
        data.extend(std::iter::repeat(b'\0').take(300));
        data.extend_from_slice(b"payload");
        data.extend(std::iter::repeat(b'z').take(64));
        roundtrip(&data);
    }

    #[test]
    fn respects_output_bound() {
        let data = vec![7u8; 10_000];
        let packed = compress(&data);
        assert!(decompress(&packed, 9_999).is_err());
        assert!(decompress(&packed, 10_000).is_ok());
    }

    #[test]
    fn rejects_zero_length_run() {
        // header = 0 => literal run of length 0.
        assert!(decompress(&[0x00], CAP).is_err());
    }

    #[test]
    fn rejects_truncated_literal() {
        // header = literal length 4, but no payload bytes follow.
        assert!(decompress(&[0x08], CAP).is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // The decoder must terminate cleanly (Ok or Err) on any input.
        for seed in 0u64..512 {
            let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15);
            let len = (x % 40) as usize;
            let mut bytes = Vec::with_capacity(len);
            for _ in 0..len {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                bytes.push(x as u8);
            }
            let _ = decompress(&bytes, CAP);
        }
    }
}
