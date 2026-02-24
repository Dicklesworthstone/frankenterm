//! Anytime-Valid Entropy Anomaly Detection (Aegis Semantic Plane).
//!
//! Replaces heuristic death-spiral detection with **e-processes** on Shannon
//! entropy, combined with error-signature density from a counting Bloom filter.
//!
//! # Mathematical Foundation
//!
//! ## E-processes (sequential testing)
//!
//! An e-process `(E_t)` is a non-negative supermartingale under H₀ with
//! `E[E_t | E_{t-1}] ≤ E_{t-1}`. The key property: at **any** stopping time τ,
//!
//! ```text
//! P(E_τ ≥ 1/α) ≤ α
//! ```
//!
//! This gives **anytime-valid** testing — we can stop and reject whenever
//! E_t crosses 1/α, with no penalty for peeking.
//!
//! ## Entropy collapse detection
//!
//! Under H₀ (normal operation), entropy in a sliding window is stationary
//! around a baseline. We define:
//!
//! ```text
//! e_t = likelihood_ratio(H_t | H₁) / likelihood_ratio(H_t | H₀)
//! ```
//!
//! Where H₁ = "entropy has collapsed" and H₀ = "entropy is stable".
//! The e-value accumulates multiplicatively: E_t = Π_{i=1}^{t} e_i.
//!
//! ## Combined anomaly criterion
//!
//! **Anomaly = (Collapsed Entropy) AND (High Error Signature Density)**
//!
//! This prevents false positives on progress bars (`....`) and test suites
//! (repeated pass messages), which have low entropy but no error signatures.
//!
//! Bead: ft-l5em3.4

use crate::bloom_filter::CountingBloomFilter;
use crate::entropy_accounting::EntropyEstimator;
use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the anytime-valid entropy anomaly detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntropyAnomalyConfig {
    /// Significance level α for the e-process. Anomaly fires when E_t ≥ 1/α.
    /// Default: 0.01 (99% confidence).
    pub alpha: f64,

    /// Sliding window size in bytes for entropy estimation.
    /// Default: 4096 (4 KB).
    pub window_bytes: usize,

    /// Baseline entropy range [low, high] for H₀ (normal text output).
    /// Default: [3.0, 7.0] bits/byte.
    pub baseline_entropy_low: f64,
    pub baseline_entropy_high: f64,

    /// Entropy threshold below which we consider the stream "collapsed".
    /// Default: 2.0 bits/byte.
    pub collapse_threshold: f64,

    /// Minimum consecutive collapse observations before e-value accumulates.
    /// Prevents single-chunk transients from triggering.
    /// Default: 3.
    pub min_collapse_streak: usize,

    /// Error signature bloom filter capacity.
    /// Default: 1024.
    pub signature_bloom_capacity: usize,

    /// False positive rate for the error signature bloom filter.
    /// Default: 0.01.
    pub signature_bloom_fp_rate: f64,

    /// Minimum error signature hit density (hits / total_observations)
    /// required for the AND condition.
    /// Default: 0.3 (30% of recent observations must match known error sigs).
    pub error_density_threshold: f64,

    /// E-value decay factor per observation (prevents runaway after recovery).
    /// When entropy is normal, E_t = E_{t-1} × decay.
    /// Default: 0.95.
    pub e_value_decay: f64,

    /// Maximum e-value clamp (prevents numerical overflow).
    /// Default: 1e12.
    pub max_e_value: f64,

    /// Number of observations before the detector activates.
    /// Default: 10.
    pub warmup_observations: usize,

    /// Window size for error density tracking (recent observations).
    /// Default: 50.
    pub density_window: usize,
}

impl Default for EntropyAnomalyConfig {
    fn default() -> Self {
        Self {
            alpha: 0.01,
            window_bytes: 4096,
            baseline_entropy_low: 3.0,
            baseline_entropy_high: 7.0,
            collapse_threshold: 2.0,
            min_collapse_streak: 3,
            signature_bloom_capacity: 1024,
            signature_bloom_fp_rate: 0.01,
            error_density_threshold: 0.3,
            e_value_decay: 0.95,
            max_e_value: 1e12,
            warmup_observations: 10,
            density_window: 50,
        }
    }
}

// =============================================================================
// E-process core
// =============================================================================

/// Likelihood ratio for a single entropy observation under the mixture model.
///
/// Under H₀: entropy ~ Gaussian(μ₀, σ₀²) centered in the baseline range.
/// Under H₁: entropy ~ Gaussian(μ₁, σ₁²) centered at the collapse threshold.
///
/// Returns e_t = pdf(h | H₁) / pdf(h | H₀), clamped to [min_lr, max_lr].
fn likelihood_ratio(
    h: f64,
    baseline_mean: f64,
    baseline_std: f64,
    collapse_mean: f64,
    collapse_std: f64,
) -> f64 {
    // Gaussian log-density: -0.5 * ((x - μ) / σ)² - ln(σ)
    let log_h0 = -0.5 * ((h - baseline_mean) / baseline_std).powi(2)
        - baseline_std.ln();
    let log_h1 = -0.5 * ((h - collapse_mean) / collapse_std).powi(2)
        - collapse_std.ln();

    let log_lr = log_h1 - log_h0;

    // Clamp to avoid extreme values
    log_lr.exp().clamp(1e-6, 1e6)
}

/// Running e-value state for sequential testing.
#[derive(Debug, Clone)]
pub struct EProcess {
    /// Accumulated e-value (product of likelihood ratios).
    e_value: f64,
    /// Number of observations processed.
    n_observations: usize,
    /// Current collapse streak counter.
    collapse_streak: usize,
    /// Rejection threshold (1/α).
    rejection_threshold: f64,
    /// Decay factor for recovery.
    decay: f64,
    /// Maximum e-value clamp.
    max_e: f64,
}

impl EProcess {
    /// Create a new e-process with the given significance level.
    pub fn new(alpha: f64, decay: f64, max_e: f64) -> Self {
        Self {
            e_value: 1.0,
            n_observations: 0,
            collapse_streak: 0,
            rejection_threshold: 1.0 / alpha,
            decay,
            max_e,
        }
    }

    /// Update the e-process with a new entropy observation.
    ///
    /// Returns the current e-value after the update.
    pub fn update(
        &mut self,
        entropy: f64,
        is_collapse: bool,
        baseline_mean: f64,
        baseline_std: f64,
        collapse_mean: f64,
        collapse_std: f64,
        min_streak: usize,
    ) -> f64 {
        self.n_observations += 1;

        if is_collapse {
            self.collapse_streak += 1;
            if self.collapse_streak >= min_streak {
                let lr = likelihood_ratio(
                    entropy,
                    baseline_mean,
                    baseline_std,
                    collapse_mean,
                    collapse_std,
                );
                self.e_value *= lr;
            }
        } else {
            self.collapse_streak = 0;
            // Decay toward 1.0 when entropy is normal
            self.e_value *= self.decay;
        }

        // Clamp to prevent overflow/underflow
        self.e_value = self.e_value.clamp(1e-100, self.max_e);
        self.e_value
    }

    /// Check if the e-process has rejected H₀ (entropy is anomalous).
    pub fn is_rejected(&self) -> bool {
        self.e_value >= self.rejection_threshold
    }

    /// Current e-value.
    pub fn e_value(&self) -> f64 {
        self.e_value
    }

    /// Number of observations.
    pub fn n_observations(&self) -> usize {
        self.n_observations
    }

    /// Current collapse streak length.
    pub fn collapse_streak(&self) -> usize {
        self.collapse_streak
    }

    /// Reset the e-process to initial state.
    pub fn reset(&mut self) {
        self.e_value = 1.0;
        self.n_observations = 0;
        self.collapse_streak = 0;
    }
}

// =============================================================================
// Error signature density tracker
// =============================================================================

/// Tracks the density of known error signatures in recent observations.
///
/// Uses a circular buffer of booleans (hit / no-hit) to compute the
/// fraction of recent observations that contained an error signature.
#[derive(Debug, Clone)]
pub struct ErrorDensityTracker {
    /// Circular buffer of hit flags.
    window: Vec<bool>,
    /// Current write position.
    cursor: usize,
    /// Number of hits in the window.
    hit_count: usize,
    /// Total observations recorded (may exceed window size).
    total_observations: usize,
}

impl ErrorDensityTracker {
    /// Create a new tracker with the given window size.
    pub fn new(window_size: usize) -> Self {
        Self {
            window: vec![false; window_size],
            cursor: 0,
            hit_count: 0,
            total_observations: 0,
        }
    }

    /// Record whether this observation had an error signature hit.
    pub fn record(&mut self, is_error_hit: bool) {
        let old = self.window[self.cursor];
        if old {
            self.hit_count -= 1;
        }
        self.window[self.cursor] = is_error_hit;
        if is_error_hit {
            self.hit_count += 1;
        }
        self.cursor = (self.cursor + 1) % self.window.len();
        self.total_observations += 1;
    }

    /// Current error density (hits / window_occupied).
    pub fn density(&self) -> f64 {
        let occupied = self.total_observations.min(self.window.len());
        if occupied == 0 {
            return 0.0;
        }
        self.hit_count as f64 / occupied as f64
    }

    /// Number of hits in the current window.
    pub fn hit_count(&self) -> usize {
        self.hit_count
    }

    /// Total observations recorded.
    pub fn total_observations(&self) -> usize {
        self.total_observations
    }

    /// Reset the tracker.
    pub fn reset(&mut self) {
        self.window.fill(false);
        self.cursor = 0;
        self.hit_count = 0;
        self.total_observations = 0;
    }
}

// =============================================================================
// Anomaly decision
// =============================================================================

/// The verdict from the anomaly detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnomalyDecision {
    /// Whether the detector is triggering an anomaly block.
    pub should_block: bool,

    /// Current e-value from the e-process.
    pub e_value: f64,

    /// Rejection threshold (1/α).
    pub rejection_threshold: f64,

    /// Whether entropy is currently collapsed.
    pub entropy_collapsed: bool,

    /// Whether error density is above threshold.
    pub error_density_high: bool,

    /// Current Shannon entropy (bits/byte).
    pub current_entropy: f64,

    /// Current error signature density.
    pub error_density: f64,

    /// Number of observations since reset.
    pub n_observations: usize,

    /// Current collapse streak.
    pub collapse_streak: usize,

    /// Whether the detector is still warming up.
    pub warming_up: bool,
}

// =============================================================================
// Per-pane anomaly state
// =============================================================================

/// Per-pane state for the anomaly detector.
pub struct PaneAnomalyState {
    /// E-process for entropy anomaly testing.
    e_process: EProcess,
    /// Entropy estimator (sliding window).
    entropy_estimator: EntropyEstimator,
    /// Error signature bloom filter.
    signature_bloom: CountingBloomFilter,
    /// Error density tracker.
    error_density: ErrorDensityTracker,
    /// Pane identifier.
    pane_id: u64,
    /// Last computed entropy.
    last_entropy: f64,
}

impl PaneAnomalyState {
    /// Create a new per-pane anomaly state.
    pub fn new(pane_id: u64, config: &EntropyAnomalyConfig) -> Self {
        Self {
            e_process: EProcess::new(config.alpha, config.e_value_decay, config.max_e_value),
            entropy_estimator: EntropyEstimator::new(config.window_bytes),
            signature_bloom: CountingBloomFilter::with_capacity(
                config.signature_bloom_capacity,
                config.signature_bloom_fp_rate,
            ),
            error_density: ErrorDensityTracker::new(config.density_window),
            pane_id,
            last_entropy: 5.0, // start at mid-range
        }
    }

    /// Get the pane ID.
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Current e-value.
    pub fn e_value(&self) -> f64 {
        self.e_process.e_value()
    }

    /// Current entropy.
    pub fn last_entropy(&self) -> f64 {
        self.last_entropy
    }

    /// Current error density.
    pub fn error_density(&self) -> f64 {
        self.error_density.density()
    }
}

// =============================================================================
// Main detector
// =============================================================================

/// Anytime-valid entropy anomaly detector combining e-processes with
/// error signature bloom filters.
///
/// # Usage
///
/// ```rust,ignore
/// let mut detector = EntropyAnomalyDetector::new(EntropyAnomalyConfig::default());
///
/// // Register known error signatures
/// detector.register_error_signature(b"error[E0308]: mismatched types");
/// detector.register_error_signature(b"FAILED");
///
/// // Feed data chunks and check for anomalies
/// let decision = detector.observe(pane_id, &chunk, &[b"error[E0308]"]);
/// if decision.should_block {
///     // Inject intervention
/// }
/// ```
pub struct EntropyAnomalyDetector {
    /// Configuration.
    config: EntropyAnomalyConfig,
    /// Per-pane state.
    pane_states: std::collections::HashMap<u64, PaneAnomalyState>,
    /// Global error signature bloom filter (shared known signatures).
    global_signatures: CountingBloomFilter,
    /// Baseline entropy statistics (adaptive).
    baseline_mean: f64,
    baseline_variance: f64,
    baseline_n: usize,
}

impl EntropyAnomalyDetector {
    /// Create a new detector with the given configuration.
    pub fn new(config: EntropyAnomalyConfig) -> Self {
        let global_signatures = CountingBloomFilter::with_capacity(
            config.signature_bloom_capacity,
            config.signature_bloom_fp_rate,
        );
        let baseline_mean = (config.baseline_entropy_low + config.baseline_entropy_high) / 2.0;
        Self {
            config,
            pane_states: std::collections::HashMap::new(),
            global_signatures,
            baseline_mean,
            baseline_variance: 2.0, // wide initial prior
            baseline_n: 0,
        }
    }

    /// Create a detector with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(EntropyAnomalyConfig::default())
    }

    /// Register a known error signature in the global bloom filter.
    pub fn register_error_signature(&mut self, signature: &[u8]) {
        self.global_signatures.insert(signature);
    }

    /// Remove a known error signature from the global bloom filter.
    pub fn remove_error_signature(&mut self, signature: &[u8]) {
        self.global_signatures.remove(signature);
    }

    /// Check if a signature is known (probabilistic).
    pub fn is_known_signature(&self, signature: &[u8]) -> bool {
        self.global_signatures.contains(signature)
    }

    /// Observe a data chunk from a pane, optionally with error signatures found.
    ///
    /// `data` is the raw terminal output chunk.
    /// `error_signatures` is a list of error signature byte slices found in this chunk.
    ///
    /// Returns an `AnomalyDecision` indicating whether to block.
    pub fn observe(
        &mut self,
        pane_id: u64,
        data: &[u8],
        error_signatures: &[&[u8]],
    ) -> AnomalyDecision {
        // Ensure per-pane state exists
        let config = &self.config;
        let state = self
            .pane_states
            .entry(pane_id)
            .or_insert_with(|| PaneAnomalyState::new(pane_id, config));

        // 1. Update entropy estimator
        state.entropy_estimator.update_block(data);
        let entropy = state.entropy_estimator.entropy();
        state.last_entropy = entropy;

        // 2. Check for error signature hits
        let has_error_hit = !error_signatures.is_empty()
            || data.windows(5).any(|w| self.global_signatures.contains(w));

        // Register new signatures in the per-pane bloom
        for sig in error_signatures {
            state.signature_bloom.insert(sig);
            // Also add to global
            self.global_signatures.insert(sig);
        }

        // 3. Update error density tracker
        state.error_density.record(has_error_hit);

        // 4. Determine if entropy is collapsed
        let is_collapse = entropy < config.collapse_threshold;

        // 5. Compute baseline statistics
        let baseline_std = self.baseline_variance.sqrt().max(0.5);
        let collapse_mean = config.collapse_threshold / 2.0;
        let collapse_std = config.collapse_threshold / 3.0;

        // 6. Update e-process
        state.e_process.update(
            entropy,
            is_collapse,
            self.baseline_mean,
            baseline_std,
            collapse_mean,
            collapse_std.max(0.1),
            config.min_collapse_streak,
        );

        // 7. Update baseline if entropy is in normal range
        if entropy >= config.baseline_entropy_low && entropy <= config.baseline_entropy_high {
            self.baseline_n += 1;
            let n = self.baseline_n as f64;
            let delta = entropy - self.baseline_mean;
            self.baseline_mean += delta / n;
            if n > 1.0 {
                let delta2 = entropy - self.baseline_mean;
                self.baseline_variance += (delta * delta2 - self.baseline_variance) / n;
            }
        }

        // 8. Combined decision
        let warming_up = state.e_process.n_observations() < config.warmup_observations;
        let entropy_collapsed = state.e_process.is_rejected();
        let error_density_val = state.error_density.density();
        let error_density_high = error_density_val >= config.error_density_threshold;

        // Anomaly = collapsed entropy AND high error density (AND not warming up)
        let should_block = !warming_up && entropy_collapsed && error_density_high;

        AnomalyDecision {
            should_block,
            e_value: state.e_process.e_value(),
            rejection_threshold: 1.0 / config.alpha,
            entropy_collapsed,
            error_density_high,
            current_entropy: entropy,
            error_density: error_density_val,
            n_observations: state.e_process.n_observations(),
            collapse_streak: state.e_process.collapse_streak(),
            warming_up,
        }
    }

    /// Get a snapshot of a pane's anomaly state.
    pub fn pane_snapshot(&self, pane_id: u64) -> Option<PaneAnomalySnapshot> {
        self.pane_states.get(&pane_id).map(|s| PaneAnomalySnapshot {
            pane_id,
            e_value: s.e_process.e_value(),
            n_observations: s.e_process.n_observations(),
            collapse_streak: s.e_process.collapse_streak(),
            last_entropy: s.last_entropy,
            error_density: s.error_density.density(),
            error_hits: s.error_density.hit_count(),
        })
    }

    /// Get snapshots for all tracked panes.
    pub fn all_snapshots(&self) -> Vec<PaneAnomalySnapshot> {
        self.pane_states
            .keys()
            .filter_map(|&pid| self.pane_snapshot(pid))
            .collect()
    }

    /// Reset a specific pane's anomaly state.
    pub fn reset_pane(&mut self, pane_id: u64) {
        self.pane_states.remove(&pane_id);
    }

    /// Reset all pane states and baseline statistics.
    pub fn reset(&mut self) {
        self.pane_states.clear();
        self.baseline_mean =
            (self.config.baseline_entropy_low + self.config.baseline_entropy_high) / 2.0;
        self.baseline_variance = 2.0;
        self.baseline_n = 0;
    }

    /// Number of tracked panes.
    pub fn pane_count(&self) -> usize {
        self.pane_states.len()
    }

    /// Access the configuration.
    pub fn config(&self) -> &EntropyAnomalyConfig {
        &self.config
    }

    /// Current baseline entropy mean.
    pub fn baseline_mean(&self) -> f64 {
        self.baseline_mean
    }

    /// Current baseline entropy variance.
    pub fn baseline_variance(&self) -> f64 {
        self.baseline_variance
    }
}

/// Snapshot of a pane's anomaly detection state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneAnomalySnapshot {
    /// Pane identifier.
    pub pane_id: u64,
    /// Current e-value.
    pub e_value: f64,
    /// Number of observations.
    pub n_observations: usize,
    /// Current collapse streak.
    pub collapse_streak: usize,
    /// Last computed entropy (bits/byte).
    pub last_entropy: f64,
    /// Current error density.
    pub error_density: f64,
    /// Error hits in the current window.
    pub error_hits: usize,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── EProcess tests ────────────────────────────────────────────────────

    #[test]
    fn e_process_initial_state() {
        let ep = EProcess::new(0.01, 0.95, 1e12);
        assert!((ep.e_value() - 1.0).abs() < 1e-10);
        assert_eq!(ep.n_observations(), 0);
        assert_eq!(ep.collapse_streak(), 0);
        assert!(!ep.is_rejected());
    }

    #[test]
    fn e_process_normal_entropy_decays() {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        // Feed normal entropy (not collapse)
        for _ in 0..10 {
            ep.update(5.0, false, 5.0, 1.0, 1.0, 0.5, 3);
        }
        // E-value should decay below 1.0
        assert!(ep.e_value() < 1.0);
        assert_eq!(ep.n_observations(), 10);
        assert!(!ep.is_rejected());
    }

    #[test]
    fn e_process_collapse_accumulates() {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        // Feed collapsed entropy with streak >= min_streak
        for _ in 0..20 {
            ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        }
        // E-value should be large
        assert!(ep.e_value() > 1.0);
    }

    #[test]
    fn e_process_rejects_on_sustained_collapse() {
        let mut ep = EProcess::new(0.05, 0.95, 1e12);
        // Sustained collapse should eventually reject
        for _ in 0..50 {
            ep.update(0.3, true, 5.0, 1.0, 0.5, 0.3, 3);
        }
        assert!(ep.is_rejected());
    }

    #[test]
    fn e_process_recovers_with_normal_entropy() {
        let mut ep = EProcess::new(0.01, 0.5, 1e12);
        // Build up e-value
        for _ in 0..10 {
            ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        }
        let peak = ep.e_value();
        // Recovery with aggressive decay
        for _ in 0..50 {
            ep.update(5.0, false, 5.0, 1.0, 0.5, 0.3, 3);
        }
        assert!(ep.e_value() < peak);
    }

    #[test]
    fn e_process_streak_gate() {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        // Collapse but below min_streak (3) — should not accumulate
        ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        let after_2 = ep.e_value();
        // E-value should still be 1.0 (no accumulation before streak met)
        assert!((after_2 - 1.0).abs() < 1e-10);

        // Third collapse crosses streak threshold
        ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        assert!(ep.e_value() > 1.0);
    }

    #[test]
    fn e_process_streak_resets_on_normal() {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        assert_eq!(ep.collapse_streak(), 2);

        // Normal observation resets streak
        ep.update(5.0, false, 5.0, 1.0, 0.5, 0.3, 3);
        assert_eq!(ep.collapse_streak(), 0);
    }

    #[test]
    fn e_process_reset() {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        for _ in 0..20 {
            ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 3);
        }
        ep.reset();
        assert!((ep.e_value() - 1.0).abs() < 1e-10);
        assert_eq!(ep.n_observations(), 0);
        assert_eq!(ep.collapse_streak(), 0);
    }

    #[test]
    fn e_process_clamp_prevents_overflow() {
        let mut ep = EProcess::new(0.001, 0.99, 100.0);
        for _ in 0..1000 {
            ep.update(0.1, true, 5.0, 1.0, 0.1, 0.1, 1);
        }
        assert!(ep.e_value() <= 100.0);
    }

    // ── ErrorDensityTracker tests ─────────────────────────────────────────

    #[test]
    fn density_tracker_empty() {
        let tracker = ErrorDensityTracker::new(10);
        assert!((tracker.density()).abs() < 1e-10);
        assert_eq!(tracker.hit_count(), 0);
        assert_eq!(tracker.total_observations(), 0);
    }

    #[test]
    fn density_tracker_all_hits() {
        let mut tracker = ErrorDensityTracker::new(10);
        for _ in 0..10 {
            tracker.record(true);
        }
        assert!((tracker.density() - 1.0).abs() < 1e-10);
        assert_eq!(tracker.hit_count(), 10);
    }

    #[test]
    fn density_tracker_no_hits() {
        let mut tracker = ErrorDensityTracker::new(10);
        for _ in 0..10 {
            tracker.record(false);
        }
        assert!((tracker.density()).abs() < 1e-10);
    }

    #[test]
    fn density_tracker_half_hits() {
        let mut tracker = ErrorDensityTracker::new(10);
        for i in 0..10 {
            tracker.record(i % 2 == 0);
        }
        assert!((tracker.density() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn density_tracker_window_eviction() {
        let mut tracker = ErrorDensityTracker::new(5);
        // Fill with 5 hits
        for _ in 0..5 {
            tracker.record(true);
        }
        assert!((tracker.density() - 1.0).abs() < 1e-10);

        // Now push 5 non-hits (evicting the hits)
        for _ in 0..5 {
            tracker.record(false);
        }
        assert!((tracker.density()).abs() < 1e-10);
    }

    #[test]
    fn density_tracker_reset() {
        let mut tracker = ErrorDensityTracker::new(10);
        for _ in 0..5 {
            tracker.record(true);
        }
        tracker.reset();
        assert!((tracker.density()).abs() < 1e-10);
        assert_eq!(tracker.hit_count(), 0);
        assert_eq!(tracker.total_observations(), 0);
    }

    // ── Likelihood ratio tests ────────────────────────────────────────────

    #[test]
    fn lr_at_h1_mean_is_large() {
        // Observation right at the collapse mean should have high LR
        let lr = likelihood_ratio(0.5, 5.0, 1.0, 0.5, 0.3);
        assert!(lr > 1.0);
    }

    #[test]
    fn lr_at_h0_mean_is_small() {
        // Observation at baseline mean should have low LR
        let lr = likelihood_ratio(5.0, 5.0, 1.0, 0.5, 0.3);
        assert!(lr < 1.0);
    }

    #[test]
    fn lr_monotone_toward_h1() {
        // LR should be highest near H₁ mean (0.5), lower far from it
        let lr_at_h1 = likelihood_ratio(0.5, 5.0, 1.0, 0.5, 0.3);
        let lr_at_h0 = likelihood_ratio(5.0, 5.0, 1.0, 0.5, 0.3);
        // At H₁ mean, H₁ is much more likely than H₀
        assert!(
            lr_at_h1 > lr_at_h0,
            "LR at H₁ mean ({}) should exceed LR at H₀ mean ({})",
            lr_at_h1,
            lr_at_h0
        );
    }

    #[test]
    fn lr_is_positive() {
        // Likelihood ratio must always be positive
        for h in [0.0, 1.0, 2.5, 5.0, 7.5, 8.0] {
            let lr = likelihood_ratio(h, 5.0, 1.0, 1.0, 0.5);
            assert!(lr > 0.0, "LR must be positive for h={}", h);
        }
    }

    // ── Detector integration tests ────────────────────────────────────────

    #[test]
    fn detector_no_block_on_diverse_text() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        // Simulate diverse text output (high entropy)
        let diverse: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        for _ in 0..20 {
            let decision = det.observe(1, &diverse, &[]);
            assert!(!decision.should_block);
        }
    }

    #[test]
    fn detector_no_block_on_progress_bar() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        // Progress bar: low entropy, but NO error signatures
        let progress = vec![b'.'; 4096];
        for _ in 0..50 {
            let decision = det.observe(1, &progress, &[]);
            // Should NOT block — low entropy but no error signatures
            assert!(
                !decision.should_block,
                "Progress bar should not trigger block (e_value={}, error_density={})",
                decision.e_value,
                decision.error_density
            );
        }
    }

    #[test]
    fn detector_blocks_on_repeated_errors() {
        // Compute the actual entropy of the error output to set threshold correctly
        let error_output = b"error[E0308]: mismatched types\nerror[E0308]: mismatched types\nerror[E0308]: mismatched types\nerror[E0308]: mismatched types\nerror[E0308]: mismatched types\n";
        let actual_entropy = crate::entropy_accounting::compute_entropy(error_output);

        let config = EntropyAnomalyConfig {
            alpha: 0.05,
            warmup_observations: 3,
            min_collapse_streak: 2,
            window_bytes: 256,
            // Set threshold above the actual entropy of the repeated error
            collapse_threshold: actual_entropy + 1.0,
            error_density_threshold: 0.2,
            density_window: 10,
            baseline_entropy_low: actual_entropy + 1.5,
            baseline_entropy_high: 7.0,
            ..Default::default()
        };
        let mut det = EntropyAnomalyDetector::new(config);

        // Register known error signature
        det.register_error_signature(b"error[E0308]");

        let mut blocked = false;
        for _ in 0..200 {
            let decision = det.observe(1, error_output, &[b"error[E0308]"]);
            if decision.should_block {
                blocked = true;
                break;
            }
        }
        assert!(blocked, "Repeated errors should eventually trigger block (entropy={})", actual_entropy);
    }

    #[test]
    fn detector_multi_pane_isolation() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        // Pane 1: diverse text
        let diverse: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        det.observe(1, &diverse, &[]);
        // Pane 2: low entropy
        let low = vec![0u8; 4096];
        det.observe(2, &low, &[]);

        let snap1 = det.pane_snapshot(1).unwrap();
        let snap2 = det.pane_snapshot(2).unwrap();
        assert!(snap1.last_entropy > snap2.last_entropy);
        assert_eq!(snap1.pane_id, 1);
        assert_eq!(snap2.pane_id, 2);
    }

    #[test]
    fn detector_pane_reset() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        det.observe(1, &data, &[]);
        assert!(det.pane_snapshot(1).is_some());

        det.reset_pane(1);
        assert!(det.pane_snapshot(1).is_none());
    }

    #[test]
    fn detector_global_reset() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        det.observe(1, &data, &[]);
        det.observe(2, &data, &[]);
        assert_eq!(det.pane_count(), 2);

        det.reset();
        assert_eq!(det.pane_count(), 0);
    }

    #[test]
    fn detector_warmup_prevents_block() {
        let config = EntropyAnomalyConfig {
            warmup_observations: 10,
            alpha: 0.5, // very loose
            min_collapse_streak: 1,
            window_bytes: 64,
            collapse_threshold: 3.0,
            error_density_threshold: 0.0, // always meets density
            ..Default::default()
        };
        let mut det = EntropyAnomalyDetector::new(config);
        let low = vec![0u8; 64];
        // Observations 0..9 (n_observations 1..10) should all be warming up
        for i in 0..9 {
            let decision = det.observe(1, &low, &[b"error"]);
            assert!(decision.warming_up, "Should be warming up at obs {}", i);
            assert!(!decision.should_block, "Must not block during warmup");
        }
    }

    #[test]
    fn detector_config_serde_roundtrip() {
        let config = EntropyAnomalyConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: EntropyAnomalyConfig = serde_json::from_str(&json).unwrap();
        assert!((config.alpha - back.alpha).abs() < 1e-10);
        assert_eq!(config.window_bytes, back.window_bytes);
        assert_eq!(config.warmup_observations, back.warmup_observations);
    }

    #[test]
    fn detector_snapshot_serde_roundtrip() {
        let snap = PaneAnomalySnapshot {
            pane_id: 42,
            e_value: 3.14,
            n_observations: 100,
            collapse_streak: 5,
            last_entropy: 2.1,
            error_density: 0.5,
            error_hits: 25,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PaneAnomalySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap.pane_id, back.pane_id);
        assert!((snap.e_value - back.e_value).abs() < 1e-10);
        assert_eq!(snap.error_hits, back.error_hits);
    }

    #[test]
    fn detector_decision_serde_roundtrip() {
        let decision = AnomalyDecision {
            should_block: true,
            e_value: 150.0,
            rejection_threshold: 100.0,
            entropy_collapsed: true,
            error_density_high: true,
            current_entropy: 0.5,
            error_density: 0.8,
            n_observations: 50,
            collapse_streak: 10,
            warming_up: false,
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: AnomalyDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decision.should_block, back.should_block);
        assert!((decision.e_value - back.e_value).abs() < 1e-10);
    }

    #[test]
    fn detector_signature_registration() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        det.register_error_signature(b"FAILED");
        assert!(det.is_known_signature(b"FAILED"));
        det.remove_error_signature(b"FAILED");
        assert!(!det.is_known_signature(b"FAILED"));
    }

    #[test]
    fn detector_all_snapshots() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        det.observe(1, &data, &[]);
        det.observe(2, &data, &[]);
        det.observe(3, &data, &[]);
        let snaps = det.all_snapshots();
        assert_eq!(snaps.len(), 3);
    }

    #[test]
    fn detector_baseline_adapts() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        // Feed data with known entropy in normal range
        let data: Vec<u8> = (0..4096).map(|i| (i % 128 + 32) as u8).collect();
        for _ in 0..50 {
            det.observe(1, &data, &[]);
        }
        // Baseline should have adapted from initial prior
        assert!(det.baseline_n > 0);
    }

    // ── E2E scenario: test suite output (no false positive) ───────────────

    #[test]
    fn e2e_test_suite_output_no_false_positive() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        // Simulate test suite: repetitive pass messages (low entropy, no errors)
        let test_output = b"test module::test_foo ... ok\ntest module::test_bar ... ok\ntest module::test_baz ... ok\n";
        for _ in 0..100 {
            let decision = det.observe(1, test_output, &[]);
            assert!(
                !decision.should_block,
                "Test suite pass output must not trigger block"
            );
        }
    }

    // ── E2E scenario: cargo build error loop ─────────────────────────────

    #[test]
    fn e2e_cargo_error_loop_blocks() {
        let error_msg = b"error[E0308]: mismatched types\n  --> src/main.rs:42:5\n   |\n42 |     let x: u32 = \"hello\";\n   |                  ^^^^^^^ expected `u32`, found `&str`\n\nerror: aborting due to 1 previous error\n";
        let actual_entropy = crate::entropy_accounting::compute_entropy(error_msg);

        let config = EntropyAnomalyConfig {
            alpha: 0.05,
            warmup_observations: 5,
            min_collapse_streak: 2,
            window_bytes: 512,
            // Set collapse threshold above actual entropy of error output
            collapse_threshold: actual_entropy + 1.0,
            baseline_entropy_low: actual_entropy + 1.5,
            baseline_entropy_high: 7.5,
            error_density_threshold: 0.3,
            density_window: 20,
            ..Default::default()
        };
        let mut det = EntropyAnomalyDetector::new(config);
        det.register_error_signature(b"error[E");

        let mut blocked = false;
        for _ in 0..200 {
            let decision = det.observe(1, error_msg, &[b"error[E"]);
            if decision.should_block {
                blocked = true;
                break;
            }
        }
        assert!(
            blocked,
            "Repeated cargo error should trigger block (entropy={})",
            actual_entropy
        );
    }

    // ── E2E scenario: mixed normal then collapse ─────────────────────────

    #[test]
    fn e2e_normal_then_collapse_transition() {
        let error_loop = b"ERROR: connection refused\nERROR: connection refused\nERROR: connection refused\n";
        let error_entropy = crate::entropy_accounting::compute_entropy(error_loop);

        let config = EntropyAnomalyConfig {
            alpha: 0.05,
            warmup_observations: 5,
            window_bytes: 256,
            min_collapse_streak: 3,
            collapse_threshold: error_entropy + 1.0,
            baseline_entropy_low: error_entropy + 1.5,
            baseline_entropy_high: 7.5,
            error_density_threshold: 0.2,
            density_window: 20,
            ..Default::default()
        };
        let mut det = EntropyAnomalyDetector::new(config);
        det.register_error_signature(b"ERROR");

        // Phase 1: Normal diverse output
        let diverse: Vec<u8> = (0..256).map(|i| (i % 200 + 32) as u8).collect();
        for _ in 0..30 {
            let decision = det.observe(1, &diverse, &[]);
            assert!(!decision.should_block, "Normal output should not block");
        }

        // Phase 2: Sudden collapse to repeating error
        let mut eventually_blocked = false;
        for _ in 0..200 {
            let decision = det.observe(1, error_loop, &[b"ERROR"]);
            if decision.should_block {
                eventually_blocked = true;
                break;
            }
        }
        assert!(
            eventually_blocked,
            "Transition from normal to error loop should eventually block (entropy={})",
            error_entropy
        );
    }

    #[test]
    fn e_process_e_value_non_negative() {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        for _ in 0..100 {
            ep.update(0.1, true, 5.0, 1.0, 0.5, 0.3, 1);
            assert!(ep.e_value() > 0.0, "E-value must always be positive");
        }
        for _ in 0..100 {
            ep.update(5.0, false, 5.0, 1.0, 0.5, 0.3, 1);
            assert!(ep.e_value() > 0.0, "E-value must always be positive");
        }
    }

    #[test]
    fn density_tracker_partial_fill() {
        let mut tracker = ErrorDensityTracker::new(10);
        tracker.record(true);
        tracker.record(false);
        tracker.record(true);
        // Only 3 observations in a window of 10 — density should be 2/3
        assert!((tracker.density() - 2.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn detector_e_value_starts_at_one() {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        let decision = det.observe(1, &data, &[]);
        // After first observation with normal entropy, e-value should be
        // close to 1.0 (slightly decayed)
        assert!(
            decision.e_value < 1.1 && decision.e_value > 0.5,
            "e_value should be near 1.0, got {}",
            decision.e_value
        );
    }
}
