//! Compression-aware memory accounting via Shannon entropy.
//!
//! Traditional memory accounting counts raw bytes. But not all bytes are equal:
//! a 100 MB pane of repeated `AAAA...` has near-zero entropy and compresses to
//! almost nothing, while a 1 MB pane of random binary has ~8 bits/byte entropy
//! and is incompressible. This module provides information-theoretic primitives
//! for smarter memory management:
//!
//! - **Sliding-window entropy estimation**: O(1) per byte update.
//! - **Information cost**: `raw_bytes × (entropy / 8.0)` — the "true" memory cost.
//! - **Eviction scoring**: `information_cost × recency_decay` — evict low-value panes first.
//!
//! # Shannon entropy
//!
//! For a byte stream with symbol frequencies `p(x)`:
//!
//! > H(X) = -Σ p(x) log₂ p(x), range \[0, 8\] bits/byte
//!
//! - H = 0 means the stream is constant (perfectly compressible).
//! - H = 8 means every byte value is equally likely (incompressible).
//!
//! # Information cost
//!
//! `I = N × H / 8` where N is raw byte count. This yields an estimate of
//! the irreducible information content:
//! - Constant data: I ≈ 0 (cheap to lose, trivially reconstructable).
//! - Random data: I ≈ N (expensive to lose, irreplaceable).
//!
//! # Compression ratio lower bound
//!
//! By the source coding theorem, `CR ≥ 8 / H`. A stream with H = 2 bits/byte
//! can be compressed to at most 25% of its raw size.

use serde::{Deserialize, Serialize};

// =============================================================================
// Sliding-window entropy estimator
// =============================================================================

/// Incrementally estimates Shannon entropy over a sliding window of bytes.
///
/// Maintains a byte frequency histogram that updates in O(1) per byte.
/// When more than `window_size` bytes have been fed, the estimator
/// produces a steady-state entropy estimate based on the last
/// `window_size` bytes.
///
/// The window is approximate: we track total byte count and scale
/// the histogram rather than maintaining a full ring buffer.
/// This trades a small amount of accuracy for O(256) memory.
pub struct EntropyEstimator {
    /// Byte frequency counts.
    counts: [u64; 256],
    /// Total bytes in the histogram.
    total: u64,
    /// Window size (max bytes tracked before decay).
    window_size: u64,
    /// Cached entropy value (invalidated on update).
    cached_entropy: Option<f64>,
}

impl EntropyEstimator {
    /// Create a new estimator with the given window size.
    ///
    /// `window_size` controls the sliding window. Common values:
    /// - 65536 (64 KB) for per-pane accounting.
    /// - 1048576 (1 MB) for aggregate accounting.
    #[must_use]
    pub fn new(window_size: usize) -> Self {
        Self {
            counts: [0u64; 256],
            total: 0,
            window_size: window_size as u64,
            cached_entropy: None,
        }
    }

    /// Feed a single byte into the estimator.
    pub fn update(&mut self, byte: u8) {
        self.counts[byte as usize] += 1;
        self.total += 1;
        self.cached_entropy = None;

        // When we exceed 2× the window, halve all counts to approximate
        // a sliding window without storing a ring buffer.
        if self.total > self.window_size * 2 {
            self.decay();
        }
    }

    /// Feed a block of bytes into the estimator.
    pub fn update_block(&mut self, data: &[u8]) {
        for &b in data {
            self.counts[b as usize] += 1;
        }
        self.total += data.len() as u64;
        self.cached_entropy = None;

        if self.total > self.window_size * 2 {
            self.decay();
        }
    }

    /// Current Shannon entropy estimate in bits per byte (0.0 to 8.0).
    #[must_use]
    pub fn entropy(&mut self) -> f64 {
        if let Some(cached) = self.cached_entropy {
            return cached;
        }
        let h = compute_entropy_from_counts(&self.counts, self.total);
        self.cached_entropy = Some(h);
        h
    }

    /// Total bytes fed into the estimator (before decay).
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// Current window fill level (0.0 to 1.0+).
    #[must_use]
    pub fn fill_ratio(&self) -> f64 {
        if self.window_size == 0 {
            return 0.0;
        }
        self.total as f64 / self.window_size as f64
    }

    /// Estimated compression ratio lower bound (8 / H).
    /// Returns `f64::INFINITY` for constant data (H = 0).
    #[must_use]
    pub fn compression_ratio_bound(&mut self) -> f64 {
        let h = self.entropy();
        if h < 1e-10 { f64::INFINITY } else { 8.0 / h }
    }

    /// Reset the estimator to empty state.
    pub fn reset(&mut self) {
        self.counts = [0u64; 256];
        self.total = 0;
        self.cached_entropy = None;
    }

    /// Halve all frequency counts to simulate a sliding window.
    fn decay(&mut self) {
        let mut new_total = 0u64;
        for count in &mut self.counts {
            *count /= 2;
            new_total += *count;
        }
        self.total = new_total;
        self.cached_entropy = None;
    }
}

// =============================================================================
// Batch entropy computation
// =============================================================================

/// Compute Shannon entropy of a byte slice (bits per byte, 0.0 to 8.0).
///
/// Returns 0.0 for empty slices.
#[must_use]
pub fn compute_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in data {
        counts[b as usize] += 1;
    }
    compute_entropy_from_counts(&counts, data.len() as u64)
}

/// Compute Shannon entropy from pre-computed byte frequency counts.
fn compute_entropy_from_counts(counts: &[u64; 256], total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    let total_f = total as f64;
    let mut h = 0.0f64;
    for &count in counts {
        if count > 0 {
            let p = count as f64 / total_f;
            h -= p * p.log2();
        }
    }
    // Clamp to valid range [0, 8].
    h.clamp(0.0, 8.0)
}

// =============================================================================
// Information cost
// =============================================================================

/// Compute the information cost of a byte stream.
///
/// `information_cost = raw_bytes × (entropy / 8.0)`
///
/// - Constant data: cost ≈ 0
/// - Uniform random data: cost ≈ raw_bytes
#[must_use]
pub fn information_cost(raw_bytes: usize, entropy_bits_per_byte: f64) -> f64 {
    raw_bytes as f64 * (entropy_bits_per_byte / 8.0)
}

// =============================================================================
// Eviction scoring
// =============================================================================

/// Configuration for entropy-aware eviction scoring.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvictionConfig {
    /// Half-life for recency decay (ms). Older data decays in value.
    pub recency_half_life_ms: u64,
    /// Minimum information cost below which a pane is always evictable.
    pub min_cost_threshold: f64,
}

impl Default for EvictionConfig {
    fn default() -> Self {
        Self {
            recency_half_life_ms: 300_000, // 5 minutes
            min_cost_threshold: 1024.0,    // 1 KB of "real" information
        }
    }
}

/// Snapshot of a pane's entropy-aware memory state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneEntropySummary {
    /// Pane ID.
    pub pane_id: u64,
    /// Raw byte count.
    pub raw_bytes: u64,
    /// Estimated Shannon entropy (bits/byte).
    pub entropy: f64,
    /// Information cost (bytes of irreducible content).
    pub information_cost: f64,
    /// Estimated compression ratio lower bound.
    pub compression_ratio_bound: f64,
    /// Eviction score (lower = better eviction candidate).
    pub eviction_score: f64,
}

/// Compute the eviction score for a pane.
///
/// `score = information_cost × recency_weight`
///
/// Low score → good eviction candidate (low information or stale).
#[must_use]
pub fn eviction_score(info_cost: f64, age_ms: u64, config: &EvictionConfig) -> f64 {
    if config.recency_half_life_ms == 0 {
        return info_cost;
    }
    let decay = 0.5_f64.powf(age_ms as f64 / config.recency_half_life_ms as f64);
    info_cost * decay
}

/// Rank panes by eviction priority (first element = best eviction candidate).
///
/// Returns pane indices sorted by ascending eviction score.
#[must_use]
pub fn eviction_order(scores: &[(u64, f64)]) -> Vec<u64> {
    let mut ranked: Vec<(u64, f64)> = scores.to_vec();
    ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked.into_iter().map(|(id, _)| id).collect()
}

// =============================================================================
// Budget tracking
// =============================================================================

/// Tracks total information budget across all panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformationBudget {
    /// Maximum information cost budget (bytes).
    pub budget_bytes: f64,
    /// Current total information cost across all tracked panes.
    pub current_cost: f64,
    /// Number of tracked panes.
    pub pane_count: usize,
}

impl InformationBudget {
    /// Create a new budget tracker.
    #[must_use]
    pub fn new(budget_bytes: f64) -> Self {
        Self {
            budget_bytes,
            current_cost: 0.0,
            pane_count: 0,
        }
    }

    /// Add a pane's information cost to the budget.
    pub fn add(&mut self, cost: f64) {
        self.current_cost += cost;
        self.pane_count += 1;
    }

    /// Remove a pane's information cost from the budget.
    pub fn remove(&mut self, cost: f64) {
        self.current_cost = (self.current_cost - cost).max(0.0);
        self.pane_count = self.pane_count.saturating_sub(1);
    }

    /// Whether the budget is exceeded.
    #[must_use]
    pub fn is_exceeded(&self) -> bool {
        self.current_cost > self.budget_bytes
    }

    /// How much over budget we are (0 if within budget).
    #[must_use]
    pub fn overage(&self) -> f64 {
        (self.current_cost - self.budget_bytes).max(0.0)
    }

    /// Utilization as a fraction (0.0 to 1.0+).
    #[must_use]
    pub fn utilization(&self) -> f64 {
        if self.budget_bytes <= 0.0 {
            return if self.current_cost > 0.0 {
                f64::INFINITY
            } else {
                0.0
            };
        }
        self.current_cost / self.budget_bytes
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- compute_entropy (batch) ----------------------------------------------

    #[test]
    fn entropy_empty_is_zero() {
        assert_eq!(compute_entropy(&[]), 0.0);
    }

    #[test]
    fn entropy_constant_is_zero() {
        let data = vec![42u8; 10_000];
        let h = compute_entropy(&data);
        assert!(h < 0.001, "constant data should have ~0 entropy, got {h}");
    }

    #[test]
    fn entropy_two_symbols_is_one() {
        // Equal mix of 0 and 1 → H = 1.0 bit/byte (only 2 symbols used out of 256)
        let mut data = vec![0u8; 5000];
        data.extend(vec![1u8; 5000]);
        let h = compute_entropy(&data);
        assert!(
            (h - 1.0).abs() < 0.01,
            "two equal-frequency symbols should have H ≈ 1.0, got {h}"
        );
    }

    #[test]
    fn entropy_uniform_random_near_eight() {
        // Simulate uniform distribution over all 256 byte values.
        let mut data = Vec::with_capacity(256 * 100);
        for _ in 0..100 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        let h = compute_entropy(&data);
        assert!(
            (h - 8.0).abs() < 0.01,
            "uniform data should have H ≈ 8.0, got {h}"
        );
    }

    #[test]
    fn entropy_english_text_midrange() {
        // Typical English text has H ≈ 4-5 bits/byte.
        let text = b"The quick brown fox jumps over the lazy dog. \
            This is a longer sentence to provide more statistical data for \
            the entropy estimator. Shannon entropy measures the average \
            information content per symbol in a message source. English \
            text typically has about 4 to 5 bits of entropy per byte due \
            to the non-uniform frequency distribution of letters.";
        let h = compute_entropy(text);
        assert!(
            h > 3.0 && h < 6.0,
            "English text should have H in [3, 6], got {h}"
        );
    }

    // -- information_cost -----------------------------------------------------

    #[test]
    fn info_cost_constant_data() {
        let data = vec![0u8; 100_000];
        let h = compute_entropy(&data);
        let cost = information_cost(data.len(), h);
        assert!(
            cost < 100.0,
            "constant data cost should be near 0, got {cost}"
        );
    }

    #[test]
    fn info_cost_random_data_equals_raw_size() {
        let mut data = Vec::with_capacity(256 * 100);
        for _ in 0..100 {
            for b in 0..=255u8 {
                data.push(b);
            }
        }
        let h = compute_entropy(&data);
        let cost = information_cost(data.len(), h);
        let raw = data.len() as f64;
        assert!(
            (cost - raw).abs() < raw * 0.01,
            "random data cost should ≈ raw size, got cost={cost}, raw={raw}"
        );
    }

    #[test]
    fn info_cost_bounded_by_raw_size() {
        // For any data, 0 ≤ information_cost ≤ raw_bytes.
        for h in [0.0, 1.0, 4.0, 7.99, 8.0] {
            let cost = information_cost(1000, h);
            assert!(cost >= 0.0);
            assert!(cost <= 1000.0, "cost {cost} exceeded raw size for h={h}");
        }
    }

    // -- EntropyEstimator (incremental) ---------------------------------------

    #[test]
    fn estimator_empty_is_zero() {
        let mut est = EntropyEstimator::new(1024);
        assert_eq!(est.entropy(), 0.0);
        assert_eq!(est.total_bytes(), 0);
    }

    #[test]
    fn estimator_constant_bytes() {
        let mut est = EntropyEstimator::new(1024);
        for _ in 0..1000 {
            est.update(42);
        }
        assert!(est.entropy() < 0.001, "constant bytes: H should be ~0");
    }

    #[test]
    fn estimator_matches_batch() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        let batch_h = compute_entropy(&data);

        let mut est = EntropyEstimator::new(data.len());
        for &b in &data {
            est.update(b);
        }
        let inc_h = est.entropy();

        assert!(
            (batch_h - inc_h).abs() < 0.01,
            "incremental ({inc_h}) should match batch ({batch_h})"
        );
    }

    #[test]
    fn estimator_block_update() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        let batch_h = compute_entropy(&data);

        let mut est = EntropyEstimator::new(data.len());
        est.update_block(&data);
        let block_h = est.entropy();

        assert!(
            (batch_h - block_h).abs() < 0.01,
            "block update ({block_h}) should match batch ({batch_h})"
        );
    }

    #[test]
    fn estimator_decay_approximates_window() {
        let mut est = EntropyEstimator::new(1000);
        // Feed 1000 bytes of value 0 (H = 0).
        for _ in 0..1000 {
            est.update(0);
        }
        assert!(est.entropy() < 0.001);

        // Feed 3000 bytes of uniform data (after decay, old counts halved).
        for i in 0..3000u16 {
            est.update((i % 256) as u8);
        }
        // Entropy should now be high (not stuck at 0).
        let h = est.entropy();
        assert!(
            h > 5.0,
            "after feeding uniform data, H should be high, got {h}"
        );
    }

    #[test]
    fn estimator_reset() {
        let mut est = EntropyEstimator::new(1024);
        for _ in 0..500 {
            est.update(99);
        }
        est.reset();
        assert_eq!(est.total_bytes(), 0);
        assert_eq!(est.entropy(), 0.0);
    }

    #[test]
    fn estimator_fill_ratio() {
        let mut est = EntropyEstimator::new(1000);
        assert_eq!(est.fill_ratio(), 0.0);
        for _ in 0..500 {
            est.update(0);
        }
        assert!((est.fill_ratio() - 0.5).abs() < 0.01);
    }

    #[test]
    fn estimator_compression_ratio_bound() {
        // Constant data → infinite compression ratio.
        let mut est = EntropyEstimator::new(1024);
        for _ in 0..1000 {
            est.update(0);
        }
        assert!(est.compression_ratio_bound().is_infinite());

        // H ≈ 4 → CR ≥ 2.
        let mut est2 = EntropyEstimator::new(10000);
        let text = b"The quick brown fox jumps over the lazy dog repeatedly ";
        for _ in 0..200 {
            est2.update_block(text);
        }
        let cr = est2.compression_ratio_bound();
        assert!(
            cr > 1.0 && cr < 4.0,
            "English text CR should be 1-4, got {cr}"
        );
    }

    // -- eviction_score -------------------------------------------------------

    #[test]
    fn eviction_score_fresh_data() {
        let config = EvictionConfig::default();
        let score = eviction_score(1000.0, 0, &config);
        assert!(
            (score - 1000.0).abs() < 0.01,
            "fresh data score should equal info cost"
        );
    }

    #[test]
    fn eviction_score_decays_with_age() {
        let config = EvictionConfig {
            recency_half_life_ms: 60_000, // 1 minute
            ..Default::default()
        };
        let fresh = eviction_score(1000.0, 0, &config);
        let one_halflife = eviction_score(1000.0, 60_000, &config);
        let two_halflives = eviction_score(1000.0, 120_000, &config);

        assert!(
            (one_halflife - 500.0).abs() < 1.0,
            "score at 1 half-life should be ~500, got {one_halflife}"
        );
        assert!(
            (two_halflives - 250.0).abs() < 1.0,
            "score at 2 half-lives should be ~250, got {two_halflives}"
        );
        assert!(fresh > one_halflife);
        assert!(one_halflife > two_halflives);
    }

    #[test]
    fn eviction_score_zero_halflife() {
        let config = EvictionConfig {
            recency_half_life_ms: 0,
            ..Default::default()
        };
        // With zero half-life, score equals info cost regardless of age.
        let score = eviction_score(500.0, 999_999, &config);
        assert!((score - 500.0).abs() < 0.01);
    }

    // -- eviction_order -------------------------------------------------------

    #[test]
    fn eviction_order_lowest_first() {
        let scores = vec![(1, 500.0), (2, 100.0), (3, 900.0), (4, 50.0)];
        let order = eviction_order(&scores);
        assert_eq!(order, vec![4, 2, 1, 3]);
    }

    #[test]
    fn eviction_order_empty() {
        let order = eviction_order(&[]);
        assert!(order.is_empty());
    }

    // -- InformationBudget ---------------------------------------------------

    #[test]
    fn budget_tracking() {
        let mut budget = InformationBudget::new(10_000.0);
        assert!(!budget.is_exceeded());
        assert_eq!(budget.pane_count, 0);
        assert_eq!(budget.utilization(), 0.0);

        budget.add(3000.0);
        budget.add(5000.0);
        assert_eq!(budget.pane_count, 2);
        assert!((budget.utilization() - 0.8).abs() < 0.01);
        assert!(!budget.is_exceeded());

        budget.add(4000.0);
        assert!(budget.is_exceeded());
        assert!((budget.overage() - 2000.0).abs() < 0.01);
    }

    #[test]
    fn budget_remove() {
        let mut budget = InformationBudget::new(10_000.0);
        budget.add(5000.0);
        budget.add(3000.0);
        budget.remove(5000.0);
        assert_eq!(budget.pane_count, 1);
        assert!((budget.current_cost - 3000.0).abs() < 0.01);
    }

    #[test]
    fn budget_remove_clamps_at_zero() {
        let mut budget = InformationBudget::new(10_000.0);
        budget.add(100.0);
        budget.remove(500.0);
        assert!((budget.current_cost - 0.0).abs() < 0.01);
    }

    #[test]
    fn budget_utilization_zero_budget() {
        let budget = InformationBudget::new(0.0);
        assert_eq!(budget.utilization(), 0.0);

        let mut budget2 = InformationBudget::new(0.0);
        budget2.add(100.0);
        assert!(budget2.utilization().is_infinite());
    }

    // -- PaneEntropySummary serde roundtrip -----------------------------------

    #[test]
    fn pane_summary_serde_roundtrip() {
        let summary = PaneEntropySummary {
            pane_id: 42,
            raw_bytes: 100_000,
            entropy: 4.5,
            information_cost: 56_250.0,
            compression_ratio_bound: 1.78,
            eviction_score: 28_125.0,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let back: PaneEntropySummary = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, 42);
        assert!((back.entropy - 4.5).abs() < 0.01);
    }

    // -- EvictionConfig serde ------------------------------------------------

    #[test]
    fn eviction_config_defaults() {
        let config = EvictionConfig::default();
        assert_eq!(config.recency_half_life_ms, 300_000);
        assert_eq!(config.min_cost_threshold, 1024.0);
    }

    #[test]
    fn eviction_config_serde_roundtrip() {
        let config = EvictionConfig {
            recency_half_life_ms: 60_000,
            min_cost_threshold: 512.0,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: EvictionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.recency_half_life_ms, 60_000);
    }

    // -- End-to-end scenario: evict low-entropy first -------------------------

    #[test]
    fn e2e_evict_low_entropy_first() {
        // Pane A: 100KB of constant data (low entropy → low cost).
        let pane_a_data = vec![0u8; 100_000];
        let pane_a_h = compute_entropy(&pane_a_data);
        let pane_a_cost = information_cost(pane_a_data.len(), pane_a_h);

        // Pane B: 10KB of "random" data (high entropy → high cost).
        let pane_b_data: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let pane_b_h = compute_entropy(&pane_b_data);
        let pane_b_cost = information_cost(pane_b_data.len(), pane_b_h);

        // Despite being 10× smaller in raw bytes, pane B has higher info cost.
        assert!(
            pane_b_cost > pane_a_cost,
            "high-entropy 10KB ({pane_b_cost:.0}) should cost more than low-entropy 100KB ({pane_a_cost:.0})"
        );

        // Eviction should target pane A first.
        let config = EvictionConfig::default();
        let scores = vec![
            (0, eviction_score(pane_a_cost, 0, &config)),
            (1, eviction_score(pane_b_cost, 0, &config)),
        ];
        let order = eviction_order(&scores);
        assert_eq!(order[0], 0, "low-entropy pane A should be evicted first");
    }
}
