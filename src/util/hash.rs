//! A small, fast, non-cryptographic hash used by the bloom filter policy and the
//! block-cache shard selector. It is a byte-oriented variant of the Murmur family
//! and is bit-for-bit identical to LevelDB's `util/hash.cc`, which matters because
//! bloom filters persisted in an SSTable must hash keys the same way on read.

/// Hash `data` with the given `seed`.
pub fn hash(data: &[u8], seed: u32) -> u32 {
    const M: u32 = 0xc6a4_a793;
    const R: u32 = 24;
    let n = data.len();
    let mut h = seed ^ (n as u32).wrapping_mul(M);

    let mut i = 0;
    while i + 4 <= n {
        let w = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        h = h.wrapping_add(w);
        h = h.wrapping_mul(M);
        h ^= h >> 16;
        i += 4;
    }

    // Tail bytes, high-order first.
    let remaining = n - i;
    if remaining >= 3 {
        h = h.wrapping_add((data[i + 2] as u32) << 16);
    }
    if remaining >= 2 {
        h = h.wrapping_add((data[i + 1] as u32) << 8);
    }
    if remaining >= 1 {
        h = h.wrapping_add(data[i] as u32);
        h = h.wrapping_mul(M);
        h ^= h >> R;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_values() {
        // Values pinned from LevelDB's hash_test.cc to lock the wire behaviour.
        assert_eq!(hash(&[], 0xbc9f_1d34), 0xbc9f_1d34);
        assert_eq!(hash(&[0x62], 0xbc9f_1d34), 0xef1345c4);
        assert_eq!(hash(&[0xc3, 0x97], 0xbc9f_1d34), 0x5b663814);
        assert_eq!(hash(&[0xe2, 0x99, 0xa5], 0xbc9f_1d34), 0x323c078f);
        assert_eq!(hash(&[0xe1, 0x80, 0xb9, 0x32], 0xbc9f_1d34), 0xed21633a);
    }

    #[test]
    fn seed_changes_output() {
        assert_ne!(hash(b"lignite", 1), hash(b"lignite", 2));
    }
}
