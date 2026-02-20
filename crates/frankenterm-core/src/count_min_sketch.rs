//! Count-Min Sketch for approximate frequency estimation.
//!
//! Provides O(1) insertion and O(d) frequency query with guaranteed upper-bound
//! error. Uses d independent hash functions over a w-wide table (d rows × w cols).
//!
//! # Error Guarantees
//!
//! For a sketch with width w and depth d:
//! - Point query error ≤ ε·N with probability ≥ 1 - δ
//! - Where ε = e/w and δ = e^(-d)
//! - Default (w=2048, d=5): ε ≈ 0.13%, δ ≈ 0.67%
//!
//! # Use Cases
//!
//! - Track most frequent error types across 50+ panes
//! - Estimate command frequency distributions
//! - Detect hot panes by output rate
//! - Count event frequencies without storing individual items
//!
//! Bead: ft-283h4.22

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// Configuration for Count-Min Sketch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CmsConfig {
    /// Width of each row. Higher = less error. Default: 2048.
    pub width: usize,
    /// Number of hash functions (rows). Higher = less false positive prob. Default: 5.
    pub depth: usize,
}

impl Default for CmsConfig {
    fn default() -> Self {
        Self {
            width: 2048,
            depth: 5,
        }
    }
}

impl CmsConfig {
    /// Create config from desired error parameters.
    /// ε = e/width, δ = e^(-depth)
    pub fn from_error_params(epsilon: f64, delta: f64) -> Self {
        let width = (std::f64::consts::E / epsilon).ceil() as usize;
        let depth = (1.0 / delta).ln().ceil() as usize;
        Self {
            width: width.max(4),
            depth: depth.max(1),
        }
    }
}

/// Statistics about the Count-Min Sketch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmsStats {
    pub width: usize,
    pub depth: usize,
    pub total_count: u64,
    pub memory_bytes: usize,
    pub epsilon: f64,
    pub delta: f64,
}

/// Count-Min Sketch for approximate frequency estimation.
///
/// # Example
/// ```
/// use frankenterm_core::count_min_sketch::CountMinSketch;
///
/// let mut cms = CountMinSketch::new();
/// for _ in 0..100 {
///     cms.increment("error_404");
/// }
/// let freq = cms.estimate("error_404");
/// assert!(freq >= 100);
/// ```
#[derive(Debug, Clone)]
pub struct CountMinSketch {
    table: Vec<Vec<u64>>,
    seeds: Vec<u64>,
    width: usize,
    depth: usize,
    total_count: u64,
}

impl CountMinSketch {
    /// Create with default parameters (w=2048, d=5).
    pub fn new() -> Self {
        Self::with_config(CmsConfig::default())
    }

    /// Create with specified width and depth.
    pub fn with_dimensions(width: usize, depth: usize) -> Self {
        Self::with_config(CmsConfig {
            width: width.max(4),
            depth: depth.max(1),
        })
    }

    /// Create from config.
    pub fn with_config(config: CmsConfig) -> Self {
        let width = config.width.max(4);
        let depth = config.depth.max(1);
        let table = vec![vec![0u64; width]; depth];

        // Deterministic seeds for reproducibility
        let seeds: Vec<u64> = (0..depth)
            .map(|i| splitmix64(i as u64 + 0x12345678))
            .collect();

        Self {
            table,
            seeds,
            width,
            depth,
            total_count: 0,
        }
    }

    /// Create from desired error parameters.
    pub fn from_error_params(epsilon: f64, delta: f64) -> Self {
        Self::with_config(CmsConfig::from_error_params(epsilon, delta))
    }

    /// Increment count for an item by 1.
    pub fn increment<T: Hash + ?Sized>(&mut self, item: &T) {
        self.add(item, 1);
    }

    /// Add `count` to an item's frequency.
    pub fn add<T: Hash + ?Sized>(&mut self, item: &T, count: u64) {
        for d in 0..self.depth {
            let idx = self.hash_to_index(item, d);
            self.table[d][idx] = self.table[d][idx].saturating_add(count);
        }
        self.total_count = self.total_count.saturating_add(count);
    }

    /// Estimate frequency of an item (guaranteed upper bound).
    pub fn estimate<T: Hash + ?Sized>(&self, item: &T) -> u64 {
        let mut min = u64::MAX;
        for d in 0..self.depth {
            let idx = self.hash_to_index(item, d);
            min = min.min(self.table[d][idx]);
        }
        min
    }

    /// Total count of all increments.
    pub fn total_count(&self) -> u64 {
        self.total_count
    }

    /// Width (columns per row).
    pub fn width(&self) -> usize {
        self.width
    }

    /// Depth (number of hash functions).
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Whether sketch is empty.
    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    /// Memory used by table in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.width * self.depth * std::mem::size_of::<u64>()
    }

    /// Error bound ε = e/width.
    pub fn epsilon(&self) -> f64 {
        std::f64::consts::E / self.width as f64
    }

    /// Failure probability δ = e^(-depth).
    pub fn delta(&self) -> f64 {
        (-(self.depth as f64)).exp()
    }

    /// Get statistics.
    pub fn stats(&self) -> CmsStats {
        CmsStats {
            width: self.width,
            depth: self.depth,
            total_count: self.total_count,
            memory_bytes: self.memory_bytes(),
            epsilon: self.epsilon(),
            delta: self.delta(),
        }
    }

    /// Merge another sketch into this one. Both must have same dimensions.
    pub fn merge(&mut self, other: &CountMinSketch) -> Result<(), &'static str> {
        if self.width != other.width || self.depth != other.depth {
            return Err("dimension mismatch");
        }
        for d in 0..self.depth {
            for w in 0..self.width {
                self.table[d][w] = self.table[d][w].saturating_add(other.table[d][w]);
            }
        }
        self.total_count = self.total_count.saturating_add(other.total_count);
        Ok(())
    }

    /// Inner product of two sketches (useful for join size estimation).
    /// Returns minimum inner product across rows.
    pub fn inner_product(&self, other: &CountMinSketch) -> Option<u64> {
        if self.width != other.width || self.depth != other.depth {
            return None;
        }
        let mut min_product = u64::MAX;
        for d in 0..self.depth {
            let product: u64 = (0..self.width)
                .map(|w| self.table[d][w].saturating_mul(other.table[d][w]))
                .fold(0u64, |acc, x| acc.saturating_add(x));
            min_product = min_product.min(product);
        }
        Some(min_product)
    }

    /// Clear all counters.
    pub fn clear(&mut self) {
        for row in &mut self.table {
            row.fill(0);
        }
        self.total_count = 0;
    }

    // ── Internal ──────────────────────────────────────────────────

    fn hash_to_index<T: Hash + ?Sized>(&self, item: &T, row: usize) -> usize {
        let mut hasher = FnvHasher::new_with_seed(self.seeds[row]);
        item.hash(&mut hasher);
        let hash = hasher.finish();
        (hash as usize) % self.width
    }
}

impl Default for CountMinSketch {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a hasher with seed.
struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    fn new_with_seed(seed: u64) -> Self {
        Self {
            state: 0xcbf29ce484222325 ^ seed,
        }
    }
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.state
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state ^= byte as u64;
            self.state = self.state.wrapping_mul(0x100000001b3);
        }
    }
}

/// SplitMix64 for seed generation.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sketch() {
        let cms = CountMinSketch::new();
        assert!(cms.is_empty());
        assert_eq!(cms.total_count(), 0);
        assert_eq!(cms.estimate(&"anything"), 0);
    }

    #[test]
    fn single_increment() {
        let mut cms = CountMinSketch::new();
        cms.increment(&42u64);
        assert_eq!(cms.total_count(), 1);
        assert!(cms.estimate(&42u64) >= 1);
    }

    #[test]
    fn multiple_increments() {
        let mut cms = CountMinSketch::new();
        for _ in 0..100 {
            cms.increment(&"hello");
        }
        let est = cms.estimate(&"hello");
        assert!(est >= 100, "estimate {} should be >= 100", est);
    }

    #[test]
    fn add_bulk() {
        let mut cms = CountMinSketch::new();
        cms.add(&42u64, 50);
        assert_eq!(cms.total_count(), 50);
        assert!(cms.estimate(&42u64) >= 50);
    }

    #[test]
    fn unseen_item_zero_or_small() {
        let mut cms = CountMinSketch::with_dimensions(1024, 5);
        for i in 0..100u64 {
            cms.increment(&i);
        }
        // Unseen item might have false positives but should be small
        let est = cms.estimate(&999999u64);
        assert!(est <= 10, "unseen item estimate {} should be small", est);
    }

    #[test]
    fn estimate_is_upper_bound() {
        let mut cms = CountMinSketch::new();
        let true_count = 42;
        for _ in 0..true_count {
            cms.increment(&"test");
        }
        let est = cms.estimate(&"test");
        assert!(
            est >= true_count,
            "estimate {} should be >= true {}",
            est,
            true_count
        );
    }

    #[test]
    fn merge_basic() {
        let mut cms1 = CountMinSketch::new();
        let mut cms2 = CountMinSketch::new();
        cms1.add(&"a", 10);
        cms2.add(&"a", 20);
        cms1.merge(&cms2).unwrap();
        assert!(cms1.estimate(&"a") >= 30);
        assert_eq!(cms1.total_count(), 30);
    }

    #[test]
    fn merge_dimension_mismatch() {
        let mut cms1 = CountMinSketch::with_dimensions(100, 3);
        let cms2 = CountMinSketch::with_dimensions(200, 3);
        assert!(cms1.merge(&cms2).is_err());
    }

    #[test]
    fn clear_resets() {
        let mut cms = CountMinSketch::new();
        for i in 0..100u64 {
            cms.increment(&i);
        }
        cms.clear();
        assert!(cms.is_empty());
        assert_eq!(cms.total_count(), 0);
        assert_eq!(cms.estimate(&0u64), 0);
    }

    #[test]
    fn from_error_params() {
        let cms = CountMinSketch::from_error_params(0.01, 0.001);
        assert!(cms.width() >= 100);
        assert!(cms.depth() >= 3);
    }

    #[test]
    fn config_serde() {
        let config = CmsConfig {
            width: 512,
            depth: 4,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: CmsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn stats_serde() {
        let stats = CmsStats {
            width: 2048,
            depth: 5,
            total_count: 1000,
            memory_bytes: 81920,
            epsilon: 0.001,
            delta: 0.01,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: CmsStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn inner_product_self() {
        let mut cms = CountMinSketch::with_dimensions(256, 3);
        cms.add(&"a", 5);
        cms.add(&"b", 10);
        let ip = cms.inner_product(&cms).unwrap();
        // Self inner product = sum of squares of true frequencies + noise
        assert!(
            ip >= 125,
            "inner product {} should be >= 5^2 + 10^2 = 125",
            ip
        );
    }

    #[test]
    fn memory_bytes_correct() {
        let cms = CountMinSketch::with_dimensions(100, 3);
        assert_eq!(cms.memory_bytes(), 100 * 3 * 8);
    }

    #[test]
    fn epsilon_delta_consistent() {
        let cms = CountMinSketch::with_dimensions(1000, 5);
        assert!(cms.epsilon() > 0.0);
        assert!(cms.delta() > 0.0 && cms.delta() < 1.0);
    }

    #[test]
    fn heavy_hitter_detection() {
        let mut cms = CountMinSketch::with_dimensions(512, 5);
        // Insert one heavy hitter and many light items
        cms.add(&"heavy", 1000);
        for i in 0..100u64 {
            cms.increment(&i);
        }
        let heavy = cms.estimate(&"heavy");
        assert!(
            heavy >= 1000,
            "heavy hitter estimate {} should be >= 1000",
            heavy
        );
        // Heavy should dominate
        for i in 0..100u64 {
            let light = cms.estimate(&i);
            assert!(
                heavy > light,
                "heavy {} should exceed light {}",
                heavy,
                light
            );
        }
    }

    #[test]
    fn saturation_add() {
        let mut cms = CountMinSketch::with_dimensions(8, 2);
        cms.add(&"x", u64::MAX - 1);
        cms.add(&"x", 10);
        // Should saturate, not overflow
        assert_eq!(cms.estimate(&"x"), u64::MAX);
    }

    #[test]
    fn saturation_total_count() {
        let mut cms = CountMinSketch::with_dimensions(8, 2);
        cms.add(&"x", u64::MAX - 1);
        cms.add(&"y", 10);
        // total_count should saturate
        assert_eq!(cms.total_count(), u64::MAX);
    }

    #[test]
    fn string_keys() {
        let mut cms = CountMinSketch::new();
        cms.increment(&String::from("hello"));
        cms.increment(&String::from("hello"));
        cms.increment(&String::from("world"));
        assert!(cms.estimate(&String::from("hello")) >= 2);
        assert!(cms.estimate(&String::from("world")) >= 1);
    }

    #[test]
    fn different_types_same_sketch() {
        let mut cms = CountMinSketch::new();
        cms.increment(&42u32);
        cms.increment(&"text");
        cms.increment(&vec![1u8, 2, 3]);
        assert_eq!(cms.total_count(), 3);
    }

    #[test]
    fn minimum_dimensions_enforced() {
        let cms = CountMinSketch::with_dimensions(1, 0);
        assert!(cms.width() >= 4);
        assert!(cms.depth() >= 1);
    }

    #[test]
    fn config_minimum_clamp() {
        let config = CmsConfig { width: 0, depth: 0 };
        let cms = CountMinSketch::with_config(config);
        assert!(cms.width() >= 4);
        assert!(cms.depth() >= 1);
    }

    #[test]
    fn default_matches_new() {
        let cms1 = CountMinSketch::new();
        let cms2 = CountMinSketch::default();
        assert_eq!(cms1.width(), cms2.width());
        assert_eq!(cms1.depth(), cms2.depth());
    }

    #[test]
    fn merge_with_empty() {
        let mut cms1 = CountMinSketch::new();
        let cms2 = CountMinSketch::new();
        cms1.add(&"x", 42);
        cms1.merge(&cms2).unwrap();
        assert_eq!(cms1.total_count(), 42);
        assert!(cms1.estimate(&"x") >= 42);
    }

    #[test]
    fn inner_product_dimension_mismatch() {
        let cms1 = CountMinSketch::with_dimensions(100, 3);
        let cms2 = CountMinSketch::with_dimensions(200, 3);
        assert!(cms1.inner_product(&cms2).is_none());
    }

    #[test]
    fn inner_product_orthogonal() {
        let mut cms1 = CountMinSketch::with_dimensions(4096, 5);
        let mut cms2 = CountMinSketch::with_dimensions(4096, 5);
        cms1.add(&"only_a", 100);
        cms2.add(&"only_b", 100);
        let ip = cms1.inner_product(&cms2).unwrap();
        // Orthogonal items should have small inner product (just noise)
        assert!(
            ip < 10000,
            "orthogonal inner product {} should be small",
            ip
        );
    }

    #[test]
    fn clear_then_reuse() {
        let mut cms = CountMinSketch::new();
        cms.add(&"first", 100);
        cms.clear();
        cms.add(&"second", 200);
        assert_eq!(cms.total_count(), 200);
        assert!(cms.estimate(&"second") >= 200);
        // First item should be gone
        assert_eq!(cms.estimate(&"first"), 0);
    }

    #[test]
    fn error_params_very_tight() {
        let cms = CountMinSketch::from_error_params(0.001, 0.0001);
        assert!(cms.width() >= 1000);
        assert!(cms.depth() >= 5);
    }

    #[test]
    fn error_params_very_loose() {
        let cms = CountMinSketch::from_error_params(1.0, 0.5);
        assert!(cms.width() >= 4); // clamped minimum
        assert!(cms.depth() >= 1);
    }

    #[test]
    fn clone_independence() {
        let mut cms = CountMinSketch::new();
        cms.add(&"x", 10);
        let mut clone = cms.clone();
        clone.add(&"x", 20);
        // Original should be unaffected
        assert!(cms.estimate(&"x") >= 10);
        assert!(cms.estimate(&"x") < 30);
        assert!(clone.estimate(&"x") >= 30);
    }
}
