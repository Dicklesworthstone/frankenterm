//! Exponential histogram for latency/size distribution tracking.
//!
//! Log-scale buckets provide efficient, bounded-memory distribution tracking.
//! Record values in O(1), compute percentiles in O(buckets).
//!
//! # Bucket scheme
//!
//! Bucket boundaries grow exponentially: [0, base), [base, base²), [base², base³), ...
//! Default base = 2.0 gives power-of-two buckets suitable for latency (1ms, 2ms, 4ms, ...).
//!
//! # Use cases in FrankenTerm
//!
//! - **Capture latency**: Track per-pane output capture time distribution.
//! - **Message sizes**: Distribution of IPC message sizes for capacity planning.
//! - **Process lifetimes**: How long agent processes live before termination.
//! - **Polling intervals**: Actual vs. configured polling interval distributions.

use serde::{Deserialize, Serialize};

// =============================================================================
// ExpHistogram
// =============================================================================

/// An exponential (log-scale) histogram.
///
/// Bucket boundaries: [0, base^min_exp), [base^min_exp, base^(min_exp+1)), ...
///
/// # Example
///
/// ```ignore
/// let mut h = ExpHistogram::new(2.0, 0, 20); // base=2, buckets for [1, 2^20)
/// h.record(1.5);
/// h.record(100.0);
/// h.record(1000.0);
/// let p99 = h.percentile(0.99);
/// ```
pub struct ExpHistogram {
    /// Count per bucket.
    buckets: Vec<u64>,
    /// Logarithmic base for bucket boundaries.
    base: f64,
    /// Minimum exponent (bucket 0 covers [0, base^min_exp)).
    min_exp: i32,
    /// Number of buckets.
    num_buckets: usize,
    /// Count of values below the minimum bucket.
    underflow: u64,
    /// Count of values at or above the maximum bucket boundary.
    overflow: u64,
    /// Total number of recorded values.
    count: u64,
    /// Sum of all recorded values.
    sum: f64,
    /// Minimum recorded value.
    min: f64,
    /// Maximum recorded value.
    max: f64,
}

impl ExpHistogram {
    /// Create a new exponential histogram.
    ///
    /// - `base`: logarithmic base (e.g., 2.0 for power-of-two buckets).
    /// - `min_exp`: minimum exponent. Bucket 0 covers [0, base^min_exp).
    /// - `max_exp`: maximum exponent (exclusive). Creates (max_exp - min_exp) buckets.
    ///
    /// # Panics
    ///
    /// Panics if `base <= 1.0` or `max_exp <= min_exp`.
    #[must_use]
    pub fn new(base: f64, min_exp: i32, max_exp: i32) -> Self {
        assert!(base > 1.0, "base must be > 1.0");
        assert!(max_exp > min_exp, "max_exp must be > min_exp");
        let num_buckets = (max_exp - min_exp) as usize;
        Self {
            buckets: vec![0; num_buckets],
            base,
            min_exp,
            num_buckets,
            underflow: 0,
            overflow: 0,
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Create a histogram with power-of-two buckets covering [1, 2^max_exp).
    ///
    /// Convenient for latency tracking in milliseconds.
    #[must_use]
    pub fn power_of_two(max_exp: i32) -> Self {
        Self::new(2.0, 0, max_exp)
    }

    /// Record a value.
    pub fn record(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }

        if value <= 0.0 {
            self.underflow += 1;
            return;
        }

        let exp = value.log(self.base).floor() as i32;
        let bucket_idx = exp - self.min_exp;

        if bucket_idx < 0 {
            self.underflow += 1;
        } else if bucket_idx >= self.num_buckets as i32 {
            self.overflow += 1;
        } else {
            self.buckets[bucket_idx as usize] += 1;
        }
    }

    /// Record multiple occurrences of the same value.
    pub fn record_n(&mut self, value: f64, n: u64) {
        for _ in 0..n {
            self.record(value);
        }
    }

    /// Estimate the value at the given percentile (0.0 to 1.0).
    ///
    /// Returns the upper bound of the bucket containing the target percentile.
    /// Returns `None` if no values have been recorded.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let target = (p * self.count as f64).ceil() as u64;
        let mut cumulative = self.underflow;

        if cumulative >= target {
            return Some(self.base.powi(self.min_exp));
        }

        for (i, &count) in self.buckets.iter().enumerate() {
            cumulative += count;
            if cumulative >= target {
                // Return upper bound of this bucket.
                let exp = self.min_exp + i as i32 + 1;
                return Some(self.base.powi(exp));
            }
        }

        // In the overflow bucket.
        Some(self.max)
    }

    /// Shorthand for common percentiles.
    #[must_use]
    pub fn p50(&self) -> Option<f64> {
        self.percentile(0.50)
    }

    #[must_use]
    pub fn p90(&self) -> Option<f64> {
        self.percentile(0.90)
    }

    #[must_use]
    pub fn p99(&self) -> Option<f64> {
        self.percentile(0.99)
    }

    /// Mean of all recorded values.
    #[must_use]
    pub fn mean(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.sum / self.count as f64)
        }
    }

    /// Minimum recorded value.
    #[must_use]
    pub fn min(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.min)
        }
    }

    /// Maximum recorded value.
    #[must_use]
    pub fn max(&self) -> Option<f64> {
        if self.count == 0 {
            None
        } else {
            Some(self.max)
        }
    }

    /// Total count of recorded values.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Sum of all recorded values.
    #[must_use]
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Number of values that fell below the minimum bucket.
    #[must_use]
    pub fn underflow(&self) -> u64 {
        self.underflow
    }

    /// Number of values that exceeded the maximum bucket.
    #[must_use]
    pub fn overflow(&self) -> u64 {
        self.overflow
    }

    /// Number of buckets.
    #[must_use]
    pub fn num_buckets(&self) -> usize {
        self.num_buckets
    }

    /// Clear all data.
    pub fn clear(&mut self) {
        self.buckets.iter_mut().for_each(|b| *b = 0);
        self.underflow = 0;
        self.overflow = 0;
        self.count = 0;
        self.sum = 0.0;
        self.min = f64::INFINITY;
        self.max = f64::NEG_INFINITY;
    }

    /// Merge another histogram into this one.
    ///
    /// Both must have the same base, min_exp, and bucket count.
    ///
    /// # Panics
    ///
    /// Panics if the histograms have different parameters.
    pub fn merge(&mut self, other: &ExpHistogram) {
        assert!(
            (self.base - other.base).abs() < f64::EPSILON
                && self.min_exp == other.min_exp
                && self.num_buckets == other.num_buckets,
            "histogram parameters must match for merge"
        );

        for (a, b) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            *a += b;
        }
        self.underflow += other.underflow;
        self.overflow += other.overflow;
        self.count += other.count;
        self.sum += other.sum;
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }
    }

    /// Get the bucket boundaries and counts for visualization.
    #[must_use]
    pub fn bucket_details(&self) -> Vec<BucketDetail> {
        let mut details = Vec::with_capacity(self.num_buckets + 2);

        // Underflow bucket.
        if self.underflow > 0 {
            details.push(BucketDetail {
                lower: f64::NEG_INFINITY,
                upper: self.base.powi(self.min_exp),
                count: self.underflow,
            });
        }

        // Regular buckets.
        for (i, &count) in self.buckets.iter().enumerate() {
            if count > 0 {
                let exp = self.min_exp + i as i32;
                details.push(BucketDetail {
                    lower: self.base.powi(exp),
                    upper: self.base.powi(exp + 1),
                    count,
                });
            }
        }

        // Overflow bucket.
        if self.overflow > 0 {
            details.push(BucketDetail {
                lower: self.base.powi(self.min_exp + self.num_buckets as i32),
                upper: f64::INFINITY,
                count: self.overflow,
            });
        }

        details
    }

    /// Get serializable statistics.
    #[must_use]
    pub fn stats(&self) -> HistogramStats {
        HistogramStats {
            count: self.count,
            sum: self.sum,
            mean: self.mean(),
            min: self.min(),
            max: self.max(),
            p50: self.p50(),
            p90: self.p90(),
            p99: self.p99(),
            underflow: self.underflow,
            overflow: self.overflow,
            num_buckets: self.num_buckets,
        }
    }
}

impl std::fmt::Debug for ExpHistogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExpHistogram")
            .field("base", &self.base)
            .field("num_buckets", &self.num_buckets)
            .field("count", &self.count)
            .field("min", &self.min)
            .field("max", &self.max)
            .finish()
    }
}

// =============================================================================
// Supporting types
// =============================================================================

/// Detail about a single bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketDetail {
    /// Lower bound (inclusive).
    pub lower: f64,
    /// Upper bound (exclusive).
    pub upper: f64,
    /// Count of values in this bucket.
    pub count: u64,
}

/// Serializable histogram statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistogramStats {
    /// Total count.
    pub count: u64,
    /// Sum of all values.
    pub sum: f64,
    /// Mean value.
    pub mean: Option<f64>,
    /// Minimum value.
    pub min: Option<f64>,
    /// Maximum value.
    pub max: Option<f64>,
    /// 50th percentile.
    pub p50: Option<f64>,
    /// 90th percentile.
    pub p90: Option<f64>,
    /// 99th percentile.
    pub p99: Option<f64>,
    /// Underflow count.
    pub underflow: u64,
    /// Overflow count.
    pub overflow: u64,
    /// Number of buckets.
    pub num_buckets: usize,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Basic ------------------------------------------------------------------

    #[test]
    fn empty_histogram() {
        let h = ExpHistogram::power_of_two(20);
        assert_eq!(h.count(), 0);
        assert_eq!(h.percentile(0.5), None);
        assert_eq!(h.mean(), None);
        assert_eq!(h.min(), None);
        assert_eq!(h.max(), None);
    }

    #[test]
    fn single_value() {
        let mut h = ExpHistogram::power_of_two(20);
        h.record(100.0);
        assert_eq!(h.count(), 1);
        assert_eq!(h.min(), Some(100.0));
        assert_eq!(h.max(), Some(100.0));
        assert_eq!(h.mean(), Some(100.0));
    }

    #[test]
    fn multiple_values() {
        let mut h = ExpHistogram::power_of_two(20);
        h.record(1.0);
        h.record(10.0);
        h.record(100.0);
        h.record(1000.0);
        assert_eq!(h.count(), 4);
        assert_eq!(h.min(), Some(1.0));
        assert_eq!(h.max(), Some(1000.0));
    }

    #[test]
    fn sum_and_mean() {
        let mut h = ExpHistogram::power_of_two(20);
        h.record(10.0);
        h.record(20.0);
        h.record(30.0);
        assert!((h.sum() - 60.0).abs() < f64::EPSILON);
        assert!((h.mean().unwrap() - 20.0).abs() < f64::EPSILON);
    }

    // -- Bucket placement -------------------------------------------------------

    #[test]
    fn power_of_two_buckets() {
        let mut h = ExpHistogram::power_of_two(10);
        // Value 1.5 should go in bucket [1, 2) = bucket 0
        h.record(1.5);
        assert_eq!(h.buckets[0], 1);

        // Value 3.0 should go in bucket [2, 4) = bucket 1
        h.record(3.0);
        assert_eq!(h.buckets[1], 1);

        // Value 500.0 should go in bucket [256, 512) = bucket 8
        h.record(500.0);
        assert_eq!(h.buckets[8], 1);
    }

    #[test]
    fn underflow() {
        let mut h = ExpHistogram::new(2.0, 2, 10); // min = 2^2 = 4
        h.record(1.0); // below minimum
        h.record(3.0); // below minimum (3 < 4)
        assert_eq!(h.underflow(), 2);
    }

    #[test]
    fn overflow() {
        let mut h = ExpHistogram::power_of_two(5); // max = 2^5 = 32
        h.record(100.0); // above max
        assert_eq!(h.overflow(), 1);
    }

    #[test]
    fn zero_value() {
        let mut h = ExpHistogram::power_of_two(10);
        h.record(0.0);
        assert_eq!(h.underflow(), 1);
    }

    #[test]
    fn negative_value() {
        let mut h = ExpHistogram::power_of_two(10);
        h.record(-5.0);
        assert_eq!(h.underflow(), 1);
    }

    // -- Percentiles ------------------------------------------------------------

    #[test]
    fn percentile_all_same_bucket() {
        let mut h = ExpHistogram::power_of_two(20);
        for _ in 0..100 {
            h.record(5.0); // all in [4, 8) = bucket 2
        }
        // p50 should be upper bound of bucket 2 = 8.0
        assert_eq!(h.p50(), Some(8.0));
        assert_eq!(h.p99(), Some(8.0));
    }

    #[test]
    fn percentile_spread() {
        let mut h = ExpHistogram::power_of_two(20);
        // 90 fast values (1-2ms), 10 slow values (1024-2048ms)
        for _ in 0..90 {
            h.record(1.5);
        }
        for _ in 0..10 {
            h.record(1500.0);
        }
        // p50 should be in the fast bucket
        let p50 = h.p50().unwrap();
        assert!(p50 <= 4.0, "p50={p50}, expected <= 4");

        // p99 should be in the slow bucket
        let p99 = h.p99().unwrap();
        assert!(p99 >= 1024.0, "p99={p99}, expected >= 1024");
    }

    #[test]
    fn percentile_boundaries() {
        let mut h = ExpHistogram::power_of_two(10);
        h.record(1.5);
        // p(0.0): ceil(0) = 0, underflow=0 doesn't reach target, bucket 0 has it → upper=2.0
        // But ceil(0.0 * 1) = 0, and 0 >= 0 is true for underflow check
        let p0 = h.percentile(0.0).unwrap();
        assert!(p0 <= 2.0, "p0={p0}");
        assert_eq!(h.percentile(1.0), Some(2.0));
    }

    // -- Custom base ------------------------------------------------------------

    #[test]
    fn base_10() {
        let mut h = ExpHistogram::new(10.0, 0, 6); // [1, 10, 100, ..., 10^6)
        h.record(50.0); // [10, 100) = bucket 1
        h.record(5000.0); // [1000, 10000) = bucket 3
        assert_eq!(h.buckets[1], 1);
        assert_eq!(h.buckets[3], 1);
    }

    // -- Clear ------------------------------------------------------------------

    #[test]
    fn clear() {
        let mut h = ExpHistogram::power_of_two(10);
        h.record(5.0);
        h.record(50.0);
        h.clear();
        assert_eq!(h.count(), 0);
        assert_eq!(h.sum(), 0.0);
        assert_eq!(h.min(), None);
    }

    // -- Merge ------------------------------------------------------------------

    #[test]
    fn merge() {
        let mut h1 = ExpHistogram::power_of_two(10);
        let mut h2 = ExpHistogram::power_of_two(10);

        h1.record(5.0);
        h1.record(50.0);

        h2.record(500.0);
        h2.record(1.0);

        h1.merge(&h2);
        assert_eq!(h1.count(), 4);
        assert_eq!(h1.min(), Some(1.0));
        assert_eq!(h1.max(), Some(500.0));
    }

    #[test]
    #[should_panic(expected = "histogram parameters must match")]
    fn merge_mismatch_panics() {
        let mut h1 = ExpHistogram::power_of_two(10);
        let h2 = ExpHistogram::power_of_two(20);
        h1.merge(&h2);
    }

    // -- Bucket details ---------------------------------------------------------

    #[test]
    fn bucket_details() {
        let mut h = ExpHistogram::power_of_two(10);
        h.record(5.0);
        h.record(50.0);
        let details = h.bucket_details();
        assert_eq!(details.len(), 2); // two non-empty buckets
        assert!(details.iter().all(|d| d.count > 0));
    }

    #[test]
    fn bucket_details_with_overflow() {
        let mut h = ExpHistogram::power_of_two(5);
        h.record(100.0); // overflow
        let details = h.bucket_details();
        assert_eq!(details.len(), 1);
        assert!(details[0].upper.is_infinite());
    }

    // -- Stats ------------------------------------------------------------------

    #[test]
    fn stats() {
        let mut h = ExpHistogram::power_of_two(20);
        h.record(1.0);
        h.record(100.0);
        h.record(10000.0);
        let s = h.stats();
        assert_eq!(s.count, 3);
        assert!(s.p50.is_some());
        assert!(s.p99.is_some());
    }

    #[test]
    fn stats_serde_roundtrip() {
        let mut h = ExpHistogram::power_of_two(20);
        h.record(42.0);
        let s = h.stats();
        let json = serde_json::to_string(&s).unwrap();
        let back: HistogramStats = serde_json::from_str(&json).unwrap();
        assert_eq!(s.count, back.count);
    }

    // -- Debug ------------------------------------------------------------------

    #[test]
    fn debug_format() {
        let h = ExpHistogram::power_of_two(10);
        let s = format!("{h:?}");
        assert!(s.contains("ExpHistogram"));
    }

    // -- Panics -----------------------------------------------------------------

    #[test]
    #[should_panic(expected = "base must be > 1.0")]
    fn base_one_panics() {
        let _ = ExpHistogram::new(1.0, 0, 10);
    }

    #[test]
    #[should_panic(expected = "max_exp must be > min_exp")]
    fn equal_exp_panics() {
        let _ = ExpHistogram::new(2.0, 5, 5);
    }

    // -- record_n ---------------------------------------------------------------

    #[test]
    fn record_n() {
        let mut h = ExpHistogram::power_of_two(10);
        h.record_n(5.0, 100);
        assert_eq!(h.count(), 100);
        assert!((h.sum() - 500.0).abs() < f64::EPSILON);
    }
}
