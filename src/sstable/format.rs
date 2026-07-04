//! Physical layout primitives shared by the SSTable reader and writer.
//!
//! An SSTable file is a sequence of length-delimited *blocks* followed by a
//! fixed-size footer:
//!
//! ```text
//! [ data block 0 ] [ data block 1 ] ... [ filter block ]
//! [ metaindex block ] [ index block ] [ footer (48 bytes) ]
//! ```
//!
//! Every block is written with a five-byte trailer: one byte naming its
//! [`CompressionType`] and a four-byte masked CRC32C over the block contents plus
//! that type byte. A [`BlockHandle`] — an `(offset, size)` pair where `size`
//! excludes the trailer — locates a block. The footer pins the metaindex and
//! index handles and ends with an eight-byte magic number that identifies the
//! format.

use crate::error::{Error, Result};
use crate::options::CompressionType;
use crate::util::coding::{decode_varint64, encode_fixed64, put_varint64};

/// Bytes appended after every block: `[compression_type(1)][crc32c(4)]`.
pub const BLOCK_TRAILER_SIZE: usize = 5;

/// Maximum bytes a [`BlockHandle`] occupies once encoded (two 10-byte varints).
pub const MAX_BLOCK_HANDLE_ENCODED_LENGTH: usize = 20;

/// Fixed footer size: two padded block handles plus the 8-byte magic.
pub const FOOTER_ENCODED_LENGTH: usize = 2 * MAX_BLOCK_HANDLE_ENCODED_LENGTH + 8;

/// Identifies a `lignite` SSTable. (An arbitrary but fixed 64-bit constant.)
pub const TABLE_MAGIC_NUMBER: u64 = 0x6c69_676e_6974_6531; // "lignite1" in ASCII

/// The location and length of a block within a table file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockHandle {
    /// Byte offset of the block within the file.
    pub offset: u64,
    /// Size of the block contents, excluding the [`BLOCK_TRAILER_SIZE`] trailer.
    pub size: u64,
}

impl BlockHandle {
    /// Create a handle.
    pub fn new(offset: u64, size: u64) -> Self {
        BlockHandle { offset, size }
    }

    /// Append the varint encoding of this handle to `dst`.
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        put_varint64(dst, self.offset);
        put_varint64(dst, self.size);
    }

    /// Decode a handle from the front of `src`, returning it and the remainder.
    pub fn decode_from(src: &[u8]) -> Result<(BlockHandle, &[u8])> {
        let (offset, rest) = decode_varint64(src)
            .ok_or_else(|| Error::corruption("bad block handle offset"))?;
        let (size, rest) = decode_varint64(rest)
            .ok_or_else(|| Error::corruption("bad block handle size"))?;
        Ok((BlockHandle { offset, size }, rest))
    }
}

/// The trailing metadata block of a table file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Footer {
    /// Handle of the metaindex block (maps meta-block names to handles).
    pub metaindex_handle: BlockHandle,
    /// Handle of the index block (maps block boundary keys to data handles).
    pub index_handle: BlockHandle,
}

impl Footer {
    /// Serialise the footer to exactly [`FOOTER_ENCODED_LENGTH`] bytes.
    pub fn encode_to(&self, dst: &mut Vec<u8>) {
        let start = dst.len();
        self.metaindex_handle.encode_to(dst);
        self.index_handle.encode_to(dst);
        // Zero-pad the handle region to a fixed width.
        dst.resize(start + 2 * MAX_BLOCK_HANDLE_ENCODED_LENGTH, 0);
        encode_fixed64(dst, TABLE_MAGIC_NUMBER);
        debug_assert_eq!(dst.len() - start, FOOTER_ENCODED_LENGTH);
    }

    /// Parse a footer from a [`FOOTER_ENCODED_LENGTH`]-byte slice.
    pub fn decode_from(src: &[u8]) -> Result<Footer> {
        if src.len() < FOOTER_ENCODED_LENGTH {
            return Err(Error::corruption("footer too short"));
        }
        let magic_off = FOOTER_ENCODED_LENGTH - 8;
        let magic = crate::util::coding::decode_fixed64(&src[magic_off..])
            .ok_or_else(|| Error::corruption("footer magic unreadable"))?;
        if magic != TABLE_MAGIC_NUMBER {
            return Err(Error::corruption("not a lignite table (bad magic)"));
        }
        let (metaindex_handle, rest) = BlockHandle::decode_from(src)?;
        let (index_handle, _) = BlockHandle::decode_from(rest)?;
        Ok(Footer {
            metaindex_handle,
            index_handle,
        })
    }
}

/// Verify (or record) that `type_byte` names a supported compression.
pub fn parse_compression(type_byte: u8) -> Result<CompressionType> {
    CompressionType::from_u8(type_byte)
        .ok_or_else(|| Error::corruption(format!("unknown compression byte {type_byte}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_handle_roundtrip() {
        let h = BlockHandle::new(123456, 789);
        let mut buf = Vec::new();
        h.encode_to(&mut buf);
        let (decoded, rest) = BlockHandle::decode_from(&buf).unwrap();
        assert_eq!(decoded, h);
        assert!(rest.is_empty());
    }

    #[test]
    fn footer_roundtrip() {
        let f = Footer {
            metaindex_handle: BlockHandle::new(10, 20),
            index_handle: BlockHandle::new(40, 55),
        };
        let mut buf = Vec::new();
        f.encode_to(&mut buf);
        assert_eq!(buf.len(), FOOTER_ENCODED_LENGTH);
        let decoded = Footer::decode_from(&buf).unwrap();
        assert_eq!(decoded, f);
    }

    #[test]
    fn footer_rejects_bad_magic() {
        let mut buf = vec![0u8; FOOTER_ENCODED_LENGTH];
        buf[FOOTER_ENCODED_LENGTH - 1] = 0xff;
        assert!(Footer::decode_from(&buf).is_err());
    }
}
