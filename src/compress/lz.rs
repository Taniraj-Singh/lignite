//! A minimal LZ77 block codec with a hash-chain match finder.
//!
//! This is the general-purpose compressor of the pair: where [`super::rle`] only
//! catches runs of one byte, LZ77 replaces any repeated *substring* seen earlier
//! in the block with a short `(length, distance)` back-reference. SSTable data
//! blocks are full of such repetition — recurring key suffixes, shared value
//! shapes, structural bytes — so the win is real without pulling in an external
//! dependency.
//!
//! ## Wire format
//!
//! The stream is a sequence of *tokens*. Each token is a varint header whose
//! least-significant bit selects its kind:
//!
//! ```text
//! literal := varint(len << 1 | 0)              len raw bytes
//! match   := varint((len - MIN_MATCH) << 1 | 1)  varint(distance)
//! ```
//!
//! A **literal** token copies `len` bytes straight from the stream. A **match**
//! token copies `len` bytes from `distance` bytes before the current output
//! position; `distance` is one-based (`1` is the previous byte) and the copy is
//! defined byte-by-byte, so an overlapping copy (`distance < len`) legally repeats
//! a short pattern — the standard LZ77 idiom the encoder relies on.
//!
//! ## Safety of the decoder
//!
//! The decoder treats its input as hostile. Every `distance` is checked against
//! the number of bytes produced so far, every `len` against `max_output`, and
//! every varint against the remaining input. A crafted token can therefore only
//! yield a bounded buffer or an [`Error::Corruption`]; it can never read out of
//! bounds, allocate without limit, or loop forever.

use crate::error::{Error, Result};
use crate::util::coding::{decode_varint64, put_varint64};

/// Shortest back-reference worth encoding. A match must beat the ~2-3 bytes a
/// literal run would spend on the same span, so three is the practical floor.
const MIN_MATCH: usize = 4;

/// Longest single match the encoder will emit. Longer repeats are split across
/// several match tokens.
const MAX_MATCH: usize = 1 << 20;

/// Size of the sliding window; a match's distance never exceeds this.
const WINDOW: usize = 1 << 16;

/// Number of hash-chain links the encoder follows before giving up on a longer
/// match. A small ceiling keeps compression O(n) on adversarial inputs.
const MAX_CHAIN: usize = 32;

/// Number of buckets in the match-finder hash table (a power of two).
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// Hash the four bytes at `p` into a bucket index.
#[inline]
fn hash4(bytes: &[u8], p: usize) -> usize {
    let x = u32::from_le_bytes([bytes[p], bytes[p + 1], bytes[p + 2], bytes[p + 3]]);
    // Fibonacci hashing: multiply and take the high bits.
    ((x.wrapping_mul(2_654_435_761)) >> (32 - HASH_BITS)) as usize
}

/// Compress `src` with LZ77.
///
/// The output round-trips through [`decompress`] for any input. As with RLE the
/// encoder never guarantees a size reduction; the block writer compares lengths
/// and stores the block verbatim when compression does not pay.
pub fn compress(src: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / 2 + 16);
    let n = src.len();
    if n < MIN_MATCH {
        // Too short to hold even one match: a single literal run.
        emit_literals(&mut out, src);
        return out;
    }

    // head[h] = most recent position hashing to h, or NONE.
    // prev[i] = position before i that shared i's hash (a chain).
    const NONE: u32 = u32::MAX;
    let mut head = vec![NONE; HASH_SIZE];
    let mut prev = vec![NONE; n];

    let mut i = 0;
    let mut literal_start = 0;
    while i + MIN_MATCH <= n {
        let h = hash4(src, i);
        let (mut best_len, mut best_dist) = (0usize, 0usize);

        // Walk the chain for this hash bucket, newest first.
        let mut cand = head[h];
        let mut chain = 0;
        while cand != NONE && chain < MAX_CHAIN {
            let cpos = cand as usize;
            let dist = i - cpos;
            if dist > WINDOW {
                break; // past the window; older links only get further away
            }
            let len = match_length(src, cpos, i, n);
            if len > best_len {
                best_len = len;
                best_dist = dist;
                if len >= MAX_MATCH {
                    break;
                }
            }
            cand = prev[cpos];
            chain += 1;
        }

        // Record i in the hash chain before we advance.
        prev[i] = head[h];
        head[h] = i as u32;

        if best_len >= MIN_MATCH {
            emit_literals(&mut out, &src[literal_start..i]);
            emit_match(&mut out, best_len, best_dist);
            // Insert positions covered by the match so future matches can find
            // them, then skip past the match.
            let end = i + best_len;
            let mut j = i + 1;
            while j + MIN_MATCH <= n && j < end {
                let hj = hash4(src, j);
                prev[j] = head[hj];
                head[hj] = j as u32;
                j += 1;
            }
            i = end;
            literal_start = i;
        } else {
            i += 1;
        }
    }
    emit_literals(&mut out, &src[literal_start..]);
    out
}

/// Length of the common run of `src[a..]` and `src[b..]`, capped at [`MAX_MATCH`].
fn match_length(src: &[u8], a: usize, b: usize, n: usize) -> usize {
    let mut len = 0;
    while b + len < n && len < MAX_MATCH && src[a + len] == src[b + len] {
        len += 1;
    }
    len
}

/// Emit a literal token for `bytes` (splitting oversized spans), or nothing when
/// empty.
fn emit_literals(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut rest = bytes;
    while !rest.is_empty() {
        let chunk = rest.len().min(MAX_MATCH);
        put_varint64(out, (chunk as u64) << 1);
        out.extend_from_slice(&rest[..chunk]);
        rest = &rest[chunk..];
    }
}

/// Emit a match token.
fn emit_match(out: &mut Vec<u8>, len: usize, dist: usize) {
    debug_assert!(len >= MIN_MATCH);
    debug_assert!(dist >= 1);
    put_varint64(out, (((len - MIN_MATCH) as u64) << 1) | 1);
    put_varint64(out, dist as u64);
}

/// Decompress an LZ77 stream, refusing to produce more than `max_output` bytes.
pub fn decompress(src: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut rest = src;
    while !rest.is_empty() {
        let (header, tail) =
            decode_varint64(rest).ok_or_else(|| Error::corruption("lz: truncated token"))?;
        if header & 1 == 0 {
            // Literal token.
            let len = (header >> 1) as usize;
            if len == 0 {
                return Err(Error::corruption("lz: zero-length literal"));
            }
            if tail.len() < len {
                return Err(Error::corruption("lz: truncated literal payload"));
            }
            if out.len().saturating_add(len) > max_output {
                return Err(Error::corruption("lz: output exceeds bound"));
            }
            out.extend_from_slice(&tail[..len]);
            rest = &tail[len..];
        } else {
            // Match token.
            let len = (header >> 1) as usize + MIN_MATCH;
            let (dist, tail2) =
                decode_varint64(tail).ok_or_else(|| Error::corruption("lz: truncated distance"))?;
            let dist = dist as usize;
            if dist == 0 || dist > out.len() {
                return Err(Error::corruption("lz: distance out of range"));
            }
            if out.len().saturating_add(len) > max_output {
                return Err(Error::corruption("lz: output exceeds bound"));
            }
            // Byte-by-byte copy so overlapping matches (dist < len) repeat the
            // pattern, exactly as the encoder intends.
            let start = out.len() - dist;
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
            rest = tail2;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 1 << 22;

    fn roundtrip(bytes: &[u8]) {
        let packed = compress(bytes);
        let back = decompress(&packed, CAP).expect("decompress");
        assert_eq!(back, bytes, "roundtrip mismatch for {} bytes", bytes.len());
    }

    #[test]
    fn roundtrips_empty_and_tiny() {
        for s in [&b""[..], b"a", b"ab", b"abc", b"abcd"] {
            roundtrip(s);
        }
    }

    #[test]
    fn roundtrips_highly_repetitive() {
        let data: Vec<u8> = b"abcabcabc".iter().cloned().cycle().take(9000).collect();
        roundtrip(&data);
        assert!(compress(&data).len() < data.len() / 4);
    }

    #[test]
    fn roundtrips_overlap_pattern() {
        // A single byte followed by a long run exercises overlapping copies
        // (distance 1, large length).
        let mut data = vec![b'Q'];
        data.extend(std::iter::repeat(b'Q').take(4000));
        roundtrip(&data);
    }

    #[test]
    fn roundtrips_structured_keys() {
        let mut data = Vec::new();
        for i in 0..500u32 {
            data.extend_from_slice(format!("user:{i:08}:profile").as_bytes());
        }
        roundtrip(&data);
        assert!(compress(&data).len() < data.len());
    }

    #[test]
    fn roundtrips_incompressible() {
        // Pseudo-random data should still round-trip (expanding slightly).
        let mut x = 0x1234_5678_9abc_def0u64;
        let mut data = Vec::with_capacity(2000);
        for _ in 0..2000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            data.push(x as u8);
        }
        roundtrip(&data);
    }

    #[test]
    fn rejects_distance_past_output() {
        // Match token, len 4, distance 1 with no prior output.
        let mut buf = Vec::new();
        put_varint64(&mut buf, 1); // match token, (len - MIN_MATCH) = 0
        put_varint64(&mut buf, 1);
        assert!(decompress(&buf, CAP).is_err());
    }

    #[test]
    fn rejects_output_bound() {
        let data = vec![0u8; 8000];
        let packed = compress(&data);
        assert!(decompress(&packed, 7999).is_err());
        assert!(decompress(&packed, 8000).is_ok());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x = 0xdead_beef_0000_0001u64;
        for _ in 0..4000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x as usize) % 24;
            let mut bytes = Vec::with_capacity(len);
            let mut y = x;
            for _ in 0..len {
                y = y.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((y >> 33) as u8);
            }
            let _ = decompress(&bytes, CAP);
        }
    }
}
