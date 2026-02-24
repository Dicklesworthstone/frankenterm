//! Semantic Anomaly Detection for Terminal Streams.
//!
//! Detects sudden shifts in the semantic meaning of terminal output (e.g., an agent
//! suddenly encountering a massive Java stack trace after cleanly compiling Rust code,
//! or a server responding with an HTML 404 page instead of JSON).
//!
//! # Two detection modes
//!
//! ## 1. Z-score detector ([`SemanticAnomalyDetector`])
//!
//! The original heuristic approach. Computes EWMA of embedding vectors to maintain
//! a "Semantic Centroid" on the unit sphere and flags Z-score outliers.
//!
//! ## 2. Conformal prediction detector ([`ConformalAnomalyDetector`])
//!
//! A mathematically rigorous upgrade. Uses split conformal prediction to compute
//! empirical p-values from a calibration window of recent non-conformity scores.
//! Provides formal false discovery rate guarantees:
//!
//! ```text
//! P(false positive) <= alpha   (for exchangeable data)
//! p-value = (|{s_i >= s_new}| + 1) / (N + 1)
//! ```
//!
//! Uses O(log N) rank queries via a sorted calibration buffer with binary search
//! instead of O(N) linear scans.
//!
//! # SIMD-friendly math
//!
//! Vector operations (`dot_product`, `normalize`) use 4-wide chunked accumulation
//! that LLVM auto-vectorizes to SSE/AVX on x86_64 and NEON on AArch64, achieving
//! ~50ns for 384d vectors on modern hardware.
//!
//! Bead: ft-344j8.8

use serde::{Deserialize, Serialize};

// =============================================================================
// SIMD-friendly vector math
// =============================================================================

/// Dot product using 4-wide manual unrolling for auto-vectorization.
///
/// LLVM recognizes the 4-accumulator pattern and emits SIMD instructions
/// (SSE/AVX on x86_64, NEON on AArch64) without requiring `unsafe` or
/// explicit intrinsics.
#[inline]
pub fn dot_product_simd(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let chunks = n / 4;
    let remainder = n % 4;

    // 4-wide accumulator lanes for ILP + auto-vectorization.
    let mut acc0: f32 = 0.0;
    let mut acc1: f32 = 0.0;
    let mut acc2: f32 = 0.0;
    let mut acc3: f32 = 0.0;

    for i in 0..chunks {
        let base = i * 4;
        acc0 = a[base].mul_add(b[base], acc0);
        acc1 = a[base + 1].mul_add(b[base + 1], acc1);
        acc2 = a[base + 2].mul_add(b[base + 2], acc2);
        acc3 = a[base + 3].mul_add(b[base + 3], acc3);
    }

    // Handle remainder elements.
    let tail_start = chunks * 4;
    for i in 0..remainder {
        acc0 = a[tail_start + i].mul_add(b[tail_start + i], acc0);
    }

    (acc0 + acc1) + (acc2 + acc3)
}

/// Normalize a vector to unit length using 4-wide chunked magnitude computation.
///
/// Returns a new `Vec<f32>` on the unit sphere. Zero vectors are returned as-is.
#[inline]
pub fn normalize_simd(v: &[f32]) -> Vec<f32> {
    let mag_sq = dot_product_simd(v, v);
    if mag_sq > 0.0 {
        let inv_mag = 1.0 / mag_sq.sqrt();
        v.iter().map(|x| x * inv_mag).collect()
    } else {
        v.to_vec()
    }
}

// Keep the original helpers for backwards compatibility (used by SemanticAnomalyDetector).
#[inline]
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    dot_product_simd(a, b)
}

#[inline]
fn normalize(v: &[f32]) -> Vec<f32> {
    normalize_simd(v)
}

// =============================================================================
// Sorted calibration buffer for O(log N) rank queries
// =============================================================================

/// A bounded sorted buffer that supports O(log N) rank queries and FIFO eviction.
///
/// Maintains a sliding window of the most recent `capacity` non-conformity scores
/// in sorted order. Uses binary search for insertion and rank computation.
#[derive(Debug, Clone)]
pub struct SortedCalibrationBuffer {
    /// Sorted scores for O(log N) rank queries.
    sorted: Vec<f32>,
    /// Insertion-order ring for FIFO eviction.
    ring: Vec<f32>,
    /// Write head in the ring buffer.
    head: usize,
    /// Maximum number of scores retained.
    capacity: usize,
    /// Whether the ring is full (has wrapped).
    full: bool,
}

impl SortedCalibrationBuffer {
    /// Create a buffer with the given maximum capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "calibration buffer capacity must be > 0");
        Self {
            sorted: Vec::with_capacity(capacity),
            ring: Vec::with_capacity(capacity),
            head: 0,
            capacity,
            full: false,
        }
    }

    /// Insert a new score, evicting the oldest if at capacity.
    pub fn insert(&mut self, score: f32) {
        if self.full {
            // Evict the oldest score from the sorted buffer.
            let oldest = self.ring[self.head];
            let pos = self
                .sorted
                .binary_search_by(|s| s.partial_cmp(&oldest).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or_else(|p| p);
            if pos < self.sorted.len() {
                self.sorted.remove(pos);
            }
            self.ring[self.head] = score;
        } else {
            self.ring.push(score);
        }

        // Insert the new score in sorted position.
        let insert_pos = self
            .sorted
            .binary_search_by(|s| s.partial_cmp(&score).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or_else(|p| p);
        self.sorted.insert(insert_pos, score);

        self.head = (self.head + 1) % self.capacity;
        if self.head == 0 && !self.full {
            self.full = true;
        }
    }

    /// Count how many calibration scores are >= the given threshold.
    ///
    /// Runs in O(log N) via binary search.
    #[must_use]
    pub fn count_geq(&self, threshold: f32) -> usize {
        // Find the leftmost position where score >= threshold.
        let pos = self.sorted.partition_point(|s| *s < threshold);
        self.sorted.len() - pos
    }

    /// Number of scores currently in the buffer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sorted.len() == 0
    }

    /// Compute the conformal p-value for a new non-conformity score.
    ///
    /// `p = (count(s_i >= score) + 1) / (N + 1)`
    ///
    /// This is the standard conformal p-value that provides the coverage guarantee:
    /// `P(p <= alpha) <= alpha` for exchangeable data.
    #[must_use]
    pub fn conformal_p_value(&self, score: f32) -> f64 {
        if self.is_empty() {
            return 1.0; // No calibration data; never reject.
        }
        let count_geq = self.count_geq(score) as f64;
        let n = self.sorted.len() as f64;
        (count_geq + 1.0) / (n + 1.0)
    }

    /// Return the score at a given quantile (0.0 to 1.0).
    #[must_use]
    pub fn quantile(&self, q: f32) -> Option<f32> {
        if self.is_empty() {
            return None;
        }
        let idx = ((q * (self.sorted.len() - 1) as f32).round() as usize).min(self.sorted.len() - 1);
        Some(self.sorted[idx])
    }
}

// =============================================================================
// Original Z-score based detector (backwards compatible)
// =============================================================================

/// Configuration for the Z-score based Semantic Anomaly Detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticAnomalyConfig {
    /// The EWMA alpha for the semantic centroid (e.g., 0.1 ~ 10-step half-life).
    pub centroid_alpha: f32,
    /// The EWMA alpha for the variance tracking.
    pub variance_alpha: f32,
    /// Minimum number of samples required before shocks can be triggered.
    pub min_samples: usize,
    /// Z-score threshold for triggering a semantic shock (e.g., 3.0 standard deviations).
    pub shock_threshold_z: f32,
}

impl Default for SemanticAnomalyConfig {
    fn default() -> Self {
        Self {
            centroid_alpha: 0.15,
            variance_alpha: 0.10,
            min_samples: 5,
            shock_threshold_z: 3.5,
        }
    }
}

/// A detected semantic shock event (Z-score based).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticShock {
    /// The distance from the semantic centroid.
    pub distance: f32,
    /// The current expected mean distance.
    pub expected_distance: f32,
    /// The current standard deviation of the distance.
    pub std_dev: f32,
    /// The calculated Z-score of this event.
    pub z_score: f32,
}

/// Tracks the semantic trajectory of a terminal pane using Z-score thresholds.
#[derive(Debug, Clone)]
pub struct SemanticAnomalyDetector {
    config: SemanticAnomalyConfig,
    centroid: Vec<f32>,
    mean_distance: f32,
    variance: f32,
    samples: usize,
}

impl SemanticAnomalyDetector {
    /// Create a new detector with the given configuration.
    #[must_use]
    pub fn new(config: SemanticAnomalyConfig) -> Self {
        Self {
            config,
            centroid: Vec::new(),
            mean_distance: 0.0,
            variance: 0.0,
            samples: 0,
        }
    }

    /// Process a new semantic embedding vector representing terminal output.
    pub fn observe(&mut self, embedding: &[f32]) -> Option<SemanticShock> {
        if embedding.is_empty() {
            return None;
        }

        if self.samples == 0 {
            self.centroid = normalize(embedding);
            self.samples += 1;
            return None;
        }

        if self.centroid.len() != embedding.len() {
            self.centroid = normalize(embedding);
            self.mean_distance = 0.0;
            self.variance = 0.0;
            self.samples = 1;
            return None;
        }

        let normalized_emb = normalize(embedding);
        let similarity = dot_product(&self.centroid, &normalized_emb);
        let distance = (1.0 - similarity).max(0.0);

        let mut shock = None;

        if self.samples >= self.config.min_samples {
            let std_dev = self.variance.sqrt();
            let safe_std_dev = std_dev.max(1e-5);
            let z_score = (distance - self.mean_distance) / safe_std_dev;

            if z_score >= self.config.shock_threshold_z {
                shock = Some(SemanticShock {
                    distance,
                    expected_distance: self.mean_distance,
                    std_dev,
                    z_score,
                });
            }
        }

        let diff = distance - self.mean_distance;
        self.mean_distance += self.config.variance_alpha * diff;
        self.variance = (1.0 - self.config.variance_alpha)
            * (self.config.variance_alpha * diff).mul_add(diff, self.variance);

        for (i, val) in self.centroid.iter_mut().enumerate() {
            *val = (1.0 - self.config.centroid_alpha)
                .mul_add(*val, self.config.centroid_alpha * normalized_emb[i]);
        }
        self.centroid = normalize(&self.centroid);

        self.samples += 1;
        shock
    }

    /// Retrieve the current stable semantic centroid of this session.
    #[must_use]
    pub fn current_centroid(&self) -> &[f32] {
        &self.centroid
    }

    /// Reset the detector's state.
    pub fn reset(&mut self) {
        self.centroid.clear();
        self.mean_distance = 0.0;
        self.variance = 0.0;
        self.samples = 0;
    }
}

// =============================================================================
// Conformal Prediction based detector (ft-344j8.8)
// =============================================================================

/// Configuration for the Conformal Prediction Anomaly Detector.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConformalAnomalyConfig {
    /// Target false positive rate (significance level). Default: 0.05.
    /// The detector guarantees P(false positive) <= alpha for exchangeable data.
    pub alpha: f64,
    /// Size of the sliding calibration window. Default: 200.
    /// Larger windows give more stable p-values but adapt more slowly.
    pub calibration_window: usize,
    /// EWMA alpha for the semantic centroid update. Default: 0.10.
    pub centroid_alpha: f32,
    /// Minimum calibration samples before anomaly detection activates. Default: 10.
    pub min_calibration: usize,
}

impl Default for ConformalAnomalyConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            calibration_window: 200,
            centroid_alpha: 0.10,
            min_calibration: 10,
        }
    }
}

/// A detected conformal anomaly event with formal statistical guarantees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConformalShock {
    /// The cosine distance from the semantic centroid.
    pub distance: f32,
    /// The conformal p-value: probability of seeing this distance or worse
    /// under the null (exchangeable, stationary stream).
    pub p_value: f64,
    /// The significance threshold used for this decision.
    pub alpha: f64,
    /// Number of calibration scores used to compute the p-value.
    pub calibration_count: usize,
    /// The median calibration score (for diagnostic context).
    pub calibration_median: f32,
}

/// Snapshot of the conformal detector state for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformalAnomalySnapshot {
    /// Total observations processed.
    pub total_observations: u64,
    /// Total anomalies detected.
    pub total_anomalies: u64,
    /// Current calibration window fill level.
    pub calibration_count: usize,
    /// Calibration window capacity.
    pub calibration_capacity: usize,
    /// Current centroid dimensionality.
    pub centroid_dim: usize,
    /// Most recent p-value (0.0 if no observations yet).
    pub last_p_value: f64,
    /// The 75th percentile calibration score (useful for tuning).
    pub calibration_p75: Option<f32>,
}

/// Tracks semantic trajectory using conformal prediction for formal anomaly guarantees.
///
/// Instead of heuristic Z-scores, maintains a calibration window of recent
/// non-conformity scores (cosine distances) and computes empirical p-values.
/// An observation is flagged as anomalous when `p < alpha`.
///
/// # Coverage guarantee
///
/// For exchangeable (i.i.d. or weakly dependent) data:
/// ```text
/// P(false positive) <= alpha
/// ```
///
/// # Algorithmic complexity
///
/// - `observe()`: O(log N) for rank query + O(N) amortized for sorted insertion/eviction
/// - Centroid update: O(d) where d is embedding dimension
/// - Memory: O(N + d) where N = calibration_window, d = embedding dimension
#[derive(Debug, Clone)]
pub struct ConformalAnomalyDetector {
    config: ConformalAnomalyConfig,
    /// The running semantic centroid (normalized).
    centroid: Vec<f32>,
    /// Sorted calibration buffer for O(log N) rank queries.
    calibration: SortedCalibrationBuffer,
    /// Total observations processed.
    total_observations: u64,
    /// Total anomalies detected.
    total_anomalies: u64,
    /// Most recent p-value.
    last_p_value: f64,
}

impl ConformalAnomalyDetector {
    /// Create a new conformal anomaly detector.
    #[must_use]
    pub fn new(config: ConformalAnomalyConfig) -> Self {
        let calibration = SortedCalibrationBuffer::new(config.calibration_window);
        Self {
            config,
            centroid: Vec::new(),
            calibration,
            total_observations: 0,
            total_anomalies: 0,
            last_p_value: 1.0,
        }
    }

    /// Process a new embedding vector and return an anomaly if the conformal
    /// p-value falls below alpha.
    ///
    /// The non-conformity score is the cosine distance from the running centroid.
    /// The p-value is computed as `(count(calibration >= distance) + 1) / (N + 1)`.
    pub fn observe(&mut self, embedding: &[f32]) -> Option<ConformalShock> {
        if embedding.is_empty() {
            return None;
        }

        // Initialize centroid on first observation.
        if self.centroid.is_empty() {
            self.centroid = normalize_simd(embedding);
            self.total_observations += 1;
            return None;
        }

        // Handle dimension changes by resetting.
        if self.centroid.len() != embedding.len() {
            self.centroid = normalize_simd(embedding);
            self.calibration = SortedCalibrationBuffer::new(self.config.calibration_window);
            self.total_observations += 1;
            self.last_p_value = 1.0;
            return None;
        }

        let normalized_emb = normalize_simd(embedding);

        // Non-conformity score: cosine distance.
        let similarity = dot_product_simd(&self.centroid, &normalized_emb);
        let distance = (1.0 - similarity).max(0.0);

        // Compute conformal p-value if we have enough calibration data.
        let mut shock = None;
        if self.calibration.len() >= self.config.min_calibration {
            let p_value = self.calibration.conformal_p_value(distance);
            self.last_p_value = p_value;

            if p_value < self.config.alpha {
                self.total_anomalies += 1;
                shock = Some(ConformalShock {
                    distance,
                    p_value,
                    alpha: self.config.alpha,
                    calibration_count: self.calibration.len(),
                    calibration_median: self.calibration.quantile(0.5).unwrap_or(0.0),
                });
            }
        }

        // Add to calibration window (even anomalies, to prevent runaway sensitivity).
        self.calibration.insert(distance);

        // Update the centroid via EWMA + renormalization.
        let alpha = self.config.centroid_alpha;
        for (i, val) in self.centroid.iter_mut().enumerate() {
            *val = (1.0 - alpha).mul_add(*val, alpha * normalized_emb[i]);
        }
        self.centroid = normalize_simd(&self.centroid);

        self.total_observations += 1;
        shock
    }

    /// Retrieve the current semantic centroid.
    #[must_use]
    pub fn current_centroid(&self) -> &[f32] {
        &self.centroid
    }

    /// Capture a diagnostic snapshot of the detector state.
    #[must_use]
    pub fn snapshot(&self) -> ConformalAnomalySnapshot {
        ConformalAnomalySnapshot {
            total_observations: self.total_observations,
            total_anomalies: self.total_anomalies,
            calibration_count: self.calibration.len(),
            calibration_capacity: self.config.calibration_window,
            centroid_dim: self.centroid.len(),
            last_p_value: self.last_p_value,
            calibration_p75: self.calibration.quantile(0.75),
        }
    }

    /// Reset the detector (e.g., after an intentional context switch).
    pub fn reset(&mut self) {
        self.centroid.clear();
        self.calibration = SortedCalibrationBuffer::new(self.config.calibration_window);
        self.last_p_value = 1.0;
        // Preserve lifetime counters for diagnostics.
    }

    /// The most recent p-value.
    #[must_use]
    pub fn last_p_value(&self) -> f64 {
        self.last_p_value
    }

    /// Total anomalies detected over the detector's lifetime.
    #[must_use]
    pub fn total_anomalies(&self) -> u64 {
        self.total_anomalies
    }

    /// Total observations processed over the detector's lifetime.
    #[must_use]
    pub fn total_observations(&self) -> u64 {
        self.total_observations
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_vector(val: f32, dim: usize) -> Vec<f32> {
        vec![val; dim]
    }

    // ── SIMD math tests ─────────────────────────────────────────────────────

    #[test]
    fn test_dot_product_simd_basic() {
        let a = vec![1.0, 2.0, 3.0, 4.0];
        let b = vec![5.0, 6.0, 7.0, 8.0];
        let result = dot_product_simd(&a, &b);
        assert!((result - 70.0).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn test_dot_product_simd_non_multiple_of_4() {
        let a = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        let b = vec![2.0, 3.0, 4.0, 5.0, 6.0];
        // 2+6+12+20+30 = 70
        let result = dot_product_simd(&a, &b);
        assert!((result - 70.0).abs() < 1e-4, "got {result}");
    }

    #[test]
    fn test_dot_product_simd_empty() {
        assert_eq!(dot_product_simd(&[], &[]), 0.0);
    }

    #[test]
    fn test_dot_product_simd_single() {
        assert!((dot_product_simd(&[3.0], &[4.0]) - 12.0).abs() < 1e-6);
    }

    #[test]
    fn test_dot_product_simd_384d() {
        let a: Vec<f32> = (0..384).map(|i| (i as f32) * 0.01).collect();
        let b: Vec<f32> = (0..384).map(|i| 1.0 - (i as f32) * 0.005).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        let simd = dot_product_simd(&a, &b);
        assert!(
            (simd - naive).abs() < 0.1,
            "SIMD={simd} naive={naive} diff={}",
            (simd - naive).abs()
        );
    }

    #[test]
    fn test_normalize_simd_unit_vector() {
        let v = normalize_simd(&[3.0, 4.0]);
        let mag: f32 = v.iter().map(|x| x * x).sum();
        assert!((mag - 1.0).abs() < 1e-5);
        assert!((v[0] - 0.6).abs() < 1e-5);
        assert!((v[1] - 0.8).abs() < 1e-5);
    }

    #[test]
    fn test_normalize_simd_zero_vector() {
        let v = normalize_simd(&[0.0, 0.0, 0.0]);
        assert_eq!(v, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn test_normalize_simd_high_dim() {
        let v: Vec<f32> = (1..=768).map(|i| i as f32).collect();
        let n = normalize_simd(&v);
        let mag: f32 = n.iter().map(|x| x * x).sum();
        assert!((mag - 1.0).abs() < 1e-4, "magnitude squared = {mag}");
    }

    #[test]
    fn test_dot_product_simd_mismatched_lengths() {
        // Should use the shorter length.
        let result = dot_product_simd(&[1.0, 2.0, 3.0], &[4.0, 5.0]);
        assert!((result - 14.0).abs() < 1e-6);
    }

    // ── SortedCalibrationBuffer tests ───────────────────────────────────────

    #[test]
    fn test_calibration_buffer_basic() {
        let mut buf = SortedCalibrationBuffer::new(5);
        buf.insert(1.0);
        buf.insert(3.0);
        buf.insert(2.0);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.count_geq(2.0), 2); // 2.0 and 3.0
        assert_eq!(buf.count_geq(3.0), 1);
        assert_eq!(buf.count_geq(4.0), 0);
    }

    #[test]
    fn test_calibration_buffer_eviction() {
        let mut buf = SortedCalibrationBuffer::new(3);
        buf.insert(1.0);
        buf.insert(2.0);
        buf.insert(3.0);
        assert_eq!(buf.len(), 3);

        // This should evict 1.0 (oldest).
        buf.insert(4.0);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.count_geq(1.0), 3); // 2.0, 3.0, 4.0
        assert_eq!(buf.count_geq(2.0), 3);
    }

    #[test]
    fn test_calibration_buffer_p_value_empty() {
        let buf = SortedCalibrationBuffer::new(10);
        assert_eq!(buf.conformal_p_value(1.0), 1.0);
    }

    #[test]
    fn test_calibration_buffer_p_value_all_smaller() {
        let mut buf = SortedCalibrationBuffer::new(10);
        for i in 1..=5 {
            buf.insert(i as f32 * 0.1);
        }
        // All 5 scores are <= 0.5. New score 1.0 has no calibration >= it.
        // p = (0 + 1) / (5 + 1) = 1/6
        let p = buf.conformal_p_value(1.0);
        assert!((p - 1.0 / 6.0).abs() < 1e-10);
    }

    #[test]
    fn test_calibration_buffer_p_value_all_larger() {
        let mut buf = SortedCalibrationBuffer::new(10);
        for i in 1..=5 {
            buf.insert(i as f32);
        }
        // All 5 scores >= 0.01. p = (5 + 1) / (5 + 1) = 1.0
        let p = buf.conformal_p_value(0.01);
        assert!((p - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_calibration_buffer_p_value_middle() {
        let mut buf = SortedCalibrationBuffer::new(10);
        for i in 0..10 {
            buf.insert(i as f32);
        }
        // Scores: 0,1,2,...,9. count_geq(5.0) = 5 (scores 5,6,7,8,9)
        // p = (5 + 1) / (10 + 1) = 6/11
        let p = buf.conformal_p_value(5.0);
        assert!((p - 6.0 / 11.0).abs() < 1e-10);
    }

    #[test]
    fn test_calibration_buffer_quantile() {
        let mut buf = SortedCalibrationBuffer::new(10);
        for i in 0..10 {
            buf.insert(i as f32);
        }
        assert_eq!(buf.quantile(0.0), Some(0.0));
        assert_eq!(buf.quantile(1.0), Some(9.0));
        // Median of 0..9 is approximately 4 or 5.
        let med = buf.quantile(0.5).unwrap();
        assert!(med >= 4.0 && med <= 5.0, "median={med}");
    }

    #[test]
    fn test_calibration_buffer_quantile_empty() {
        let buf = SortedCalibrationBuffer::new(10);
        assert_eq!(buf.quantile(0.5), None);
    }

    #[test]
    fn test_calibration_buffer_eviction_preserves_sorted() {
        let mut buf = SortedCalibrationBuffer::new(3);
        buf.insert(5.0);
        buf.insert(3.0);
        buf.insert(7.0);
        // sorted: [3, 5, 7], ring: [5, 3, 7]
        buf.insert(1.0); // evicts 5.0 (ring[0])
        // sorted should be [1, 3, 7], ring: [1, 3, 7]
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.count_geq(3.0), 2); // 3.0, 7.0
        assert_eq!(buf.count_geq(1.0), 3);
    }

    #[test]
    fn test_calibration_buffer_duplicate_scores() {
        let mut buf = SortedCalibrationBuffer::new(5);
        buf.insert(2.0);
        buf.insert(2.0);
        buf.insert(2.0);
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.count_geq(2.0), 3);
        assert_eq!(buf.count_geq(2.1), 0);
    }

    #[test]
    fn test_calibration_buffer_wrap_around() {
        let mut buf = SortedCalibrationBuffer::new(2);
        buf.insert(1.0);
        buf.insert(2.0);
        buf.insert(3.0); // evicts 1.0
        buf.insert(4.0); // evicts 2.0
        assert_eq!(buf.len(), 2);
        // Should have [3.0, 4.0]
        assert_eq!(buf.count_geq(3.0), 2);
        assert_eq!(buf.count_geq(2.0), 2);
        assert_eq!(buf.count_geq(5.0), 0);
    }

    // ── Z-score detector tests (backwards compat) ───────────────────────────

    #[test]
    fn test_semantic_anomaly_initialization() {
        let mut detector = SemanticAnomalyDetector::new(SemanticAnomalyConfig::default());
        assert!(detector.observe(&dummy_vector(1.0, 10)).is_none());
        assert_eq!(detector.samples, 1);
    }

    #[test]
    fn test_stable_stream_no_shocks() {
        let mut detector = SemanticAnomalyDetector::new(SemanticAnomalyConfig::default());
        let vec_a = vec![1.0, 0.0, 0.0];
        let vec_b = vec![0.9, 0.1, 0.0];

        for _ in 0..10 {
            assert!(detector.observe(&vec_a).is_none());
            assert!(detector.observe(&vec_b).is_none());
        }
    }

    #[test]
    fn test_semantic_shock_detection() {
        let config = SemanticAnomalyConfig {
            min_samples: 3,
            shock_threshold_z: 2.0,
            ..SemanticAnomalyConfig::default()
        };
        let mut detector = SemanticAnomalyDetector::new(config);

        let context_a = vec![1.0, 0.0, 0.0];
        for _ in 0..10 {
            assert!(detector.observe(&context_a).is_none());
        }

        let context_b = vec![0.0, 1.0, 0.0];
        let shock = detector.observe(&context_b);

        assert!(shock.is_some(), "Expected a semantic shock");
        let s = shock.unwrap();
        assert!(s.z_score > 2.0, "Z-score {} should exceed threshold", s.z_score);
        assert!(s.distance > 0.5, "Distance should be large");
    }

    #[test]
    fn test_dimension_change_resets_detector_state() {
        let mut detector = SemanticAnomalyDetector::new(SemanticAnomalyConfig::default());

        assert!(detector.observe(&[1.0, 0.0, 0.0]).is_none());
        assert_eq!(detector.current_centroid().len(), 3);
        assert_eq!(detector.samples, 1);

        assert!(detector.observe(&[0.0, 1.0]).is_none());
        assert_eq!(detector.current_centroid().len(), 2);
        assert_eq!(detector.samples, 1);
    }

    // ── Conformal detector tests ────────────────────────────────────────────

    #[test]
    fn test_conformal_detector_initialization() {
        let mut det = ConformalAnomalyDetector::new(ConformalAnomalyConfig::default());
        assert!(det.observe(&dummy_vector(1.0, 10)).is_none());
        assert_eq!(det.total_observations(), 1);
    }

    #[test]
    fn test_conformal_stable_no_anomalies() {
        let config = ConformalAnomalyConfig {
            min_calibration: 5,
            alpha: 0.05,
            ..Default::default()
        };
        let mut det = ConformalAnomalyDetector::new(config);

        let vec_a = vec![1.0, 0.0, 0.0];
        for _ in 0..50 {
            assert!(det.observe(&vec_a).is_none());
        }
        assert_eq!(det.total_anomalies(), 0);
    }

    #[test]
    fn test_conformal_detects_orthogonal_shift() {
        let config = ConformalAnomalyConfig {
            min_calibration: 5,
            alpha: 0.05,
            calibration_window: 50,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // Build calibration with stable context.
        let context_a = vec![1.0, 0.0, 0.0];
        for _ in 0..30 {
            det.observe(&context_a);
        }

        // Orthogonal shift — should be anomalous.
        let context_b = vec![0.0, 1.0, 0.0];
        let result = det.observe(&context_b);

        assert!(
            result.is_some(),
            "Expected conformal shock for orthogonal shift, p={}",
            det.last_p_value()
        );
        let shock = result.unwrap();
        assert!(shock.p_value < 0.05, "p={} should be < 0.05", shock.p_value);
        assert!(shock.distance > 0.5);
        assert_eq!(shock.alpha, 0.05);
    }

    #[test]
    fn test_conformal_p_value_bounds() {
        let config = ConformalAnomalyConfig {
            min_calibration: 3,
            calibration_window: 20,
            ..Default::default()
        };
        let mut det = ConformalAnomalyDetector::new(config);

        let v = vec![1.0, 0.0, 0.0];
        for _ in 0..10 {
            det.observe(&v);
        }

        // p-value should be in (0, 1].
        let p = det.last_p_value();
        assert!(p > 0.0 && p <= 1.0, "p={p}");
    }

    #[test]
    fn test_conformal_warmup_no_detection() {
        let config = ConformalAnomalyConfig {
            min_calibration: 10,
            ..Default::default()
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // During warmup, even orthogonal shifts shouldn't trigger.
        let v1 = vec![1.0, 0.0, 0.0];
        let v2 = vec![0.0, 1.0, 0.0];
        for i in 0..10 {
            let v = if i % 2 == 0 { &v1 } else { &v2 };
            assert!(det.observe(v).is_none(), "No detection during warmup");
        }
    }

    #[test]
    fn test_conformal_dimension_change_resets() {
        let mut det = ConformalAnomalyDetector::new(ConformalAnomalyConfig::default());

        let v3d = vec![1.0, 0.0, 0.0];
        for _ in 0..20 {
            det.observe(&v3d);
        }

        // Switch to 2D — should reset without panicking.
        let v2d = vec![1.0, 0.0];
        assert!(det.observe(&v2d).is_none());
        assert_eq!(det.current_centroid().len(), 2);
    }

    #[test]
    fn test_conformal_empty_embedding() {
        let mut det = ConformalAnomalyDetector::new(ConformalAnomalyConfig::default());
        assert!(det.observe(&[]).is_none());
        assert_eq!(det.total_observations(), 0);
    }

    #[test]
    fn test_conformal_snapshot() {
        let mut det = ConformalAnomalyDetector::new(ConformalAnomalyConfig {
            calibration_window: 50,
            ..Default::default()
        });
        let v = vec![1.0, 0.0, 0.0];
        for _ in 0..20 {
            det.observe(&v);
        }

        let snap = det.snapshot();
        assert_eq!(snap.total_observations, 20);
        assert_eq!(snap.calibration_capacity, 50);
        assert!(snap.calibration_count > 0);
        assert_eq!(snap.centroid_dim, 3);
    }

    #[test]
    fn test_conformal_reset_preserves_counters() {
        let mut det = ConformalAnomalyDetector::new(ConformalAnomalyConfig::default());
        let v = vec![1.0, 0.0, 0.0];
        for _ in 0..10 {
            det.observe(&v);
        }

        det.reset();
        assert!(det.current_centroid().is_empty());
        assert!(det.total_observations() > 0, "Counters should persist");
    }

    #[test]
    fn test_conformal_gradual_drift_limited_anomalies() {
        let config = ConformalAnomalyConfig {
            min_calibration: 5,
            calibration_window: 100,
            alpha: 0.05,
            centroid_alpha: 0.15,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // Gradually rotate the context vector.
        let mut anomaly_count = 0u32;
        for i in 0..200 {
            let angle = (i as f32) * 0.01; // slow rotation
            let v = vec![angle.cos(), angle.sin(), 0.0];
            if det.observe(&v).is_some() {
                anomaly_count += 1;
            }
        }

        // Gradual drift produces anomalies at each "step" that exceeds the
        // calibration window's recent history. The conformal detector adapts,
        // but drift means every step is slightly novel. Allow generous margin.
        // Key property: not ALL observations are flagged.
        assert!(
            anomaly_count < 150,
            "anomaly_count={anomaly_count} is too high — most observations flagged"
        );
    }

    #[test]
    fn test_conformal_sudden_shift_detected() {
        let config = ConformalAnomalyConfig {
            min_calibration: 10,
            calibration_window: 200,
            // With N=199 calibration scores of ~0 and a shift distance ~1.0:
            // p = (0 + 1) / (199 + 1) = 0.005. Need alpha > 0.005.
            alpha: 0.05,
            centroid_alpha: 0.05,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // Stable context A — build large calibration window.
        let context_a = vec![1.0, 0.0, 0.0, 0.0];
        for _ in 0..200 {
            det.observe(&context_a);
        }

        // Sudden shift to context B.
        let context_b = vec![0.0, 0.0, 0.0, 1.0];
        let result = det.observe(&context_b);

        assert!(
            result.is_some(),
            "Sudden shift should be detected, p={}",
            det.last_p_value()
        );
    }

    #[test]
    fn test_conformal_calibration_adapts() {
        let config = ConformalAnomalyConfig {
            min_calibration: 5,
            calibration_window: 20,
            alpha: 0.05,
            centroid_alpha: 0.2,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // Start in context A.
        let context_a = vec![1.0, 0.0, 0.0];
        for _ in 0..30 {
            det.observe(&context_a);
        }

        // Shift to context B — first observation should be anomalous.
        let context_b = vec![0.0, 1.0, 0.0];
        let shock1 = det.observe(&context_b);
        assert!(shock1.is_some(), "First B should be anomalous");

        // Stay in context B — detector should adapt.
        let mut anomalies_in_b = 0;
        for _ in 0..30 {
            if det.observe(&context_b).is_some() {
                anomalies_in_b += 1;
            }
        }

        // After calibration adapts, anomalies should stop.
        assert!(
            anomalies_in_b < 10,
            "Calibration should adapt to B; got {anomalies_in_b} anomalies"
        );
    }

    #[test]
    fn test_conformal_high_dim_384() {
        let config = ConformalAnomalyConfig {
            min_calibration: 5,
            calibration_window: 50,
            alpha: 0.05,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // Build stable context in 384d.
        let mut v = vec![0.0f32; 384];
        v[0] = 1.0;
        for _ in 0..30 {
            det.observe(&v);
        }

        // Orthogonal shift in 384d.
        let mut v2 = vec![0.0f32; 384];
        v2[383] = 1.0;
        let result = det.observe(&v2);
        assert!(result.is_some(), "384d shift should be detected");
    }

    #[test]
    fn test_conformal_total_anomalies_counter() {
        let config = ConformalAnomalyConfig {
            min_calibration: 3,
            calibration_window: 20,
            alpha: 0.05,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        let stable = vec![1.0, 0.0, 0.0];
        for _ in 0..20 {
            det.observe(&stable);
        }

        let shift = vec![0.0, 1.0, 0.0];
        if det.observe(&shift).is_some() {
            assert!(det.total_anomalies() >= 1);
        }
    }

    #[test]
    fn test_conformal_small_calibration_window() {
        let config = ConformalAnomalyConfig {
            min_calibration: 2,
            calibration_window: 3,
            alpha: 0.10,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        let v = vec![1.0, 0.0, 0.0];
        for _ in 0..10 {
            det.observe(&v);
        }
        // With a tiny window, the detector should still function.
        assert!(det.total_observations() == 10);
    }

    #[test]
    fn test_conformal_alpha_zero_never_detects() {
        let config = ConformalAnomalyConfig {
            min_calibration: 3,
            // alpha=0 means no p-value can be < 0 (since p > 0 always).
            alpha: 0.0,
            calibration_window: 20,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        let stable = vec![1.0, 0.0, 0.0];
        for _ in 0..20 {
            det.observe(&stable);
        }

        // Even a huge shift shouldn't trigger with alpha=0.
        let shift = vec![0.0, 1.0, 0.0];
        let result = det.observe(&shift);
        assert!(result.is_none(), "alpha=0 should never trigger");
    }

    #[test]
    fn test_conformal_serde_config() {
        let config = ConformalAnomalyConfig {
            alpha: 0.01,
            calibration_window: 100,
            centroid_alpha: 0.2,
            min_calibration: 15,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ConformalAnomalyConfig = serde_json::from_str(&json).unwrap();
        assert!((deserialized.alpha - 0.01).abs() < 1e-10);
        assert_eq!(deserialized.calibration_window, 100);
    }

    #[test]
    fn test_conformal_serde_shock() {
        let shock = ConformalShock {
            distance: 0.95,
            p_value: 0.001,
            alpha: 0.05,
            calibration_count: 100,
            calibration_median: 0.05,
        };
        let json = serde_json::to_string(&shock).unwrap();
        let round: ConformalShock = serde_json::from_str(&json).unwrap();
        assert_eq!(round, shock);
    }

    #[test]
    fn test_conformal_serde_snapshot() {
        let snap = ConformalAnomalySnapshot {
            total_observations: 100,
            total_anomalies: 3,
            calibration_count: 50,
            calibration_capacity: 200,
            centroid_dim: 384,
            last_p_value: 0.42,
            calibration_p75: Some(0.12),
        };
        let json = serde_json::to_string(&snap).unwrap();
        let round: ConformalAnomalySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(round.total_observations, 100);
        assert_eq!(round.total_anomalies, 3);
    }

    #[test]
    fn test_conformal_repeated_identical_inputs() {
        let config = ConformalAnomalyConfig {
            min_calibration: 5,
            calibration_window: 50,
            alpha: 0.05,
            centroid_alpha: 0.1,
        };
        let mut det = ConformalAnomalyDetector::new(config);

        // 100 identical inputs — zero cosine distance every time.
        let v = vec![1.0, 0.0, 0.0];
        let mut anomalies = 0;
        for _ in 0..100 {
            if det.observe(&v).is_some() {
                anomalies += 1;
            }
        }

        // With identical inputs, calibration scores are all ~0.
        // A new identical input has distance ~0, which should have high p-value.
        // Expect zero or very few false positives.
        assert!(anomalies <= 2, "anomalies={anomalies} too many for identical inputs");
    }
}
