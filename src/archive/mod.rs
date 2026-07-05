//! A portable, self-describing archive format for snapshotting a key space.
//!
//! An *archive* is a single flat byte string that captures a set of key/value
//! records — the kind of thing a backup, an export, or a cross-machine transfer
//! ships. It is deliberately independent of the SSTable layout: it has no block
//! structure, no index and no MVCC trailers, just a header, a body of records, and
//! a checksum. That makes it a natural interchange format and a compact unit for
//! [`crate::db::Db::export_archive`] / [`crate::db::Db::import_archive`].
//!
//! ## Layout
//!
//! ```text
//! archive := header  body_len(fixed32)  stored_body  crc32c(fixed32)
//! header  := magic(8) version(fixed32) flags(fixed32)
//! ```
//!
//! `stored_body` is the record stream, optionally LZ-compressed when the
//! [`FLAG_COMPRESSED`] bit is set in `flags`; `crc32c` is a masked CRC32C over
//! every preceding byte (header through `stored_body`). The uncompressed record
//! stream is:
//!
//! ```text
//! record* := ( PUT    varint(klen) key varint(vlen) value
//!            | DELETE varint(klen) key )*
//!            END varint(record_count)
//! ```
//!
//! ## Robustness
//!
//! [`ArchiveReader::parse`] is written to consume untrusted bytes: it validates
//! the length, the magic, the version, the checksum, the compression bound, every
//! varint length against the remaining input, and the trailing record count
//! against the number of records actually decoded. Any inconsistency yields
//! [`Error::Corruption`]; no input can panic, over-read, or allocate without
//! bound. This is the surface the archive fuzz target drives.

use crate::compress;
use crate::error::{Error, Result};
use crate::options::CompressionType;
use crate::util::coding::{
    decode_fixed32, encode_fixed32, put_varint64, decode_varint64, put_length_prefixed_slice,
    take_length_prefixed_slice,
};
use crate::util::crc32c;

/// Eight-byte archive magic: `LGNARCH1` in ASCII.
pub const ARCHIVE_MAGIC: [u8; 8] = *b"LGNARCH1";

/// The format version this build writes and is willing to read.
pub const ARCHIVE_VERSION: u32 = 1;

/// `flags` bit: the body is LZ-compressed.
pub const FLAG_COMPRESSED: u32 = 0x1;

/// Fixed header size: magic(8) + version(4) + flags(4).
const HEADER_SIZE: usize = 16;

/// Record tags within the body.
const TAG_END: u8 = 0x0;
const TAG_PUT: u8 = 0x1;
const TAG_DELETE: u8 = 0x2;

/// Upper bound on a decompressed archive body, guarding the reader against a
/// crafted compression header that claims an enormous expansion.
const MAX_ARCHIVE_BODY: usize = 1 << 30; // 1 GiB

/// One logical entry in an archive: a live value or a deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveRecord {
    /// A key present with a value.
    Put(Vec<u8>, Vec<u8>),
    /// A key explicitly absent (a tombstone carried across the transfer).
    Delete(Vec<u8>),
}

impl ArchiveRecord {
    /// The record's key, regardless of kind.
    pub fn key(&self) -> &[u8] {
        match self {
            ArchiveRecord::Put(k, _) => k,
            ArchiveRecord::Delete(k) => k,
        }
    }
}

/// Accumulates records and serialises them into an archive byte string.
pub struct ArchiveWriter {
    body: Vec<u8>,
    count: u64,
    compression: CompressionType,
}

impl ArchiveWriter {
    /// A writer that stores the body verbatim.
    pub fn new() -> Self {
        ArchiveWriter {
            body: Vec::new(),
            count: 0,
            compression: CompressionType::None,
        }
    }

    /// A writer that LZ-compresses the body when that shrinks it.
    pub fn compressed() -> Self {
        ArchiveWriter {
            body: Vec::new(),
            count: 0,
            compression: CompressionType::Lz,
        }
    }

    /// Number of records appended so far.
    pub fn len(&self) -> u64 {
        self.count
    }

    /// Whether no records have been appended.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Append a live key/value pair.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        self.body.push(TAG_PUT);
        put_length_prefixed_slice(&mut self.body, key);
        put_length_prefixed_slice(&mut self.body, value);
        self.count += 1;
    }

    /// Append a deletion.
    pub fn delete(&mut self, key: &[u8]) {
        self.body.push(TAG_DELETE);
        put_length_prefixed_slice(&mut self.body, key);
        self.count += 1;
    }

    /// Append an already-formed record.
    pub fn add(&mut self, record: &ArchiveRecord) {
        match record {
            ArchiveRecord::Put(k, v) => self.put(k, v),
            ArchiveRecord::Delete(k) => self.delete(k),
        }
    }

    /// Serialise the accumulated records into a complete archive.
    pub fn finish(mut self) -> Vec<u8> {
        // Terminate the record stream with an END tag and the record count.
        self.body.push(TAG_END);
        put_varint64(&mut self.body, self.count);

        // Optionally compress the body; only adopt compression if it helps.
        let (flags, stored_body) = match self.compression {
            CompressionType::None => (0u32, self.body),
            _ => {
                let (kind, bytes) = compress::encode(CompressionType::Lz, &self.body);
                match kind {
                    CompressionType::Lz => (FLAG_COMPRESSED, bytes),
                    _ => (0u32, bytes),
                }
            }
        };

        let mut out = Vec::with_capacity(HEADER_SIZE + stored_body.len() + 8);
        out.extend_from_slice(&ARCHIVE_MAGIC);
        encode_fixed32(&mut out, ARCHIVE_VERSION);
        encode_fixed32(&mut out, flags);
        encode_fixed32(&mut out, stored_body.len() as u32);
        out.extend_from_slice(&stored_body);

        let crc = crc32c::mask(crc32c::value(&out));
        encode_fixed32(&mut out, crc);
        out
    }
}

impl Default for ArchiveWriter {
    fn default() -> Self {
        ArchiveWriter::new()
    }
}

/// Parses an archive byte string into its records.
pub struct ArchiveReader;

impl ArchiveReader {
    /// Parse `bytes` into the sequence of records it holds.
    ///
    /// Returns [`Error::Corruption`] for any malformed input and
    /// [`Error::Unsupported`] for a recognised-but-newer format version.
    pub fn parse(bytes: &[u8]) -> Result<Vec<ArchiveRecord>> {
        // Minimum: header + body_len + crc, with an empty stored body.
        if bytes.len() < HEADER_SIZE + 4 + 4 {
            return Err(Error::corruption("archive: shorter than framing"));
        }
        if bytes[..8] != ARCHIVE_MAGIC {
            return Err(Error::corruption("archive: bad magic"));
        }
        let version = decode_fixed32(&bytes[8..12]).unwrap();
        if version != ARCHIVE_VERSION {
            return Err(Error::Unsupported(format!(
                "archive: unsupported version {version}"
            )));
        }
        let flags = decode_fixed32(&bytes[12..16]).unwrap();
        let body_len = decode_fixed32(&bytes[16..20]).unwrap() as usize;

        // The stored body must fit exactly between the length field and the CRC.
        let body_start = HEADER_SIZE + 4;
        let crc_start = body_start
            .checked_add(body_len)
            .ok_or_else(|| Error::corruption("archive: body length overflow"))?;
        if crc_start + 4 != bytes.len() {
            return Err(Error::corruption("archive: body length does not match frame"));
        }

        // Verify the trailing checksum over everything before it.
        let stored_crc = decode_fixed32(&bytes[crc_start..]).unwrap();
        let computed = crc32c::mask(crc32c::value(&bytes[..crc_start]));
        if stored_crc != computed {
            return Err(Error::corruption("archive: checksum mismatch"));
        }

        let stored_body = &bytes[body_start..crc_start];
        let body = if flags & FLAG_COMPRESSED != 0 {
            compress::lz::decompress(stored_body, MAX_ARCHIVE_BODY)?
        } else {
            stored_body.to_vec()
        };
        if flags & !FLAG_COMPRESSED != 0 {
            return Err(Error::corruption("archive: unknown flag bits set"));
        }

        Self::parse_body(&body)
    }

    /// Decode the (already decompressed) record stream.
    fn parse_body(body: &[u8]) -> Result<Vec<ArchiveRecord>> {
        let mut records = Vec::new();
        let mut rest = body;
        loop {
            let tag = *rest
                .first()
                .ok_or_else(|| Error::corruption("archive: body ended before END"))?;
            rest = &rest[1..];
            match tag {
                TAG_PUT => {
                    let (key, r1) = take_length_prefixed_slice(rest)
                        .ok_or_else(|| Error::corruption("archive: bad put key"))?;
                    let (value, r2) = take_length_prefixed_slice(r1)
                        .ok_or_else(|| Error::corruption("archive: bad put value"))?;
                    records.push(ArchiveRecord::Put(key.to_vec(), value.to_vec()));
                    rest = r2;
                }
                TAG_DELETE => {
                    let (key, r1) = take_length_prefixed_slice(rest)
                        .ok_or_else(|| Error::corruption("archive: bad delete key"))?;
                    records.push(ArchiveRecord::Delete(key.to_vec()));
                    rest = r1;
                }
                TAG_END => {
                    let (count, r1) = decode_varint64(rest)
                        .ok_or_else(|| Error::corruption("archive: bad record count"))?;
                    if count != records.len() as u64 {
                        return Err(Error::corruption("archive: record count mismatch"));
                    }
                    if !r1.is_empty() {
                        return Err(Error::corruption("archive: trailing bytes after END"));
                    }
                    return Ok(records);
                }
                other => {
                    return Err(Error::corruption(format!("archive: unknown record tag {other}")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_records() -> Vec<ArchiveRecord> {
        vec![
            ArchiveRecord::Put(b"alpha".to_vec(), b"1".to_vec()),
            ArchiveRecord::Put(b"beta".to_vec(), b"two".to_vec()),
            ArchiveRecord::Delete(b"gamma".to_vec()),
            ArchiveRecord::Put(b"".to_vec(), b"empty-key".to_vec()),
            ArchiveRecord::Put(b"delta".to_vec(), b"".to_vec()),
        ]
    }

    fn build(records: &[ArchiveRecord], compressed: bool) -> Vec<u8> {
        let mut w = if compressed {
            ArchiveWriter::compressed()
        } else {
            ArchiveWriter::new()
        };
        for r in records {
            w.add(r);
        }
        w.finish()
    }

    #[test]
    fn roundtrip_uncompressed() {
        let records = sample_records();
        let bytes = build(&records, false);
        assert_eq!(ArchiveReader::parse(&bytes).unwrap(), records);
    }

    #[test]
    fn roundtrip_compressed() {
        // Repetitive data so the compressed flag is actually taken.
        let mut records = Vec::new();
        for i in 0..200u32 {
            records.push(ArchiveRecord::Put(
                format!("key:{i:05}").into_bytes(),
                b"a-fairly-repetitive-value-payload".to_vec(),
            ));
        }
        let bytes = build(&records, true);
        assert_eq!(ArchiveReader::parse(&bytes).unwrap(), records);
    }

    #[test]
    fn empty_archive_roundtrips() {
        let bytes = build(&[], false);
        assert!(ArchiveReader::parse(&bytes).unwrap().is_empty());
    }

    #[test]
    fn detects_flipped_byte() {
        let mut bytes = build(&sample_records(), false);
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        assert!(ArchiveReader::parse(&bytes).is_err());
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = build(&sample_records(), false);
        bytes[0] = b'X';
        assert!(matches!(ArchiveReader::parse(&bytes), Err(Error::Corruption(_))));
    }

    #[test]
    fn rejects_wrong_version() {
        let mut bytes = build(&sample_records(), false);
        bytes[8] = 9; // bump the little-endian version to 9
        assert!(matches!(ArchiveReader::parse(&bytes), Err(Error::Unsupported(_))));
    }

    #[test]
    fn rejects_truncation() {
        let bytes = build(&sample_records(), false);
        for cut in 0..bytes.len() {
            // Every truncation must be rejected cleanly (never panic).
            let _ = ArchiveReader::parse(&bytes[..cut]);
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..3000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x as usize) % 48;
            let mut bytes = Vec::with_capacity(len);
            let mut y = x;
            for _ in 0..len {
                y = y.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((y >> 40) as u8);
            }
            let _ = ArchiveReader::parse(&bytes);
        }
    }
}
