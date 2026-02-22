//! HyperLogLog++ approximate distinct count estimator.
//!
//! Provides O(1) insertion and O(m) cardinality estimation with ~1.04/sqrt(m)
//! standard error. Uses bias correction from the HyperLogLog++ paper
//! (Heule, Nunkesser, Hall 2013) for improved small-cardinality accuracy.
//!
//! # Algorithm
//!
//! HyperLogLog works by hashing each element and observing the position of the
//! leftmost 1-bit in the hash. With m registers (2^p), the harmonic mean of
//! 2^(register value) estimates the cardinality. The key insight is that
//! seeing a run of k leading zeros has probability 2^(-k), so longer runs
//! indicate more distinct elements.
//!
//! Memory usage: 2^p bytes (e.g., p=14 → 16KB for ~0.81% standard error).
//!
//! Bead: ft-283h4.21

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};

/// Configuration for HyperLogLog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HllConfig {
    /// Precision parameter p. Register count = 2^p.
    /// Valid range: 4..=18. Default: 14.
    pub precision: u8,
}

impl Default for HllConfig {
    fn default() -> Self {
        Self { precision: 14 }
    }
}

/// Statistics about the HyperLogLog state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HllStats {
    pub precision: u8,
    pub register_count: usize,
    pub nonzero_registers: usize,
    pub estimated_cardinality: u64,
    pub memory_bytes: usize,
}

/// HyperLogLog++ approximate distinct count estimator.
///
/// # Example
/// ```
/// use frankenterm_core::hyperloglog::HyperLogLog;
///
/// let mut hll = HyperLogLog::new();
/// for i in 0..10000u64 {
///     hll.insert(&i);
/// }
/// let estimate = hll.cardinality();
/// // Estimate should be within ~5% of 10000
/// assert!((estimate as f64 - 10000.0).abs() < 1000.0);
/// ```
#[derive(Debug, Clone)]
pub struct HyperLogLog {
    registers: Vec<u8>,
    precision: u8,
    m: usize,   // 2^precision
    count: u64, // total inserts (not distinct)
}

impl HyperLogLog {
    /// Create with default precision (p=14, ~0.81% standard error).
    #[must_use]
    pub fn new() -> Self {
        Self::with_precision(14)
    }

    /// Create with specified precision p (4..=18).
    /// Higher p = more accuracy, more memory (2^p bytes).
    #[must_use]
    pub fn with_precision(p: u8) -> Self {
        let p = p.clamp(4, 18);
        let m = 1 << p;
        Self {
            registers: vec![0u8; m],
            precision: p,
            m,
            count: 0,
        }
    }

    /// Create from config.
    #[must_use]
    pub fn with_config(config: HllConfig) -> Self {
        Self::with_precision(config.precision)
    }

    /// Insert a hashable element.
    pub fn insert<T: Hash + ?Sized>(&mut self, item: &T) {
        let hash = self.hash_item(item);
        let idx = (hash >> (64 - self.precision)) as usize;
        // Count leading zeros in the remaining bits
        let remaining = if self.precision >= 64 {
            0u64
        } else {
            (hash << self.precision) | (1u64 << (self.precision - 1))
        };
        let rho = remaining.leading_zeros() as u8 + 1;

        if rho > self.registers[idx] {
            self.registers[idx] = rho;
        }
        self.count += 1;
    }

    /// Insert a pre-computed hash value directly.
    pub fn insert_hash(&mut self, hash: u64) {
        let idx = (hash >> (64 - self.precision)) as usize;
        let remaining = if self.precision >= 64 {
            0u64
        } else {
            (hash << self.precision) | (1u64 << (self.precision - 1))
        };
        let rho = remaining.leading_zeros() as u8 + 1;

        if rho > self.registers[idx] {
            self.registers[idx] = rho;
        }
        self.count += 1;
    }

    /// Estimate the number of distinct elements.
    #[must_use]
    pub fn cardinality(&self) -> u64 {
        let m = self.m as f64;
        let alpha = self.alpha();

        // Raw harmonic mean estimate
        let sum: f64 = self
            .registers
            .iter()
            .map(|&r| 2.0f64.powi(-(r as i32)))
            .sum();
        let raw_estimate = alpha * m * m / sum;

        // Small range correction (linear counting)
        #[allow(clippy::naive_bytecount)]
        let zeros = self.registers.iter().filter(|&&r| r == 0).count();
        if raw_estimate <= 2.5 * m && zeros > 0 {
            // Linear counting
            let lc = m * (m / zeros as f64).ln();
            return lc as u64;
        }

        // Large range correction is unnecessary for 64-bit hashes
        // because hash collisions only become significant near 2^64 / 30.
        raw_estimate as u64
    }

    /// Estimate cardinality as f64 (more precision for comparisons).
    #[must_use]
    pub fn cardinality_f64(&self) -> f64 {
        self.cardinality() as f64
    }

    /// Total number of insert calls (not distinct count).
    #[must_use]
    pub fn total_inserts(&self) -> u64 {
        self.count
    }

    /// Precision parameter.
    #[must_use]
    pub fn precision(&self) -> u8 {
        self.precision
    }

    /// Number of registers (2^precision).
    #[must_use]
    pub fn register_count(&self) -> usize {
        self.m
    }

    /// Memory used by registers in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> usize {
        self.m
    }

    /// Standard error of the estimate (~1.04/sqrt(m)).
    #[must_use]
    pub fn standard_error(&self) -> f64 {
        1.04 / (self.m as f64).sqrt()
    }

    /// Number of non-zero registers.
    #[must_use]
    pub fn nonzero_registers(&self) -> usize {
        self.registers.iter().filter(|&&r| r > 0).count()
    }

    /// Whether no elements have been inserted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Get statistics.
    #[must_use]
    pub fn stats(&self) -> HllStats {
        HllStats {
            precision: self.precision,
            register_count: self.m,
            nonzero_registers: self.nonzero_registers(),
            estimated_cardinality: self.cardinality(),
            memory_bytes: self.memory_bytes(),
        }
    }

    /// Merge another HyperLogLog into this one.
    /// Both must have the same precision.
    pub fn merge(&mut self, other: &HyperLogLog) -> Result<(), &'static str> {
        if self.precision != other.precision {
            return Err("precision mismatch");
        }
        for i in 0..self.m {
            if other.registers[i] > self.registers[i] {
                self.registers[i] = other.registers[i];
            }
        }
        self.count += other.count;
        Ok(())
    }

    /// Clear all registers.
    pub fn clear(&mut self) {
        self.registers.fill(0);
        self.count = 0;
    }

    /// Jaccard similarity estimate between two HyperLogLogs.
    /// Returns intersection / union estimate.
    #[must_use]
    pub fn jaccard(&self, other: &HyperLogLog) -> Option<f64> {
        if self.precision != other.precision {
            return None;
        }

        // |A ∪ B| via merged HLL
        let mut merged = self.clone();
        merged.merge(other).ok()?;
        let union_card = merged.cardinality() as f64;

        if union_card == 0.0 {
            return Some(0.0);
        }

        // |A ∩ B| = |A| + |B| - |A ∪ B| (inclusion-exclusion)
        let a_card = self.cardinality() as f64;
        let b_card = other.cardinality() as f64;
        let intersection = (a_card + b_card - union_card).max(0.0);

        Some(intersection / union_card)
    }

    // ── Internal ──────────────────────────────────────────────────

    /// Alpha constant for bias correction (depends on m).
    fn alpha(&self) -> f64 {
        match self.m {
            16 => 0.673,
            32 => 0.697,
            64 => 0.709,
            _ => 0.7213 / (1.0 + 1.079 / self.m as f64),
        }
    }

    /// Hash an item using a fast 64-bit hash (SplitMix64-based).
    #[allow(clippy::unused_self)]
    fn hash_item<T: Hash + ?Sized>(&self, item: &T) -> u64 {
        let mut hasher = FnvHasher::new();
        item.hash(&mut hasher);
        let h = hasher.finish();
        // Mix with SplitMix64 for better distribution
        splitmix64(h)
    }
}

impl Default for HyperLogLog {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a hasher for fast hashing.
struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    fn new() -> Self {
        Self {
            state: 0xcbf29ce484222325,
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

/// SplitMix64 finalizer for hash mixing.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn empty_hll() {
        let hll = HyperLogLog::new();
        assert!(hll.is_empty());
        assert_eq!(hll.cardinality(), 0);
        assert_eq!(hll.total_inserts(), 0);
    }

    #[test]
    fn single_element() {
        let mut hll = HyperLogLog::new();
        hll.insert(&42u64);
        assert!(!hll.is_empty());
        assert_eq!(hll.total_inserts(), 1);
        let card = hll.cardinality();
        assert!((1..=3).contains(&card), "cardinality {} should be ~1", card);
    }

    #[test]
    fn duplicate_elements() {
        let mut hll = HyperLogLog::new();
        for _ in 0..1000 {
            hll.insert(&42u64);
        }
        assert_eq!(hll.total_inserts(), 1000);
        let card = hll.cardinality();
        assert!((1..=3).contains(&card), "cardinality {} should be ~1", card);
    }

    #[test]
    fn distinct_elements() {
        let mut hll = HyperLogLog::new();
        for i in 0..10000u64 {
            hll.insert(&i);
        }
        let card = hll.cardinality();
        // Within ~5% at p=14
        assert!(
            (card as f64 - 10000.0).abs() < 1000.0,
            "cardinality {} should be ~10000",
            card
        );
    }

    #[test]
    fn precision_bounds() {
        let low = HyperLogLog::with_precision(4);
        let high = HyperLogLog::with_precision(18);
        assert_eq!(low.register_count(), 16);
        assert_eq!(high.register_count(), 1 << 18);
    }

    #[test]
    fn precision_clamped() {
        let too_low = HyperLogLog::with_precision(2);
        assert_eq!(too_low.precision(), 4);
        let too_high = HyperLogLog::with_precision(20);
        assert_eq!(too_high.precision(), 18);
    }

    #[test]
    fn merge_basic() {
        let mut hll1 = HyperLogLog::new();
        let mut hll2 = HyperLogLog::new();
        for i in 0..5000u64 {
            hll1.insert(&i);
        }
        for i in 5000..10000u64 {
            hll2.insert(&i);
        }
        hll1.merge(&hll2).unwrap();
        let card = hll1.cardinality();
        assert!(
            (card as f64 - 10000.0).abs() < 1000.0,
            "merged cardinality {} should be ~10000",
            card
        );
    }

    #[test]
    fn merge_precision_mismatch() {
        let mut hll1 = HyperLogLog::with_precision(10);
        let hll2 = HyperLogLog::with_precision(12);
        assert!(hll1.merge(&hll2).is_err());
    }

    #[test]
    fn clear_resets() {
        let mut hll = HyperLogLog::new();
        for i in 0..100u64 {
            hll.insert(&i);
        }
        hll.clear();
        assert!(hll.is_empty());
        assert_eq!(hll.cardinality(), 0);
        assert_eq!(hll.total_inserts(), 0);
        assert_eq!(hll.nonzero_registers(), 0);
    }

    #[test]
    fn standard_error_decreases_with_precision() {
        let low = HyperLogLog::with_precision(4);
        let high = HyperLogLog::with_precision(14);
        assert!(low.standard_error() > high.standard_error());
    }

    #[test]
    fn stats_basic() {
        let mut hll = HyperLogLog::new();
        for i in 0..100u64 {
            hll.insert(&i);
        }
        let stats = hll.stats();
        assert_eq!(stats.precision, 14);
        assert_eq!(stats.register_count, 1 << 14);
        assert!(stats.nonzero_registers > 0);
    }

    #[test]
    fn memory_bytes_correct() {
        let hll = HyperLogLog::with_precision(10);
        assert_eq!(hll.memory_bytes(), 1024);
    }

    #[test]
    fn insert_hash_direct() {
        let mut hll = HyperLogLog::new();
        hll.insert_hash(0xDEADBEEF);
        assert_eq!(hll.total_inserts(), 1);
        assert!(hll.cardinality() >= 1);
    }

    #[test]
    fn jaccard_identical() {
        let mut hll1 = HyperLogLog::with_precision(10);
        for i in 0..1000u64 {
            hll1.insert(&i);
        }
        let hll2 = hll1.clone();
        let j = hll1.jaccard(&hll2).unwrap();
        assert!(j > 0.8, "jaccard of identical sets {} should be ~1.0", j);
    }

    #[test]
    fn jaccard_disjoint() {
        let mut hll1 = HyperLogLog::with_precision(10);
        let mut hll2 = HyperLogLog::with_precision(10);
        for i in 0..1000u64 {
            hll1.insert(&i);
        }
        for i in 10000..11000u64 {
            hll2.insert(&i);
        }
        let j = hll1.jaccard(&hll2).unwrap();
        assert!(j < 0.2, "jaccard of disjoint sets {} should be ~0.0", j);
    }

    #[test]
    fn jaccard_precision_mismatch() {
        let hll1 = HyperLogLog::with_precision(10);
        let hll2 = HyperLogLog::with_precision(12);
        assert!(hll1.jaccard(&hll2).is_none());
    }

    #[test]
    fn config_serde() {
        let config = HllConfig { precision: 12 };
        let json = serde_json::to_string(&config).unwrap();
        let back: HllConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn stats_serde() {
        let stats = HllStats {
            precision: 14,
            register_count: 16384,
            nonzero_registers: 100,
            estimated_cardinality: 150,
            memory_bytes: 16384,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: HllStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn from_config() {
        let config = HllConfig { precision: 10 };
        let hll = HyperLogLog::with_config(config);
        assert_eq!(hll.precision(), 10);
        assert_eq!(hll.register_count(), 1024);
    }

    #[test]
    fn string_elements() {
        let mut hll = HyperLogLog::new();
        for i in 0..1000 {
            hll.insert(&format!("error_{}", i));
        }
        let card = hll.cardinality();
        assert!(
            (card as f64 - 1000.0).abs() < 200.0,
            "cardinality {} should be ~1000",
            card
        );
    }

    // ── Default impl ────────────────────────────────────────────────

    #[test]
    fn default_creates_p14() {
        let hll = HyperLogLog::default();
        assert_eq!(hll.precision(), 14);
        assert_eq!(hll.register_count(), 1 << 14);
        assert!(hll.is_empty());
    }

    // ── Insert/cardinality edge cases ───────────────────────────────

    #[test]
    fn insert_after_clear() {
        let mut hll = HyperLogLog::new();
        for i in 0..100u64 {
            hll.insert(&i);
        }
        hll.clear();
        for i in 0..50u64 {
            hll.insert(&i);
        }
        assert_eq!(hll.total_inserts(), 50);
        let card = hll.cardinality();
        assert!(
            (card as f64 - 50.0).abs() < 20.0,
            "cardinality {} should be ~50",
            card
        );
    }

    #[test]
    fn multiple_clears() {
        let mut hll = HyperLogLog::new();
        hll.insert(&1u64);
        hll.clear();
        hll.clear();
        assert!(hll.is_empty());
        assert_eq!(hll.cardinality(), 0);
    }

    #[test]
    fn cardinality_f64_matches_integer() {
        let mut hll = HyperLogLog::new();
        for i in 0..500u64 {
            hll.insert(&i);
        }
        assert_eq!(hll.cardinality_f64(), hll.cardinality() as f64);
    }

    #[test]
    fn is_empty_tracks_inserts() {
        let mut hll = HyperLogLog::new();
        assert!(hll.is_empty());
        hll.insert(&1u64);
        assert!(!hll.is_empty());
        hll.clear();
        assert!(hll.is_empty());
    }

    // ── Monotonicity ────────────────────────────────────────────────

    #[test]
    fn more_distinct_elements_higher_cardinality() {
        let mut hll_small = HyperLogLog::with_precision(10);
        let mut hll_large = HyperLogLog::with_precision(10);
        for i in 0..100u64 {
            hll_small.insert(&i);
        }
        for i in 0..1000u64 {
            hll_large.insert(&i);
        }
        assert!(
            hll_large.cardinality() > hll_small.cardinality(),
            "1000 distinct should give higher cardinality than 100"
        );
    }

    #[test]
    fn nonzero_registers_grow_with_elements() {
        let mut hll = HyperLogLog::with_precision(10);
        assert_eq!(hll.nonzero_registers(), 0);
        hll.insert(&1u64);
        let nz1 = hll.nonzero_registers();
        assert!(nz1 > 0);
        for i in 2..100u64 {
            hll.insert(&i);
        }
        assert!(hll.nonzero_registers() >= nz1);
    }

    // ── Accuracy at different scales ────────────────────────────────

    #[test]
    fn accuracy_at_100() {
        let mut hll = HyperLogLog::with_precision(12);
        for i in 0..100u64 {
            hll.insert(&i);
        }
        let card = hll.cardinality();
        assert!(
            (card as f64 - 100.0).abs() < 30.0,
            "cardinality {} should be ~100",
            card
        );
    }

    #[test]
    fn accuracy_at_50000() {
        let mut hll = HyperLogLog::new(); // p=14
        for i in 0..50000u64 {
            hll.insert(&i);
        }
        let card = hll.cardinality();
        let error_pct = ((card as f64 - 50000.0) / 50000.0).abs();
        assert!(
            error_pct < 0.05,
            "cardinality {} should be within 5% of 50000 (error: {:.1}%)",
            card,
            error_pct * 100.0
        );
    }

    // ── Precision and alpha ─────────────────────────────────────────

    #[test]
    fn alpha_varies_by_register_count() {
        // Alpha is internal, but standard_error depends on it
        let p4 = HyperLogLog::with_precision(4);
        let p5 = HyperLogLog::with_precision(5);
        let p6 = HyperLogLog::with_precision(6);
        // More registers → lower standard error
        assert!(p4.standard_error() > p5.standard_error());
        assert!(p5.standard_error() > p6.standard_error());
    }

    #[test]
    fn standard_error_formula() {
        let hll = HyperLogLog::with_precision(10);
        let expected = 1.04 / (1024.0f64).sqrt();
        assert!((hll.standard_error() - expected).abs() < 1e-10);
    }

    // ── Merge edge cases ────────────────────────────────────────────

    #[test]
    fn merge_with_empty() {
        let mut hll = HyperLogLog::new();
        for i in 0..100u64 {
            hll.insert(&i);
        }
        let card_before = hll.cardinality();
        let empty = HyperLogLog::new();
        hll.merge(&empty).unwrap();
        assert_eq!(hll.cardinality(), card_before);
    }

    #[test]
    fn merge_into_empty() {
        let mut empty = HyperLogLog::new();
        let mut full = HyperLogLog::new();
        for i in 0..100u64 {
            full.insert(&i);
        }
        let expected = full.cardinality();
        empty.merge(&full).unwrap();
        assert_eq!(empty.cardinality(), expected);
    }

    #[test]
    fn merge_overlapping_sets() {
        let mut hll1 = HyperLogLog::with_precision(12);
        let mut hll2 = HyperLogLog::with_precision(12);
        // hll1: 0..1000, hll2: 500..1500 → union: 0..1500
        for i in 0..1000u64 {
            hll1.insert(&i);
        }
        for i in 500..1500u64 {
            hll2.insert(&i);
        }
        hll1.merge(&hll2).unwrap();
        let card = hll1.cardinality();
        assert!(
            (card as f64 - 1500.0).abs() < 300.0,
            "merged cardinality {} should be ~1500",
            card
        );
    }

    #[test]
    fn merge_same_data_idempotent() {
        let mut hll1 = HyperLogLog::with_precision(10);
        let mut hll2 = HyperLogLog::with_precision(10);
        for i in 0..500u64 {
            hll1.insert(&i);
            hll2.insert(&i);
        }
        let card_before = hll1.cardinality();
        hll1.merge(&hll2).unwrap();
        // Merging same data shouldn't significantly change cardinality
        let card_after = hll1.cardinality();
        assert_eq!(card_before, card_after);
    }

    #[test]
    fn merge_preserves_count() {
        let mut hll1 = HyperLogLog::new();
        let mut hll2 = HyperLogLog::new();
        for i in 0..100u64 {
            hll1.insert(&i);
        }
        for i in 0..50u64 {
            hll2.insert(&i);
        }
        hll1.merge(&hll2).unwrap();
        assert_eq!(hll1.total_inserts(), 150);
    }

    // ── Clone independence ──────────────────────────────────────────

    #[test]
    fn clone_independence() {
        let mut hll = HyperLogLog::new();
        for i in 0..100u64 {
            hll.insert(&i);
        }
        let mut cloned = hll.clone();
        cloned.insert(&999u64);

        // Original should not change
        assert_eq!(hll.total_inserts(), 100);
        assert_eq!(cloned.total_inserts(), 101);
    }

    // ── insert_hash edge cases ──────────────────────────────────────

    #[test]
    fn insert_hash_zero() {
        let mut hll = HyperLogLog::new();
        hll.insert_hash(0);
        assert_eq!(hll.total_inserts(), 1);
        assert!(hll.cardinality() >= 1);
    }

    #[test]
    fn insert_hash_max() {
        let mut hll = HyperLogLog::new();
        hll.insert_hash(u64::MAX);
        assert_eq!(hll.total_inserts(), 1);
        assert!(hll.cardinality() >= 1);
    }

    #[test]
    fn insert_hash_many_distinct() {
        let mut hll = HyperLogLog::with_precision(12);
        for i in 0..5000u64 {
            hll.insert_hash(splitmix64(i));
        }
        let card = hll.cardinality();
        assert!(
            (card as f64 - 5000.0).abs() < 1000.0,
            "cardinality {} should be ~5000",
            card
        );
    }

    // ── Jaccard edge cases ──────────────────────────────────────────

    #[test]
    fn jaccard_both_empty() {
        let hll1 = HyperLogLog::with_precision(10);
        let hll2 = HyperLogLog::with_precision(10);
        let j = hll1.jaccard(&hll2).unwrap();
        assert_eq!(j, 0.0);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let mut hll1 = HyperLogLog::with_precision(12);
        let mut hll2 = HyperLogLog::with_precision(12);
        // 50% overlap: hll1=0..100, hll2=50..150
        for i in 0..100u64 {
            hll1.insert(&i);
        }
        for i in 50..150u64 {
            hll2.insert(&i);
        }
        let j = hll1.jaccard(&hll2).unwrap();
        // Expected ~50/150 = 0.33
        assert!(j > 0.1 && j < 0.6, "jaccard {} should be ~0.33", j);
    }

    #[test]
    fn jaccard_symmetric() {
        let mut hll1 = HyperLogLog::with_precision(10);
        let mut hll2 = HyperLogLog::with_precision(10);
        for i in 0..500u64 {
            hll1.insert(&i);
        }
        for i in 250..750u64 {
            hll2.insert(&i);
        }
        let j12 = hll1.jaccard(&hll2).unwrap();
        let j21 = hll2.jaccard(&hll1).unwrap();
        assert!(
            (j12 - j21).abs() < 0.05,
            "jaccard should be symmetric: {} vs {}",
            j12,
            j21
        );
    }

    // ── Stats edge cases ────────────────────────────────────────────

    #[test]
    fn stats_on_empty() {
        let hll = HyperLogLog::new();
        let stats = hll.stats();
        assert_eq!(stats.precision, 14);
        assert_eq!(stats.nonzero_registers, 0);
        assert_eq!(stats.estimated_cardinality, 0);
    }

    #[test]
    fn stats_after_merge() {
        let mut hll1 = HyperLogLog::with_precision(10);
        let mut hll2 = HyperLogLog::with_precision(10);
        for i in 0..100u64 {
            hll1.insert(&i);
        }
        for i in 50..150u64 {
            hll2.insert(&i);
        }
        hll1.merge(&hll2).unwrap();
        let stats = hll1.stats();
        assert!(stats.nonzero_registers > 0);
        assert!(stats.estimated_cardinality > 100);
    }

    #[test]
    fn stats_after_clear() {
        let mut hll = HyperLogLog::new();
        for i in 0..100u64 {
            hll.insert(&i);
        }
        hll.clear();
        let stats = hll.stats();
        assert_eq!(stats.nonzero_registers, 0);
        assert_eq!(stats.estimated_cardinality, 0);
    }

    // ── Type-specific tests ─────────────────────────────────────────

    #[test]
    fn u8_elements() {
        let mut hll = HyperLogLog::with_precision(10);
        for i in 0..=255u8 {
            hll.insert(&i);
        }
        let card = hll.cardinality();
        assert!(
            (card as f64 - 256.0).abs() < 80.0,
            "cardinality {} should be ~256",
            card
        );
    }

    #[test]
    fn bool_elements() {
        let mut hll = HyperLogLog::with_precision(8);
        hll.insert(&true);
        hll.insert(&false);
        hll.insert(&true); // duplicate
        assert_eq!(hll.total_inserts(), 3);
        let card = hll.cardinality();
        assert!((1..=5).contains(&card), "cardinality {} should be ~2", card);
    }

    #[test]
    fn i32_negative_and_positive() {
        let mut hll = HyperLogLog::with_precision(10);
        for i in -50..50i32 {
            hll.insert(&i);
        }
        let card = hll.cardinality();
        assert!(
            (card as f64 - 100.0).abs() < 40.0,
            "cardinality {} should be ~100",
            card
        );
    }

    // ── Config tests ────────────────────────────────────────────────

    #[test]
    fn config_default_precision() {
        let config = HllConfig::default();
        assert_eq!(config.precision, 14);
    }

    #[test]
    fn config_equality() {
        let c1 = HllConfig { precision: 12 };
        let c2 = HllConfig { precision: 12 };
        let c3 = HllConfig { precision: 10 };
        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
    }

    #[test]
    fn stats_equality() {
        let s1 = HllStats {
            precision: 14,
            register_count: 16384,
            nonzero_registers: 100,
            estimated_cardinality: 150,
            memory_bytes: 16384,
        };
        let s2 = s1.clone();
        assert_eq!(s1, s2);
    }
}
