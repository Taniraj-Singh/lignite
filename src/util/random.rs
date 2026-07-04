//! A tiny, deterministic pseudo-random generator (a Park–Miller multiplicative
//! LCG modulo 2^31 − 1). It is *not* cryptographically secure; it exists so that
//! skiplist level selection and the test harness produce reproducible sequences
//! from a fixed seed.

/// Park–Miller minimal standard generator.
#[derive(Debug, Clone)]
pub struct Random {
    seed: u32,
}

impl Random {
    const M: u32 = 2_147_483_647; // 2^31 - 1 (a Mersenne prime)
    const A: u64 = 16_807; // 7^5

    /// Create a generator from `seed`. The seed is normalised into the open
    /// interval `(0, M)` so the stream never collapses to all-zero.
    pub fn new(seed: u32) -> Self {
        let mut s = seed & 0x7fff_ffff;
        if s == 0 || s == Self::M {
            s = 1;
        }
        Random { seed: s }
    }

    /// Return the next value in `[0, 2^31 - 1)`.
    pub fn next(&mut self) -> u32 {
        // seed = (seed * A) % M, computed in 64 bits to avoid overflow.
        let product = (self.seed as u64).wrapping_mul(Self::A);
        // (product % M) via the fold used for Mersenne moduli.
        let lo = (product & Self::M as u64) as u32;
        let hi = (product >> 31) as u32;
        let mut result = lo.wrapping_add(hi);
        if result >= Self::M {
            result = result.wrapping_sub(Self::M);
        }
        self.seed = result;
        result
    }

    /// Return a value uniformly in `[0, n)`. `n` must be non-zero.
    pub fn uniform(&mut self, n: u32) -> u32 {
        self.next() % n
    }

    /// Return `true` with probability `1/n` (a Bernoulli trial). Used by the
    /// skiplist to decide whether to promote a node to the next level.
    pub fn one_in(&mut self, n: u32) -> bool {
        self.uniform(n) == 0
    }

    /// Skewed draw biased towards smaller values: returns a number in
    /// `[0, 2^max_log]` where each successive bit is half as likely.
    pub fn skewed(&mut self, max_log: u32) -> u32 {
        let base = 1u32 << self.uniform(max_log + 1);
        self.uniform(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_from_seed() {
        let mut a = Random::new(42);
        let mut b = Random::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next(), b.next());
        }
    }

    #[test]
    fn stays_in_range() {
        let mut r = Random::new(7);
        for _ in 0..10_000 {
            let v = r.uniform(100);
            assert!(v < 100);
        }
    }

    #[test]
    fn one_in_is_roughly_fair() {
        let mut r = Random::new(123);
        let mut hits = 0;
        let trials = 40_000;
        for _ in 0..trials {
            if r.one_in(4) {
                hits += 1;
            }
        }
        let ratio = hits as f64 / trials as f64;
        assert!((ratio - 0.25).abs() < 0.02, "ratio was {ratio}");
    }
}
