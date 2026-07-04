//! A bloom filter policy compatible with LevelDB's `BloomFilterPolicy`.
//!
//! Rather than compute `k` independent hashes it uses the double-hashing trick:
//! a single base hash plus a rotation-derived delta generates all `k` probe
//! positions. The number of probes `k` is chosen from the configured bits-per-key
//! to minimise the false-positive rate (`k ≈ bits_per_key · ln 2`), clamped to a
//! sane range. The trailing byte of the encoded filter records `k` so the reader
//! probes exactly the same positions the writer set.

use crate::filter::FilterPolicy;
use crate::util::hash::hash;

/// Bloom filter with a configurable bit budget per key.
#[derive(Debug, Clone)]
pub struct BloomFilterPolicy {
    bits_per_key: usize,
    k: usize,
}

impl BloomFilterPolicy {
    /// Create a policy allocating `bits_per_key` bits for every inserted key.
    pub fn new(bits_per_key: usize) -> Self {
        // k = bits_per_key * ln(2), clamped to [1, 30].
        let k = ((bits_per_key as f64 * 0.69) as usize).clamp(1, 30);
        BloomFilterPolicy { bits_per_key, k }
    }

    fn bloom_hash(key: &[u8]) -> u32 {
        hash(key, 0xbc9f_1d34)
    }
}

impl FilterPolicy for BloomFilterPolicy {
    fn name(&self) -> &'static str {
        "lignite.BuiltinBloomFilter2"
    }

    fn create_filter(&self, keys: &[&[u8]], dst: &mut Vec<u8>) {
        // Compute total bits, rounded up to a whole byte, with a floor so tiny
        // filters still have a low false-positive rate.
        let mut bits = keys.len() * self.bits_per_key;
        if bits < 64 {
            bits = 64;
        }
        let bytes = bits.div_ceil(8);
        let bits = bytes * 8;

        let init_len = dst.len();
        dst.resize(init_len + bytes, 0);
        // Record k in the trailing byte.
        dst.push(self.k as u8);

        let array = &mut dst[init_len..init_len + bytes];
        for key in keys {
            let mut h = Self::bloom_hash(key);
            let delta = h.rotate_right(17); // rotate to derive the probe stride
            for _ in 0..self.k {
                let bitpos = (h as usize) % bits;
                array[bitpos / 8] |= 1 << (bitpos % 8);
                h = h.wrapping_add(delta);
            }
        }
    }

    fn key_may_match(&self, key: &[u8], filter: &[u8]) -> bool {
        if filter.len() < 2 {
            return false;
        }
        let bits = (filter.len() - 1) * 8;
        let k = filter[filter.len() - 1] as usize;
        if k > 30 {
            // Reserved for future encodings; treat as a potential match.
            return true;
        }
        let array = &filter[..filter.len() - 1];
        let mut h = Self::bloom_hash(key);
        let delta = h.rotate_right(17);
        for _ in 0..k {
            let bitpos = (h as usize) % bits;
            if array[bitpos / 8] & (1 << (bitpos % 8)) == 0 {
                return false;
            }
            h = h.wrapping_add(delta);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(policy: &BloomFilterPolicy, keys: &[&[u8]]) -> Vec<u8> {
        let mut f = Vec::new();
        policy.create_filter(keys, &mut f);
        f
    }

    #[test]
    fn no_false_negatives() {
        let policy = BloomFilterPolicy::new(10);
        let keys: Vec<Vec<u8>> = (0..1000u32).map(|i| format!("key{i}").into_bytes()).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let filter = build(&policy, &refs);
        // Every inserted key must match.
        for k in &keys {
            assert!(policy.key_may_match(k, &filter), "missing {k:?}");
        }
    }

    #[test]
    fn false_positive_rate_is_low() {
        let policy = BloomFilterPolicy::new(10);
        let keys: Vec<Vec<u8>> = (0..1000u32).map(|i| format!("key{i}").into_bytes()).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let filter = build(&policy, &refs);
        let mut fp = 0;
        for i in 1000..11000u32 {
            let probe = format!("key{i}");
            if policy.key_may_match(probe.as_bytes(), &filter) {
                fp += 1;
            }
        }
        let rate = fp as f64 / 10000.0;
        // 10 bits/key should keep the false-positive rate well under 5%.
        assert!(rate < 0.05, "false positive rate too high: {rate}");
    }

    #[test]
    fn empty_filter_matches_nothing_meaningful() {
        let policy = BloomFilterPolicy::new(10);
        let filter = build(&policy, &[]);
        // With no keys, most probes should miss; at minimum it must not panic.
        let _ = policy.key_may_match(b"anything", &filter);
    }
}
