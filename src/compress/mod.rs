//! Block compression codecs and the dispatch that binds them to
//! [`CompressionType`].
//!
//! An SSTable stores each block with a one-byte compression tag (see
//! [`crate::sstable::format`]). This module supplies the codecs behind those
//! tags and the two operations the table layer needs:
//!
//! * [`encode`] — given the compression the caller *wants*, produce the bytes to
//!   store and the tag they were actually stored under. Compression is only
//!   adopted when it shrinks the block, so an incompressible block is transparently
//!   stored as [`CompressionType::None`]; a reader therefore never has to trust
//!   that compression helped.
//! * [`decode`] — given a tag and the stored bytes, reconstruct the original block
//!   contents, bounding the result so a corrupt or hostile block cannot exhaust
//!   memory.
//!
//! The individual codecs live in [`rle`] and [`lz`]; both are self-contained,
//! dependency-free, and hardened so their decoders return [`Error::Corruption`]
//! rather than misbehaving on arbitrary bytes.

pub mod lz;
pub mod rle;

use crate::error::{Error, Result};
use crate::options::CompressionType;

/// Hard ceiling on the size of a decompressed block.
///
/// The block format does not store the uncompressed length, so the decoder is
/// given this constant as its bound. It is far larger than any legitimate block
/// (blocks target a few kilobytes) yet small enough that a crafted stream cannot
/// drive an unbounded allocation. A block that genuinely decompresses past this
/// bound is treated as corrupt.
pub const MAX_BLOCK_DECOMPRESSED_SIZE: usize = 64 << 20; // 64 MiB

/// Compress `raw` for storage under the caller's preferred `wanted` codec.
///
/// Returns the tag to record and the bytes to store. If `wanted` is
/// [`CompressionType::None`], or the chosen codec fails to shrink the block, the
/// block is stored verbatim under [`CompressionType::None`]. This keeps the
/// stored form self-describing: the tag always names the codec actually applied.
pub fn encode(wanted: CompressionType, raw: &[u8]) -> (CompressionType, Vec<u8>) {
    let compressed = match wanted {
        CompressionType::None => return (CompressionType::None, raw.to_vec()),
        CompressionType::Rle => rle::compress(raw),
        CompressionType::Lz => lz::compress(raw),
        // Reserved tags we can decode-detect but do not emit.
        CompressionType::Snappy | CompressionType::Zstd => return (CompressionType::None, raw.to_vec()),
    };
    if compressed.len() < raw.len() {
        (wanted, compressed)
    } else {
        (CompressionType::None, raw.to_vec())
    }
}

/// Reconstruct a block's contents from its stored `bytes` under `kind`.
///
/// [`CompressionType::None`] returns the bytes unchanged. The [`CompressionType::Rle`]
/// and [`CompressionType::Lz`] codecs decompress with a [`MAX_BLOCK_DECOMPRESSED_SIZE`]
/// bound. The reserved [`CompressionType::Snappy`] and [`CompressionType::Zstd`]
/// tags are recognised but unsupported in this build and yield
/// [`Error::Unsupported`].
pub fn decode(kind: CompressionType, bytes: &[u8]) -> Result<Vec<u8>> {
    match kind {
        CompressionType::None => Ok(bytes.to_vec()),
        CompressionType::Rle => rle::decompress(bytes, MAX_BLOCK_DECOMPRESSED_SIZE),
        CompressionType::Lz => lz::decompress(bytes, MAX_BLOCK_DECOMPRESSED_SIZE),
        other => Err(Error::Unsupported(format!(
            "block compression {other:?} not supported by this build"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..400u32 {
            v.extend_from_slice(format!("row:{:06}:value-with-shared-shape\n", i % 40).as_bytes());
        }
        v
    }

    #[test]
    fn none_stores_verbatim() {
        let raw = sample();
        let (kind, stored) = encode(CompressionType::None, &raw);
        assert_eq!(kind, CompressionType::None);
        assert_eq!(stored, raw);
        assert_eq!(decode(kind, &stored).unwrap(), raw);
    }

    #[test]
    fn rle_and_lz_roundtrip_via_dispatch() {
        let raw = sample();
        for kind in [CompressionType::Rle, CompressionType::Lz] {
            let (actual, stored) = encode(kind, &raw);
            // This sample is compressible, so the wanted codec is adopted.
            assert_eq!(actual, kind);
            assert!(stored.len() < raw.len());
            assert_eq!(decode(actual, &stored).unwrap(), raw);
        }
    }

    #[test]
    fn incompressible_falls_back_to_none() {
        // Random bytes: neither codec should shrink them, so both fall back.
        let mut x = 0x51ed_270bu64;
        let mut raw = Vec::new();
        for _ in 0..1000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            raw.push(x as u8);
        }
        for kind in [CompressionType::Rle, CompressionType::Lz] {
            let (actual, stored) = encode(kind, &raw);
            assert_eq!(actual, CompressionType::None);
            assert_eq!(stored, raw);
        }
    }

    #[test]
    fn reserved_tags_are_unsupported_on_decode() {
        assert!(matches!(
            decode(CompressionType::Snappy, b"x"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            decode(CompressionType::Zstd, b"x"),
            Err(Error::Unsupported(_))
        ));
    }
}
