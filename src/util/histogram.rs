//! A small fixed-bucket histogram for latency/size distributions.
//!
//! It records values into exponentially-widening buckets and can report the
//! count, min, max, mean and approximate percentiles. It is deliberately
//! allocation-free after construction and cheap enough to update on a hot path,
//! trading exact percentiles for bounded, predictable memory. The engine can use
//! it to summarise, say, per-operation latencies or flushed-file sizes.

/// Upper bounds (inclusive) of each bucket. The final bucket catches everything
/// larger. Chosen to give fine resolution for small values and coarse resolution
/// for large ones.
const BUCKET_BOUNDS: &[f64] = &[
    1.0, 2.0, 3.0, 4.0, 6.0, 8.0, 12.0, 16.0, 24.0, 32.0, 48.0, 64.0, 96.0, 128.0, 192.0, 256.0,
    384.0, 512.0, 768.0, 1024.0, 1536.0, 2048.0, 3072.0, 4096.0, 6144.0, 8192.0, 12288.0, 16384.0,
    32768.0, 65536.0, 131072.0, 262144.0, 524288.0, 1048576.0,
];

/// A bucketed distribution of `f64` samples.
#[derive(Clone)]
pub struct Histogram {
    buckets: Vec<u64>,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Histogram {
    /// Create an empty histogram.
    pub fn new() -> Self {
        Histogram {
            buckets: vec![0; BUCKET_BOUNDS.len() + 1],
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Record one sample.
    pub fn add(&mut self, value: f64) {
        let idx = BUCKET_BOUNDS
            .iter()
            .position(|&b| value <= b)
            .unwrap_or(BUCKET_BOUNDS.len());
        self.buckets[idx] += 1;
        self.count += 1;
        self.sum += value;
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
    }

    /// Number of samples recorded.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// The smallest recorded sample, or `None` if empty.
    pub fn min(&self) -> Option<f64> {
        (self.count > 0).then_some(self.min)
    }

    /// The largest recorded sample, or `None` if empty.
    pub fn max(&self) -> Option<f64> {
        (self.count > 0).then_some(self.max)
    }

    /// The arithmetic mean, or `None` if empty.
    pub fn mean(&self) -> Option<f64> {
        (self.count > 0).then(|| self.sum / self.count as f64)
    }

    /// The approximate value at percentile `p` (0.0..=100.0).
    ///
    /// Interpolates within the bucket that contains the target rank, clamped to
    /// the observed `[min, max]` range.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let threshold = (p / 100.0) * self.count as f64;
        let mut cumulative = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            cumulative += c;
            if cumulative as f64 >= threshold {
                let upper = if i < BUCKET_BOUNDS.len() {
                    BUCKET_BOUNDS[i]
                } else {
                    self.max
                };
                let lower = if i == 0 { self.min } else { BUCKET_BOUNDS[i - 1] };
                let mid = (lower + upper) / 2.0;
                return Some(mid.clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }

    /// Merge another histogram into this one.
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.buckets.iter_mut().zip(&other.buckets) {
            *a += b;
        }
        self.count += other.count;
        self.sum += other.sum;
        if other.count > 0 {
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
        }
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_reports_none() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert!(h.mean().is_none());
        assert!(h.percentile(50.0).is_none());
    }

    #[test]
    fn basic_stats() {
        let mut h = Histogram::new();
        for v in [1.0, 2.0, 3.0, 4.0, 5.0] {
            h.add(v);
        }
        assert_eq!(h.count(), 5);
        assert_eq!(h.min(), Some(1.0));
        assert_eq!(h.max(), Some(5.0));
        assert_eq!(h.mean(), Some(3.0));
    }

    #[test]
    fn percentiles_are_monotone_and_bounded() {
        let mut h = Histogram::new();
        for i in 1..=1000 {
            h.add(i as f64);
        }
        let p50 = h.percentile(50.0).unwrap();
        let p90 = h.percentile(90.0).unwrap();
        let p99 = h.percentile(99.0).unwrap();
        assert!(p50 <= p90 && p90 <= p99);
        assert!(p99 <= 1000.0);
        assert!(p50 >= 1.0);
    }

    #[test]
    fn large_values_land_in_overflow_bucket() {
        let mut h = Histogram::new();
        h.add(10_000_000.0);
        assert_eq!(h.count(), 1);
        assert_eq!(h.max(), Some(10_000_000.0));
        assert_eq!(h.percentile(100.0), Some(10_000_000.0));
    }

    #[test]
    fn merge_combines_distributions() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        for v in [1.0, 2.0, 3.0] {
            a.add(v);
        }
        for v in [4.0, 5.0, 6.0] {
            b.add(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), 6);
        assert_eq!(a.min(), Some(1.0));
        assert_eq!(a.max(), Some(6.0));
    }
}
