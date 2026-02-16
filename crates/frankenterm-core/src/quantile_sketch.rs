//! T-Digest streaming quantile sketch for approximate percentile estimation.
//!
//! Provides O(1) amortized insertion and O(log n) percentile queries with
//! bounded relative error. Useful for SLO tracking across many panes without
//! pre-bucketing or histogram calibration.
//!
//! # Algorithm
//!
//! The t-digest (Dunning & Ertl, 2019) maintains a sorted set of centroids,
//! each representing a cluster of nearby values. The key insight is the
//! **scale function** that controls how centroids grow:
//! - Centroids near the tails (p≈0, p≈1) are kept small for high accuracy
//! - Centroids near the median can be large since less precision is needed
//!
//! This gives relative error guarantees: ε(q) ≈ O(1/δ) where δ is the
//! compression parameter, with tighter bounds at extreme quantiles.
//!
//! Bead: ft-283h4.20

use serde::{Deserialize, Serialize};

/// A single centroid in the t-digest: mean + weight (count).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Centroid {
    pub mean: f64,
    pub weight: f64,
}

impl Centroid {
    fn new(mean: f64, weight: f64) -> Self {
        Self { mean, weight }
    }

    /// Merge another centroid into this one (weighted mean update).
    fn merge(&mut self, other: &Centroid) {
        let total = self.weight + other.weight;
        if total > 0.0 {
            self.mean = (self.mean * self.weight + other.mean * other.weight) / total;
            self.weight = total;
        }
    }
}

/// Configuration for the t-digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TDigestConfig {
    /// Compression parameter δ. Higher = more centroids = more accuracy.
    /// Typical values: 100-300. Default: 100.
    pub compression: f64,
    /// Max unmerged buffer size before triggering compression.
    /// Default: 5 * compression.
    pub buffer_size: usize,
}

impl Default for TDigestConfig {
    fn default() -> Self {
        Self {
            compression: 100.0,
            buffer_size: 500,
        }
    }
}

/// Statistics about the t-digest state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TDigestStats {
    pub centroid_count: usize,
    pub total_weight: f64,
    pub buffer_len: usize,
    pub compression: f64,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Streaming quantile estimator using the t-digest algorithm.
///
/// # Example
/// ```
/// use frankenterm_core::quantile_sketch::TDigest;
///
/// let mut td = TDigest::new();
/// for i in 0..1000 {
///     td.insert(i as f64);
/// }
/// let p50 = td.quantile(0.5);
/// assert!((p50 - 500.0).abs() < 20.0);
/// ```
#[derive(Debug, Clone)]
pub struct TDigest {
    centroids: Vec<Centroid>,
    buffer: Vec<f64>,
    total_weight: f64,
    min: f64,
    max: f64,
    config: TDigestConfig,
}

impl TDigest {
    /// Create a new t-digest with default compression (δ=100).
    pub fn new() -> Self {
        Self::with_config(TDigestConfig::default())
    }

    /// Create a new t-digest with the given compression parameter.
    pub fn with_compression(compression: f64) -> Self {
        let compression = compression.max(10.0);
        Self::with_config(TDigestConfig {
            compression,
            buffer_size: (5.0 * compression) as usize,
        })
    }

    /// Create a new t-digest with full configuration.
    pub fn with_config(config: TDigestConfig) -> Self {
        Self {
            centroids: Vec::new(),
            buffer: Vec::new(),
            total_weight: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            config,
        }
    }

    /// Insert a single value.
    pub fn insert(&mut self, value: f64) {
        if value.is_nan() {
            return;
        }
        self.buffer.push(value);
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
        self.total_weight += 1.0;

        if self.buffer.len() >= self.config.buffer_size {
            self.compress();
        }
    }

    /// Insert a weighted value.
    pub fn insert_weighted(&mut self, value: f64, weight: f64) {
        if value.is_nan() || weight <= 0.0 {
            return;
        }
        // For weighted inserts, add directly as centroid to buffer
        // and track min/max
        if value < self.min {
            self.min = value;
        }
        if value > self.max {
            self.max = value;
        }
        self.total_weight += weight;
        self.centroids.push(Centroid::new(value, weight));

        if self.centroids.len() + self.buffer.len() >= self.config.buffer_size {
            self.compress();
        }
    }

    /// Estimate the value at the given quantile (0.0 to 1.0).
    ///
    /// Returns 0.0 if the digest is empty.
    pub fn quantile(&mut self, q: f64) -> f64 {
        let q = q.clamp(0.0, 1.0);

        // Flush buffer first
        if !self.buffer.is_empty() {
            self.compress();
        }

        if self.centroids.is_empty() {
            return 0.0;
        }

        // Handle extreme quantiles first (before single-centroid fallback)
        if q <= 0.0 {
            return self.min;
        }
        if q >= 1.0 {
            return self.max;
        }

        if self.centroids.len() == 1 {
            return self.centroids[0].mean;
        }

        let total = self.weight_sum();
        let target = q * total;

        // Walk centroids accumulating weight
        let mut cumulative = 0.0;
        for i in 0..self.centroids.len() {
            let c = &self.centroids[i];
            let half_w = c.weight / 2.0;

            if cumulative + half_w >= target {
                // Target is in the left half of this centroid
                if i == 0 {
                    // Interpolate between min and centroid mean
                    let t = if half_w > 0.0 {
                        (target - cumulative) / half_w
                    } else {
                        0.5
                    };
                    return self.min + t * (c.mean - self.min);
                }
                // Interpolate between previous centroid mean and this one
                let prev = &self.centroids[i - 1];
                let gap = c.mean - prev.mean;
                let prev_half_w = prev.weight / 2.0;
                let local_target = target - (cumulative - prev_half_w);
                let local_total = prev_half_w + half_w;
                let t = if local_total > 0.0 {
                    local_target / local_total
                } else {
                    0.5
                };
                return prev.mean + t * gap;
            }

            cumulative += c.weight;

            if cumulative >= target {
                // Target is in the right half of this centroid
                if i == self.centroids.len() - 1 {
                    // Interpolate between centroid mean and max
                    let t = if half_w > 0.0 {
                        (target - (cumulative - half_w)) / half_w
                    } else {
                        0.5
                    };
                    return c.mean + t * (self.max - c.mean);
                }
                let next = &self.centroids[i + 1];
                let gap = next.mean - c.mean;
                let next_half_w = next.weight / 2.0;
                let local_target = target - (cumulative - half_w);
                let local_total = half_w + next_half_w;
                let t = if local_total > 0.0 {
                    local_target / local_total
                } else {
                    0.5
                };
                return c.mean + t * gap;
            }
        }

        self.max
    }

    /// Estimate the CDF value (proportion <= x).
    pub fn cdf(&mut self, x: f64) -> f64 {
        if !self.buffer.is_empty() {
            self.compress();
        }

        if self.centroids.is_empty() {
            return 0.0;
        }

        if x <= self.min {
            return 0.0;
        }
        if x >= self.max {
            return 1.0;
        }

        let total = self.weight_sum();
        if total == 0.0 {
            return 0.0;
        }

        let mut cumulative = 0.0;
        for i in 0..self.centroids.len() {
            let c = &self.centroids[i];

            if x < c.mean {
                // Interpolate
                if i == 0 {
                    let t = if c.mean > self.min {
                        (x - self.min) / (c.mean - self.min)
                    } else {
                        0.5
                    };
                    return (cumulative + t * c.weight / 2.0) / total;
                }
                let prev = &self.centroids[i - 1];
                let t = if c.mean > prev.mean {
                    (x - prev.mean) / (c.mean - prev.mean)
                } else {
                    0.5
                };
                let prev_contrib = prev.weight / 2.0;
                let curr_contrib = c.weight / 2.0;
                return (cumulative - prev_contrib + t * (prev_contrib + curr_contrib)) / total;
            }

            cumulative += c.weight;
        }

        1.0
    }

    /// Number of values inserted.
    pub fn count(&self) -> f64 {
        self.total_weight
    }

    /// Whether the digest is empty.
    pub fn is_empty(&self) -> bool {
        self.total_weight == 0.0
    }

    /// Minimum value seen.
    pub fn min(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.min)
        }
    }

    /// Maximum value seen.
    pub fn max(&self) -> Option<f64> {
        if self.is_empty() {
            None
        } else {
            Some(self.max)
        }
    }

    /// Mean of all values.
    pub fn mean(&mut self) -> f64 {
        if !self.buffer.is_empty() {
            self.compress();
        }
        if self.centroids.is_empty() {
            return 0.0;
        }
        let total_w = self.weight_sum();
        if total_w == 0.0 {
            return 0.0;
        }
        self.centroids
            .iter()
            .map(|c| c.mean * c.weight)
            .sum::<f64>()
            / total_w
    }

    /// Number of centroids currently stored.
    pub fn centroid_count(&self) -> usize {
        self.centroids.len()
    }

    /// Get statistics about the digest.
    pub fn stats(&self) -> TDigestStats {
        TDigestStats {
            centroid_count: self.centroids.len(),
            total_weight: self.total_weight,
            buffer_len: self.buffer.len(),
            compression: self.config.compression,
            min: self.min(),
            max: self.max(),
        }
    }

    /// Merge another t-digest into this one.
    pub fn merge(&mut self, other: &TDigest) {
        if other.is_empty() {
            return;
        }

        // Flush our buffer
        if !self.buffer.is_empty() {
            self.compress();
        }

        // Add other's centroids and buffer
        for c in &other.centroids {
            self.centroids.push(c.clone());
        }
        for &v in &other.buffer {
            self.buffer.push(v);
        }

        self.total_weight += other.total_weight;
        if other.min < self.min {
            self.min = other.min;
        }
        if other.max > self.max {
            self.max = other.max;
        }

        self.compress();
    }

    /// Clear all data.
    pub fn clear(&mut self) {
        self.centroids.clear();
        self.buffer.clear();
        self.total_weight = 0.0;
        self.min = f64::INFINITY;
        self.max = f64::NEG_INFINITY;
    }

    /// Reset with a new compression parameter.
    pub fn reset(&mut self, compression: f64) {
        self.clear();
        self.config.compression = compression.max(10.0);
        self.config.buffer_size = (5.0 * self.config.compression) as usize;
    }

    // ── Internal ──────────────────────────────────────────────────────

    fn weight_sum(&self) -> f64 {
        self.centroids.iter().map(|c| c.weight).sum()
    }

    /// Compress buffer into centroids using the t-digest merge algorithm.
    fn compress(&mut self) {
        // Add buffer values as unit-weight centroids
        for &v in &self.buffer {
            self.centroids.push(Centroid::new(v, 1.0));
        }
        self.buffer.clear();

        if self.centroids.is_empty() {
            return;
        }

        // Sort centroids by mean
        self.centroids.sort_by(|a, b| {
            a.mean
                .partial_cmp(&b.mean)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let total_weight: f64 = self.centroids.iter().map(|c| c.weight).sum();
        if total_weight == 0.0 {
            return;
        }

        let delta = self.config.compression;

        // Merge pass: greedily merge centroids while respecting the scale function constraint
        let mut merged: Vec<Centroid> = Vec::with_capacity(self.centroids.len());
        let mut cumulative = 0.0;

        for centroid in &self.centroids {
            if merged.is_empty() {
                merged.push(centroid.clone());
                cumulative += centroid.weight;
                continue;
            }

            let last = merged.last().unwrap();
            let proposed_weight = last.weight + centroid.weight;

            // Scale function k₁(q) = (δ/2π) · arcsin(2q - 1).
            // The constraint is that a merged centroid must span at most 1 unit
            // in k-space: k(q_right) - k(q_left) <= 1.
            let q_left = (cumulative - last.weight) / total_weight;
            let q_right = (cumulative + centroid.weight) / total_weight;

            let k_left = self.scale_fn(q_left, delta);
            let k_right = self.scale_fn(q_right, delta);

            if k_right - k_left <= 1.0 {
                // Safe to merge
                let last_mut = merged.last_mut().unwrap();
                last_mut.merge(centroid);
                cumulative += centroid.weight;
            } else {
                // Start a new centroid
                merged.push(centroid.clone());
                cumulative += centroid.weight;
            }
        }

        self.centroids = merged;
    }

    /// Scale function k₁(q) = δ/2π · arcsin(2q - 1).
    /// Maps quantile [0,1] → index space, with steep slopes at tails.
    fn scale_fn(&self, q: f64, delta: f64) -> f64 {
        let q = q.clamp(0.0, 1.0);
        (delta / (2.0 * std::f64::consts::PI)) * ((2.0 * q - 1.0).asin())
    }
}

impl Default for TDigest {
    fn default() -> Self {
        Self::new()
    }
}

// ── Convenience constructors ──────────────────────────────────────────

/// Create a t-digest from an iterator of values.
impl FromIterator<f64> for TDigest {
    fn from_iter<I: IntoIterator<Item = f64>>(iter: I) -> Self {
        let mut td = TDigest::new();
        for v in iter {
            td.insert(v);
        }
        td
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_digest() {
        let mut td = TDigest::new();
        assert!(td.is_empty());
        assert_eq!(td.count(), 0.0);
        assert_eq!(td.min(), None);
        assert_eq!(td.max(), None);
        assert_eq!(td.quantile(0.5), 0.0);
    }

    #[test]
    fn single_value() {
        let mut td = TDigest::new();
        td.insert(42.0);
        assert_eq!(td.count(), 1.0);
        assert_eq!(td.min(), Some(42.0));
        assert_eq!(td.max(), Some(42.0));
        assert_eq!(td.quantile(0.5), 42.0);
    }

    #[test]
    fn two_values() {
        let mut td = TDigest::new();
        td.insert(10.0);
        td.insert(20.0);
        let median = td.quantile(0.5);
        assert!(
            (median - 15.0).abs() < 5.1,
            "median {} should be near 15",
            median
        );
    }

    #[test]
    fn uniform_distribution() {
        let mut td = TDigest::new();
        for i in 0..1000 {
            td.insert(i as f64);
        }
        let p50 = td.quantile(0.5);
        assert!(
            (p50 - 499.5).abs() < 30.0,
            "p50={} should be near 499.5",
            p50
        );
        let p95 = td.quantile(0.95);
        assert!(
            (p95 - 949.5).abs() < 30.0,
            "p95={} should be near 949.5",
            p95
        );
    }

    #[test]
    fn min_max_tracked() {
        let mut td = TDigest::new();
        td.insert(100.0);
        td.insert(-50.0);
        td.insert(200.0);
        assert_eq!(td.min(), Some(-50.0));
        assert_eq!(td.max(), Some(200.0));
    }

    #[test]
    fn nan_ignored() {
        let mut td = TDigest::new();
        td.insert(f64::NAN);
        assert!(td.is_empty());
        assert_eq!(td.count(), 0.0);
    }

    #[test]
    fn cdf_basic() {
        let mut td = TDigest::new();
        for i in 0..100 {
            td.insert(i as f64);
        }
        let cdf_50 = td.cdf(50.0);
        assert!(
            (cdf_50 - 0.5).abs() < 0.15,
            "cdf(50)={} should be near 0.5",
            cdf_50
        );
        assert_eq!(td.cdf(-1.0), 0.0);
        assert_eq!(td.cdf(200.0), 1.0);
    }

    #[test]
    fn mean_correct() {
        let mut td = TDigest::new();
        for i in 0..100 {
            td.insert(i as f64);
        }
        let mean = td.mean();
        assert!(
            (mean - 49.5).abs() < 1.0,
            "mean={} should be near 49.5",
            mean
        );
    }

    #[test]
    fn merge_two_digests() {
        let mut td1 = TDigest::new();
        let mut td2 = TDigest::new();
        for i in 0..500 {
            td1.insert(i as f64);
        }
        for i in 500..1000 {
            td2.insert(i as f64);
        }
        td1.merge(&td2);
        assert_eq!(td1.count(), 1000.0);
        let p50 = td1.quantile(0.5);
        assert!(
            (p50 - 499.5).abs() < 30.0,
            "p50={} should be near 499.5",
            p50
        );
    }

    #[test]
    fn clear_resets() {
        let mut td = TDigest::new();
        for i in 0..100 {
            td.insert(i as f64);
        }
        td.clear();
        assert!(td.is_empty());
        assert_eq!(td.count(), 0.0);
        assert_eq!(td.centroid_count(), 0);
    }

    #[test]
    fn compression_controls_centroid_count() {
        let mut low = TDigest::with_compression(20.0);
        let mut high = TDigest::with_compression(200.0);
        for i in 0..5000 {
            low.insert(i as f64);
            high.insert(i as f64);
        }
        // Force flush
        let _ = low.quantile(0.5);
        let _ = high.quantile(0.5);
        assert!(
            low.centroid_count() < high.centroid_count(),
            "low compression ({}) should have fewer centroids than high ({})",
            low.centroid_count(),
            high.centroid_count()
        );
    }

    #[test]
    fn stats_consistent() {
        let mut td = TDigest::new();
        for i in 0..100 {
            td.insert(i as f64);
        }
        let stats = td.stats();
        assert_eq!(stats.total_weight, 100.0);
        assert_eq!(stats.min, Some(0.0));
        assert_eq!(stats.max, Some(99.0));
    }

    #[test]
    fn from_iterator() {
        let td: TDigest = (0..100).map(|i| i as f64).collect();
        assert_eq!(td.count(), 100.0);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = TDigestConfig {
            compression: 150.0,
            buffer_size: 750,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: TDigestConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let stats = TDigestStats {
            centroid_count: 50,
            total_weight: 1000.0,
            buffer_len: 0,
            compression: 100.0,
            min: Some(0.0),
            max: Some(999.0),
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: TDigestStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn weighted_insert() {
        let mut td = TDigest::new();
        td.insert_weighted(10.0, 100.0);
        td.insert_weighted(20.0, 100.0);
        assert_eq!(td.count(), 200.0);
        let p50 = td.quantile(0.5);
        assert!((p50 - 15.0).abs() < 6.0, "p50={} should be near 15", p50);
    }

    #[test]
    fn extreme_quantiles() {
        let mut td = TDigest::new();
        for i in 0..1000 {
            td.insert(i as f64);
        }
        let p0 = td.quantile(0.0);
        let p100 = td.quantile(1.0);
        assert_eq!(p0, 0.0);
        assert_eq!(p100, 999.0);
    }

    #[test]
    fn reset_changes_compression() {
        let mut td = TDigest::with_compression(50.0);
        for i in 0..100 {
            td.insert(i as f64);
        }
        td.reset(200.0);
        assert!(td.is_empty());
        assert_eq!(td.config.compression, 200.0);
    }

    #[test]
    fn merge_empty() {
        let mut td1 = TDigest::new();
        for i in 0..100 {
            td1.insert(i as f64);
        }
        let td2 = TDigest::new();
        td1.merge(&td2);
        assert_eq!(td1.count(), 100.0);
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ──

    #[test]
    fn centroid_debug_clone_serde() {
        let c = Centroid::new(3.14, 2.0);
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("Centroid"));
        let c2 = c.clone();
        assert_eq!(c, c2);
        let json = serde_json::to_string(&c).unwrap();
        let back: Centroid = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn tdigest_config_default_values() {
        let cfg = TDigestConfig::default();
        assert!((cfg.compression - 100.0).abs() < f64::EPSILON);
        assert_eq!(cfg.buffer_size, 500);
    }

    #[test]
    fn tdigest_config_debug_clone() {
        let cfg = TDigestConfig::default();
        let dbg = format!("{:?}", cfg);
        assert!(dbg.contains("TDigestConfig"));
        let c = cfg.clone();
        assert_eq!(c, cfg);
    }

    #[test]
    fn tdigest_stats_debug_clone() {
        let stats = TDigestStats {
            centroid_count: 10,
            total_weight: 100.0,
            buffer_len: 5,
            compression: 100.0,
            min: Some(0.0),
            max: Some(99.0),
        };
        let dbg = format!("{:?}", stats);
        assert!(dbg.contains("TDigestStats"));
        let s2 = stats.clone();
        assert_eq!(s2, stats);
    }

    #[test]
    fn tdigest_default_trait() {
        let td = TDigest::default();
        assert!(td.is_empty());
        assert_eq!(td.count(), 0.0);
    }

    #[test]
    fn tdigest_debug_clone() {
        let mut td = TDigest::new();
        td.insert(42.0);
        let dbg = format!("{:?}", td);
        assert!(dbg.contains("TDigest"));
        let td2 = td.clone();
        assert_eq!(td2.count(), 1.0);
    }

    #[test]
    fn insert_weighted_zero_weight_ignored() {
        let mut td = TDigest::new();
        td.insert_weighted(10.0, 0.0);
        assert!(td.is_empty());
    }

    #[test]
    fn insert_weighted_negative_weight_ignored() {
        let mut td = TDigest::new();
        td.insert_weighted(10.0, -5.0);
        assert!(td.is_empty());
    }

    #[test]
    fn insert_weighted_nan_value_ignored() {
        let mut td = TDigest::new();
        td.insert_weighted(f64::NAN, 10.0);
        assert!(td.is_empty());
    }

    #[test]
    fn cdf_empty_returns_zero() {
        let mut td = TDigest::new();
        assert_eq!(td.cdf(50.0), 0.0);
    }

    #[test]
    fn mean_empty_returns_zero() {
        let mut td = TDigest::new();
        assert_eq!(td.mean(), 0.0);
    }

    #[test]
    fn with_compression_clamps_minimum() {
        let td = TDigest::with_compression(1.0);
        // compression should be clamped to at least 10.0
        assert!(!td.is_empty() || td.count() == 0.0);
        let stats = td.stats();
        assert!((stats.compression - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn quantile_clamped_above_one() {
        let mut td = TDigest::new();
        td.insert(10.0);
        td.insert(20.0);
        // q > 1.0 should be clamped to 1.0 and return max
        let result = td.quantile(2.0);
        assert_eq!(result, 20.0);
    }

    #[test]
    fn quantile_clamped_below_zero() {
        let mut td = TDigest::new();
        td.insert(10.0);
        td.insert(20.0);
        // q < 0.0 should be clamped to 0.0 and return min
        let result = td.quantile(-1.0);
        assert_eq!(result, 10.0);
    }
}
