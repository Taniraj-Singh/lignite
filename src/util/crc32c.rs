//! CRC32C (Castagnoli, polynomial 0x1EDC6F41) implemented with a runtime-built
//! slice-by-one table.
//!
//! The engine masks stored CRCs before writing them, exactly like LevelDB, so
//! that a checksum computed over a buffer that itself contains a CRC does not
//! accidentally collide with a common value. [`mask`] and [`unmask`] implement
//! that transformation.

/// The Castagnoli polynomial in reflected form.
const CASTAGNOLI: u32 = 0x82f6_3b78;

/// Rotation constant used by [`mask`]. Matches LevelDB's `crc32c::Mask`.
const MASK_DELTA: u32 = 0xa282_ead8;

/// Lazily initialised lookup table.
struct Table {
    entries: [u32; 256],
}

impl Table {
    const fn new() -> Self {
        let mut entries = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut j = 0;
            while j < 8 {
                if crc & 1 == 1 {
                    crc = (crc >> 1) ^ CASTAGNOLI;
                } else {
                    crc >>= 1;
                }
                j += 1;
            }
            entries[i] = crc;
            i += 1;
        }
        Table { entries }
    }
}

static TABLE: Table = Table::new();

/// Compute the CRC32C of `data`, starting from a zero initial value.
#[inline]
pub fn value(data: &[u8]) -> u32 {
    extend(0, data)
}

/// Continue a CRC32C computation over an additional chunk of `data`.
///
/// This lets a checksum be built incrementally across non-contiguous buffers,
/// which the log writer relies on when it frames a record header separately from
/// its payload.
pub fn extend(prev: u32, data: &[u8]) -> u32 {
    let mut crc = !prev;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xff) as usize;
        crc = (crc >> 8) ^ TABLE.entries[idx];
    }
    !crc
}

/// Transform a raw CRC into its masked, storable form.
#[inline]
pub fn mask(crc: u32) -> u32 {
    crc.rotate_left(17).wrapping_add(MASK_DELTA)
}

/// Invert [`mask`], recovering the raw CRC from its stored form.
#[inline]
pub fn unmask(masked: u32) -> u32 {
    masked.wrapping_sub(MASK_DELTA).rotate_right(17)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // The canonical CRC32C check value for the ASCII string "123456789".
        assert_eq!(value(b"123456789"), 0xe306_9283);
        // Empty input hashes to zero.
        assert_eq!(value(b""), 0);
        // RFC 3720 iSCSI vector: 32 bytes of zero.
        assert_eq!(value(&[0u8; 32]), 0x8a91_36aa);
    }

    #[test]
    fn extend_matches_single_shot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let (a, b) = data.split_at(10);
        assert_eq!(value(data), extend(value(a), b));
    }

    #[test]
    fn mask_roundtrip() {
        for &c in &[0u32, 1, 0xdead_beef, 0xffff_ffff, 12345] {
            assert_eq!(unmask(mask(c)), c);
            assert_ne!(mask(c), c);
        }
    }
}
