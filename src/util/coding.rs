//! Portable little-endian fixed-width and LEB128-style variable-width integer
//! coding, plus length-prefixed byte slices.
//!
//! The on-disk formats produced here are byte-for-byte stable across platforms:
//! fixed integers are always little-endian, and varints use the standard 7-bit
//! continuation encoding with the low group first. These primitives back the
//! write-ahead log, the SSTable block format and the write-batch wire format, so
//! their behaviour must never change without a format-version bump.

/// Append `value` to `dst` as 4 little-endian bytes.
#[inline]
pub fn encode_fixed32(dst: &mut Vec<u8>, value: u32) {
    dst.extend_from_slice(&value.to_le_bytes());
}

/// Append `value` to `dst` as 8 little-endian bytes.
#[inline]
pub fn encode_fixed64(dst: &mut Vec<u8>, value: u64) {
    dst.extend_from_slice(&value.to_le_bytes());
}

/// Read a little-endian `u32` from the front of `src`.
///
/// Returns `None` if fewer than four bytes are available.
#[inline]
pub fn decode_fixed32(src: &[u8]) -> Option<u32> {
    if src.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([src[0], src[1], src[2], src[3]]))
}

/// Read a little-endian `u64` from the front of `src`.
///
/// Returns `None` if fewer than eight bytes are available.
#[inline]
pub fn decode_fixed64(src: &[u8]) -> Option<u64> {
    if src.len() < 8 {
        return None;
    }
    Some(u64::from_le_bytes([
        src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
    ]))
}

/// Number of bytes the varint encoding of `value` occupies.
#[inline]
pub fn varint_length(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

/// Append the varint encoding of a 32-bit `value` to `dst`.
pub fn put_varint32(dst: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        dst.push((value as u8) | 0x80);
        value >>= 7;
    }
    dst.push(value as u8);
}

/// Append the varint encoding of a 64-bit `value` to `dst`.
pub fn put_varint64(dst: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        dst.push((value as u8) | 0x80);
        value >>= 7;
    }
    dst.push(value as u8);
}

/// Decode a varint-encoded `u32` from the front of `src`.
///
/// On success returns the decoded value together with the remaining slice.
/// Returns `None` on truncation or if the encoding overflows 32 bits.
pub fn decode_varint32(src: &[u8]) -> Option<(u32, &[u8])> {
    let mut result: u32 = 0;
    let mut shift = 0;
    let mut i = 0;
    while shift <= 28 {
        let byte = *src.get(i)?;
        i += 1;
        if shift == 28 && byte > 0x0f {
            return None; // more than 32 bits of payload
        }
        result |= ((byte & 0x7f) as u32) << shift;
        if byte & 0x80 == 0 {
            return Some((result, &src[i..]));
        }
        shift += 7;
    }
    None
}

/// Decode a varint-encoded `u64` from the front of `src`.
///
/// On success returns the decoded value together with the remaining slice.
/// Returns `None` on truncation or if the encoding uses more than ten groups.
pub fn decode_varint64(src: &[u8]) -> Option<(u64, &[u8])> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut i = 0;
    while shift <= 63 {
        let byte = *src.get(i)?;
        i += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, &src[i..]));
        }
        shift += 7;
    }
    None
}

/// Zig-zag encode a signed integer into an unsigned one so that small-magnitude
/// negatives also encode to short varints (`0 → 0`, `-1 → 1`, `1 → 2`, ...).
#[inline]
pub fn zigzag_encode(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Inverse of [`zigzag_encode`].
#[inline]
pub fn zigzag_decode(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Append a zig-zag varint encoding of a signed integer.
pub fn put_varint_signed(dst: &mut Vec<u8>, value: i64) {
    put_varint64(dst, zigzag_encode(value));
}

/// Decode a zig-zag varint-encoded signed integer.
pub fn decode_varint_signed(src: &[u8]) -> Option<(i64, &[u8])> {
    let (raw, rest) = decode_varint64(src)?;
    Some((zigzag_decode(raw), rest))
}

/// Append `slice` prefixed by its length as a varint.
pub fn put_length_prefixed_slice(dst: &mut Vec<u8>, slice: &[u8]) {
    put_varint32(dst, slice.len() as u32);
    dst.extend_from_slice(slice);
}

/// Read a varint-length-prefixed slice from the front of `src`.
///
/// Returns the slice and the remaining input, or `None` on truncation.
pub fn take_length_prefixed_slice(src: &[u8]) -> Option<(&[u8], &[u8])> {
    let (len, rest) = decode_varint32(src)?;
    let len = len as usize;
    if rest.len() < len {
        return None;
    }
    Some((&rest[..len], &rest[len..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_roundtrip() {
        let mut buf = Vec::new();
        encode_fixed32(&mut buf, 0xdead_beef);
        encode_fixed64(&mut buf, 0x0102_0304_0506_0708);
        assert_eq!(decode_fixed32(&buf), Some(0xdead_beef));
        assert_eq!(decode_fixed64(&buf[4..]), Some(0x0102_0304_0506_0708));
    }

    #[test]
    fn varint32_roundtrip() {
        for &v in &[0u32, 1, 127, 128, 16383, 16384, u32::MAX, 0x0fff_ffff] {
            let mut buf = Vec::new();
            put_varint32(&mut buf, v);
            assert_eq!(buf.len(), varint_length(v as u64));
            let (decoded, rest) = decode_varint32(&buf).unwrap();
            assert_eq!(decoded, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn varint64_roundtrip() {
        for &v in &[0u64, 300, u32::MAX as u64, u64::MAX, 1 << 62] {
            let mut buf = Vec::new();
            put_varint64(&mut buf, v);
            let (decoded, rest) = decode_varint64(&buf).unwrap();
            assert_eq!(decoded, v);
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn varint32_rejects_overflow() {
        // Five bytes whose payload exceeds 32 bits.
        let buf = [0xff, 0xff, 0xff, 0xff, 0x7f];
        assert!(decode_varint32(&buf).is_none());
    }

    #[test]
    fn length_prefixed_roundtrip() {
        let mut buf = Vec::new();
        put_length_prefixed_slice(&mut buf, b"hello");
        put_length_prefixed_slice(&mut buf, b"");
        put_length_prefixed_slice(&mut buf, b"world!");
        let (a, rest) = take_length_prefixed_slice(&buf).unwrap();
        let (b, rest) = take_length_prefixed_slice(rest).unwrap();
        let (c, rest) = take_length_prefixed_slice(rest).unwrap();
        assert_eq!(a, b"hello");
        assert_eq!(b, b"");
        assert_eq!(c, b"world!");
        assert!(rest.is_empty());
    }

    #[test]
    fn truncated_varint_is_none() {
        let buf = [0x80, 0x80];
        assert!(decode_varint32(&buf).is_none());
        assert!(decode_varint64(&buf).is_none());
    }

    #[test]
    fn zigzag_maps_small_magnitudes_to_small_codes() {
        assert_eq!(zigzag_encode(0), 0);
        assert_eq!(zigzag_encode(-1), 1);
        assert_eq!(zigzag_encode(1), 2);
        assert_eq!(zigzag_encode(-2), 3);
        assert_eq!(zigzag_encode(2), 4);
    }

    #[test]
    fn signed_varint_roundtrip() {
        for &v in &[0i64, 1, -1, 63, -64, i32::MIN as i64, i32::MAX as i64, i64::MIN, i64::MAX] {
            let mut buf = Vec::new();
            put_varint_signed(&mut buf, v);
            let (decoded, rest) = decode_varint_signed(&buf).unwrap();
            assert_eq!(decoded, v);
            assert!(rest.is_empty());
        }
    }
}
