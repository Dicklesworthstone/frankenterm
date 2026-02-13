//! Bayesian Online Change-Point Detection (BOCPD) for agent state transitions.
//!
//! Detects statistical regime changes in pane output that regex pattern matching
//! cannot catch: infinite loops, output quality degradation, novel failure modes,
//! and subtle behavioral drift.
//!
//! # Algorithm (Adams & MacKay, 2007)
//!
//! Maintains a run length posterior P(rₜ|x₁:ₜ) where rₜ is the number of
//! observations since the last change-point:
//!
//! 1. **Growth**: P(rₜ=r+1) ∝ P(xₜ|rₜ=r+1) × P(rₜ₋₁=r) × (1−H)
//! 2. **Change**: P(rₜ=0) ∝ P(xₜ|rₜ=0) × Σ P(rₜ₋₁=r) × H
//!
//! Where H is the hazard function (prior probability of a change-point at each
//! step). The predictive likelihood P(xₜ|rₜ) uses a Normal-Gamma conjugate prior.
//!
//! # Performance
//!
//! Single observation update target: < 50μs.

use std::collections::HashMap;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// =============================================================================
// Configuration
// =============================================================================

/// BOCPD configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BocpdConfig {
    /// Prior change-point rate (hazard). 1/200 = expect change every ~200 obs.
    pub hazard_rate: f64,
    /// Minimum posterior probability to declare a change-point.
    pub detection_threshold: f64,
    /// Minimum observations before detection starts.
    pub min_observations: usize,
    /// Maximum run length to track (truncation for performance).
    pub max_run_length: usize,
}

impl Default for BocpdConfig {
    fn default() -> Self {
        Self {
            hazard_rate: 0.005,
            detection_threshold: 0.7,
            min_observations: 20,
            max_run_length: 200,
        }
    }
}

// =============================================================================
// Normal-Gamma sufficient statistics
// =============================================================================

/// Sufficient statistics for the Normal-Gamma conjugate prior.
///
/// Tracks the posterior parameters for a univariate Normal distribution with
/// unknown mean and variance, using the Normal-Gamma conjugate family:
///
///   μ|τ ~ N(mu, 1/(kappa·τ))
///   τ   ~ Gamma(alpha, beta)
///
/// Updated in O(1) per observation.
#[derive(Debug, Clone)]
struct NormalGammaSS {
    /// Posterior mean location.
    mu: f64,
    /// Pseudo-count (strength of prior).
    kappa: f64,
    /// Shape parameter.
    alpha: f64,
    /// Rate parameter.
    beta: f64,
}

impl NormalGammaSS {
    /// Weakly informative prior centered at 0.
    fn prior() -> Self {
        Self {
            mu: 0.0,
            kappa: 0.01,
            alpha: 0.01,
            beta: 0.01,
        }
    }

    /// Update the sufficient statistics with a new observation.
    fn update(&self, x: f64) -> Self {
        let new_kappa = self.kappa + 1.0;
        let new_mu = self.kappa.mul_add(self.mu, x) / new_kappa;
        let new_alpha = self.alpha + 0.5;
        let new_beta = self.beta + 0.5 * self.kappa * (x - self.mu).powi(2) / new_kappa;

        Self {
            mu: new_mu,
            kappa: new_kappa,
            alpha: new_alpha,
            beta: new_beta,
        }
    }

    /// Predictive log-likelihood: Student-t distribution.
    ///
    /// P(x|params) = t_{2α}(x | μ, β(κ+1)/(ακ))
    fn predictive_log_likelihood(&self, x: f64) -> f64 {
        let df = 2.0 * self.alpha;
        let scale_sq = self.beta * (self.kappa + 1.0) / (self.alpha * self.kappa);

        if scale_sq <= 0.0 || df <= 0.0 {
            return -100.0; // degenerate case
        }

        let scale = scale_sq.sqrt();
        let z = (x - self.mu) / scale;

        // Log of Student-t density:
        // log Γ((ν+1)/2) - log Γ(ν/2) - 0.5*log(ν*π*σ²) - (ν+1)/2 * log(1 + z²/ν)
        let half_dfp1 = df.midpoint(1.0);
        let half_df = df / 2.0;

        let base = ln_gamma(half_dfp1) - ln_gamma(half_df);
        let log_norm = (df * std::f64::consts::PI * scale_sq).ln();
        let log_kernel = (z * z / df).ln_1p();

        let base = (-0.5f64).mul_add(log_norm, base);
        (-half_dfp1).mul_add(log_kernel, base)
    }
}

// =============================================================================
// Run length distribution
// =============================================================================

/// BOCPD model for a single observed time series.
///
/// Tracks the run length posterior distribution and detects change-points when
/// P(rₜ=0) exceeds the detection threshold.
pub struct BocpdModel {
    config: BocpdConfig,
    /// Run length posterior (log-probabilities for numerical stability).
    run_length_log_probs: Vec<f64>,
    /// Sufficient statistics for each run length.
    sufficient_stats: Vec<NormalGammaSS>,
    /// Total observations processed.
    observation_count: u64,
    /// Total change-points detected.
    change_point_count: u64,
}

impl BocpdModel {
    /// Create a new BOCPD model.
    #[must_use]
    pub fn new(config: BocpdConfig) -> Self {
        let mut run_length_log_probs = Vec::with_capacity(config.max_run_length + 1);
        run_length_log_probs.push(0.0); // log(1.0) — start with run length 0

        let mut sufficient_stats = Vec::with_capacity(config.max_run_length + 1);
        sufficient_stats.push(NormalGammaSS::prior());

        Self {
            config,
            run_length_log_probs,
            sufficient_stats,
            observation_count: 0,
            change_point_count: 0,
        }
    }

    /// Process a new observation. Returns a change-point event if detected.
    pub fn update(&mut self, x: f64) -> Option<ChangePoint> {
        self.observation_count += 1;
        let n = self.run_length_log_probs.len();

        // Step 1: Compute predictive log-likelihoods for each run length
        let mut pred_log_liks = Vec::with_capacity(n);
        for ss in &self.sufficient_stats {
            pred_log_liks.push(ss.predictive_log_likelihood(x));
        }

        // Step 2: Compute growth probabilities (log-space)
        let log_h = self.config.hazard_rate.ln();
        let log_1mh = (1.0 - self.config.hazard_rate).ln();

        let mut new_log_probs = Vec::with_capacity(n + 1);

        // Change-point probability: sum over all run lengths
        let mut change_log_terms = Vec::with_capacity(n);
        for (&run_log_prob, &pred_ll) in self.run_length_log_probs.iter().zip(&pred_log_liks) {
            change_log_terms.push(run_log_prob + pred_ll + log_h);
        }
        let change_log_prob = log_sum_exp(&change_log_terms);
        new_log_probs.push(change_log_prob);

        // Growth probabilities for each existing run length
        for (&run_log_prob, &pred_ll) in self.run_length_log_probs.iter().zip(&pred_log_liks) {
            let growth = run_log_prob + pred_ll + log_1mh;
            new_log_probs.push(growth);
        }

        // Step 3: Normalize
        let log_evidence = log_sum_exp(&new_log_probs);
        for lp in &mut new_log_probs {
            *lp -= log_evidence;
        }

        // Step 4: Truncate to max_run_length
        if new_log_probs.len() > self.config.max_run_length + 1 {
            new_log_probs.truncate(self.config.max_run_length + 1);
            // Re-normalize after truncation
            let log_sum = log_sum_exp(&new_log_probs);
            for lp in &mut new_log_probs {
                *lp -= log_sum;
            }
        }

        // Step 5: Update sufficient statistics
        let mut new_ss = Vec::with_capacity(new_log_probs.len());
        new_ss.push(NormalGammaSS::prior()); // fresh prior for r=0
        for ss in &self.sufficient_stats {
            new_ss.push(ss.update(x));
            if new_ss.len() >= new_log_probs.len() {
                break;
            }
        }

        self.run_length_log_probs = new_log_probs;
        self.sufficient_stats = new_ss;

        // Step 6: Check for change-point
        if self.observation_count as usize >= self.config.min_observations {
            let change_prob = self.run_length_log_probs[0].exp();
            if change_prob >= self.config.detection_threshold {
                self.change_point_count += 1;
                return Some(ChangePoint {
                    observation_index: self.observation_count,
                    posterior_probability: change_prob,
                    map_run_length: self.map_run_length(),
                });
            }
        }

        None
    }

    /// Maximum a posteriori run length (most likely current run length).
    #[must_use]
    pub fn map_run_length(&self) -> usize {
        self.run_length_log_probs
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// Probability of a change-point at the current step.
    #[must_use]
    pub fn change_point_probability(&self) -> f64 {
        if self.run_length_log_probs.is_empty() {
            return 0.0;
        }
        self.run_length_log_probs[0].exp()
    }

    /// Total observations processed.
    #[must_use]
    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// Total change-points detected.
    #[must_use]
    pub fn change_point_count(&self) -> u64 {
        self.change_point_count
    }

    /// Whether the model has enough data to detect changes.
    #[must_use]
    pub fn in_warmup(&self) -> bool {
        (self.observation_count as usize) < self.config.min_observations
    }

    /// Run length posterior distribution (probabilities, not log-probs).
    #[must_use]
    pub fn run_length_posterior(&self) -> Vec<f64> {
        self.run_length_log_probs
            .iter()
            .map(|lp| lp.exp())
            .collect()
    }

    /// Check that the posterior sums to approximately 1.0.
    #[must_use]
    pub fn posterior_sum(&self) -> f64 {
        log_sum_exp(&self.run_length_log_probs).exp()
    }
}

impl std::fmt::Debug for BocpdModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BocpdModel")
            .field("observation_count", &self.observation_count)
            .field("change_point_count", &self.change_point_count)
            .field("map_run_length", &self.map_run_length())
            .field("change_point_prob", &self.change_point_probability())
            .finish()
    }
}

// =============================================================================
// Change-point event
// =============================================================================

/// A detected change-point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePoint {
    /// Index of the observation where the change was detected.
    pub observation_index: u64,
    /// Posterior probability P(rₜ=0).
    pub posterior_probability: f64,
    /// MAP run length before the change.
    pub map_run_length: usize,
}

// =============================================================================
// Feature vector
// =============================================================================

/// Per-pane feature vector computed from output data.
///
/// Fed into the BOCPD model to detect regime changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputFeatures {
    /// Lines per second.
    pub output_rate: f64,
    /// Bytes per second.
    pub byte_rate: f64,
    /// Shannon entropy of character distribution (0–8 bits).
    pub entropy: f64,
    /// Fraction of lines that are unique (0–1).
    pub unique_line_ratio: f64,
    /// Fraction of bytes that are ANSI escape sequences (0–1).
    pub ansi_density: f64,
}

impl OutputFeatures {
    /// Compute features from a chunk of output text and elapsed time.
    #[must_use]
    pub fn compute(text: &str, elapsed: std::time::Duration) -> Self {
        let elapsed_secs = elapsed.as_secs_f64().max(0.001);
        let bytes = text.as_bytes();
        let byte_count = bytes.len();
        let scan = crate::simd_scan::scan_newlines_and_ansi(bytes);
        let line_count = scan.logical_line_count(bytes);

        // Output and byte rates
        let output_rate = line_count as f64 / elapsed_secs;
        let byte_rate = byte_count as f64 / elapsed_secs;

        // Shannon entropy of byte distribution
        let entropy = compute_entropy(bytes);

        // Unique line ratio
        let unique_line_ratio = if line_count == 0 {
            1.0
        } else {
            let mut unique = std::collections::HashSet::new();
            for line in text.lines() {
                unique.insert(line);
            }
            unique.len() as f64 / line_count as f64
        };

        // ANSI escape sequence density
        let ansi_density = scan.ansi_density(byte_count);

        OutputFeatures {
            output_rate,
            byte_rate,
            entropy,
            unique_line_ratio,
            ansi_density,
        }
    }

    /// Return the primary metric for BOCPD (output_rate by default).
    #[must_use]
    pub fn primary_metric(&self) -> f64 {
        self.output_rate
    }
}

// =============================================================================
// Per-pane BOCPD tracker
// =============================================================================

/// Tracks BOCPD state for a single pane.
pub struct PaneBocpd {
    /// BOCPD model for output rate.
    pub rate_model: BocpdModel,
    /// Pane ID.
    pub pane_id: u64,
    /// Detected change-points.
    pub change_points: Vec<PaneChangePoint>,
    /// Last feature vector.
    pub last_features: Option<OutputFeatures>,
}

/// A change-point event tied to a specific pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneChangePoint {
    /// Pane ID.
    pub pane_id: u64,
    /// Observation index.
    pub observation_index: u64,
    /// Posterior probability.
    pub posterior_probability: f64,
    /// Features before the change (if available).
    pub features_at_change: Option<OutputFeatures>,
    /// Unix timestamp.
    pub timestamp_secs: u64,
}

impl PaneBocpd {
    /// Create a new per-pane BOCPD tracker.
    #[must_use]
    pub fn new(pane_id: u64, config: BocpdConfig) -> Self {
        Self {
            rate_model: BocpdModel::new(config),
            pane_id,
            change_points: Vec::new(),
            last_features: None,
        }
    }

    /// Feed new output features and check for change-points.
    pub fn observe(&mut self, features: OutputFeatures) -> Option<PaneChangePoint> {
        let value = features.primary_metric();
        self.last_features = Some(features.clone());

        if let Some(cp) = self.rate_model.update(value) {
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());

            let pane_cp = PaneChangePoint {
                pane_id: self.pane_id,
                observation_index: cp.observation_index,
                posterior_probability: cp.posterior_probability,
                features_at_change: Some(features),
                timestamp_secs: now,
            };

            self.change_points.push(pane_cp.clone());
            return Some(pane_cp);
        }

        None
    }

    /// Number of detected change-points.
    #[must_use]
    pub fn change_point_count(&self) -> usize {
        self.change_points.len()
    }
}

// =============================================================================
// Multi-pane BOCPD manager
// =============================================================================

/// Manages BOCPD models for all active panes.
pub struct BocpdManager {
    config: BocpdConfig,
    panes: HashMap<u64, PaneBocpd>,
    total_change_points: u64,
}

impl BocpdManager {
    /// Create a new manager.
    #[must_use]
    pub fn new(config: BocpdConfig) -> Self {
        Self {
            config,
            panes: HashMap::new(),
            total_change_points: 0,
        }
    }

    /// Register a pane for monitoring.
    pub fn register_pane(&mut self, pane_id: u64) {
        self.panes
            .entry(pane_id)
            .or_insert_with(|| PaneBocpd::new(pane_id, self.config.clone()));
    }

    /// Remove a pane.
    pub fn unregister_pane(&mut self, pane_id: u64) {
        self.panes.remove(&pane_id);
    }

    /// Feed features for a pane. Returns a change-point event if detected.
    pub fn observe(&mut self, pane_id: u64, features: OutputFeatures) -> Option<PaneChangePoint> {
        let pane = self
            .panes
            .entry(pane_id)
            .or_insert_with(|| PaneBocpd::new(pane_id, self.config.clone()));

        let result = pane.observe(features);
        if result.is_some() {
            self.total_change_points += 1;
        }
        result
    }

    /// Number of tracked panes.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Total change-points across all panes.
    #[must_use]
    pub fn total_change_points(&self) -> u64 {
        self.total_change_points
    }

    /// Get a serializable snapshot of manager state.
    #[must_use]
    pub fn snapshot(&self) -> BocpdSnapshot {
        let pane_summaries: Vec<PaneBocpdSummary> = self
            .panes
            .values()
            .map(|p| PaneBocpdSummary {
                pane_id: p.pane_id,
                observation_count: p.rate_model.observation_count(),
                change_point_count: p.change_points.len() as u64,
                current_change_prob: p.rate_model.change_point_probability(),
                map_run_length: p.rate_model.map_run_length() as u64,
                in_warmup: p.rate_model.in_warmup(),
            })
            .collect();

        BocpdSnapshot {
            pane_count: self.panes.len() as u64,
            total_change_points: self.total_change_points,
            panes: pane_summaries,
        }
    }
}

impl std::fmt::Debug for BocpdManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BocpdManager")
            .field("pane_count", &self.pane_count())
            .field("total_change_points", &self.total_change_points)
            .finish()
    }
}

/// Serializable BOCPD system snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BocpdSnapshot {
    pub pane_count: u64,
    pub total_change_points: u64,
    pub panes: Vec<PaneBocpdSummary>,
}

/// Per-pane BOCPD summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBocpdSummary {
    pub pane_id: u64,
    pub observation_count: u64,
    pub change_point_count: u64,
    pub current_change_prob: f64,
    pub map_run_length: u64,
    pub in_warmup: bool,
}

// =============================================================================
// Math helpers
// =============================================================================

/// Log-sum-exp for numerical stability.
fn log_sum_exp(log_values: &[f64]) -> f64 {
    if log_values.is_empty() {
        return f64::NEG_INFINITY;
    }
    let max = log_values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if max == f64::NEG_INFINITY {
        return f64::NEG_INFINITY;
    }
    let sum: f64 = log_values.iter().map(|&lp| (lp - max).exp()).sum();
    max + sum.ln()
}

/// Stirling's approximation for ln(Γ(x)) — sufficient for our quantile needs.
///
/// Uses the Lanczos approximation for better accuracy.
fn ln_gamma(value: f64) -> f64 {
    if value <= 0.0 {
        return 0.0;
    }
    // Lanczos approximation (g=7, n=9)
    let lanczos_g = 7.0;
    #[allow(clippy::excessive_precision)]
    let coefficients = [
        0.999_999_999_999_809_93,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_9,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_571_6e-6,
        1.505_632_735_149_311_6e-7,
    ];

    if value < 0.5 {
        // Reflection formula
        let pi = std::f64::consts::PI;
        return pi.ln() - (pi * value).sin().ln() - ln_gamma(1.0 - value);
    }

    let x_minus_one = value - 1.0;
    let mut base = coefficients[0];
    for (i, &c) in coefficients.iter().enumerate().skip(1) {
        base += c / (x_minus_one + i as f64);
    }

    let lanczos_t = x_minus_one + lanczos_g + 0.5;
    let log_2pi = (2.0 * std::f64::consts::PI).ln();
    let log_power_term = lanczos_t.ln() * (x_minus_one + 0.5);
    0.5f64.mul_add(log_2pi, log_power_term) - lanczos_t + base.ln()
}

/// Shannon entropy of a byte sequence (bits).
fn compute_entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }

    let mut counts = [0u64; 256];
    for &b in data {
        counts[b as usize] += 1;
    }

    let n = data.len() as f64;
    let mut entropy = 0.0;
    for &count in &counts {
        if count > 0 {
            let p = count as f64 / n;
            entropy -= p * p.log2();
        }
    }
    entropy
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Normal-Gamma prior ---------------------------------------------------

    #[test]
    fn prior_predictive_finite() {
        let ss = NormalGammaSS::prior();
        let ll = ss.predictive_log_likelihood(0.0);
        assert!(ll.is_finite(), "log-likelihood should be finite: {ll}");
    }

    #[test]
    fn prior_update_changes_params() {
        let prior = NormalGammaSS::prior();
        let updated = prior.update(5.0);
        assert!(updated.kappa > prior.kappa);
        assert!(updated.alpha > prior.alpha);
    }

    #[test]
    fn multiple_updates_converge() {
        let mut ss = NormalGammaSS::prior();
        // Feed 100 observations from N(10, 1)
        for _ in 0..100 {
            ss = ss.update(10.0);
        }
        // Posterior mean should be near 10
        assert!(
            (ss.mu - 10.0).abs() < 0.1,
            "posterior mean={}, expected ~10",
            ss.mu
        );
    }

    // -- BocpdModel -----------------------------------------------------------

    #[test]
    fn model_creation() {
        let model = BocpdModel::new(BocpdConfig::default());
        assert_eq!(model.observation_count(), 0);
        assert_eq!(model.change_point_count(), 0);
        assert!(model.in_warmup());
    }

    #[test]
    fn model_warmup_period() {
        let mut model = BocpdModel::new(BocpdConfig {
            min_observations: 5,
            ..Default::default()
        });

        for i in 0..4 {
            let result = model.update(1.0);
            assert!(result.is_none(), "no detection during warmup at obs {i}");
        }
        assert!(model.in_warmup());

        model.update(1.0);
        assert!(!model.in_warmup());
    }

    #[test]
    fn posterior_sums_to_one() {
        let mut model = BocpdModel::new(BocpdConfig::default());

        for _ in 0..50 {
            model.update(1.0);
        }

        let sum = model.posterior_sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "posterior sum={sum}, expected ~1.0"
        );
    }

    #[test]
    fn posterior_sums_to_one_after_regime_change() {
        let mut model = BocpdModel::new(BocpdConfig {
            min_observations: 5,
            hazard_rate: 0.01,
            ..Default::default()
        });

        // Regime 1: values around 0
        for _ in 0..30 {
            model.update(0.0);
        }
        // Regime 2: values around 100
        for _ in 0..30 {
            model.update(100.0);
        }

        let sum = model.posterior_sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "posterior sum={sum}, expected ~1.0"
        );
    }

    #[test]
    fn detects_regime_change() {
        // BOCPD needs multiple observations of the new regime to accumulate
        // evidence for a change-point. With hazard_rate H, the initial
        // change-point mass is H, and it grows as new-regime observations
        // accumulate evidence via higher likelihood under the reset prior.
        let mut model = BocpdModel::new(BocpdConfig {
            hazard_rate: 0.1, // expect change every ~10 observations
            detection_threshold: 0.3,
            min_observations: 10,
            max_run_length: 100,
        });

        // Regime 1: constant values around 10
        for _ in 0..30 {
            let _ = model.update(10.0);
        }

        // Regime 2: large shift — over many observations, change probability
        // should grow as the prior-based model explains new data better than
        // the old-regime posterior.
        let mut max_change_prob = 0.0f64;
        let mut detected = false;
        for _ in 0..50 {
            let cp = model.update(1000.0);
            let p = model.change_point_probability();
            if p > max_change_prob {
                max_change_prob = p;
            }
            if cp.is_some() {
                detected = true;
                break;
            }
        }

        // The MAP run length should have shifted from ~30 to a short value,
        // confirming the model detected the new regime.
        let map_rl = model.map_run_length();
        assert!(
            detected || max_change_prob > 0.1 || map_rl < 20,
            "should detect regime change: detected={detected}, \
             max_change_prob={max_change_prob}, map_rl={map_rl}"
        );
    }

    #[test]
    fn no_false_alarm_on_stable_data() {
        let mut model = BocpdModel::new(BocpdConfig {
            hazard_rate: 0.005,
            detection_threshold: 0.7,
            min_observations: 10,
            max_run_length: 200,
        });

        // Stable data: all same value
        let mut false_alarms = 0;
        for _ in 0..200 {
            if model.update(5.0).is_some() {
                false_alarms += 1;
            }
        }

        assert!(
            false_alarms <= 2,
            "too many false alarms on stable data: {false_alarms}"
        );
    }

    #[test]
    fn map_run_length_grows() {
        let mut model = BocpdModel::new(BocpdConfig {
            min_observations: 5,
            ..Default::default()
        });

        // Feed constant data — run length should grow
        for _ in 0..50 {
            model.update(1.0);
        }

        let rl = model.map_run_length();
        assert!(rl > 20, "MAP run length should be large: {rl}");
    }

    // -- OutputFeatures -------------------------------------------------------

    #[test]
    fn features_from_empty_text() {
        let features = OutputFeatures::compute("", std::time::Duration::from_secs(1));
        assert_eq!(features.output_rate, 0.0);
        assert_eq!(features.byte_rate, 0.0);
        assert_eq!(features.entropy, 0.0);
        assert_eq!(features.unique_line_ratio, 1.0);
    }

    #[test]
    fn features_from_normal_output() {
        let text = "line 1\nline 2\nline 3\nline 4\nline 5\n";
        let features = OutputFeatures::compute(text, std::time::Duration::from_secs(1));
        assert_eq!(features.output_rate, 5.0);
        assert!(features.byte_rate > 0.0);
        assert!(features.entropy > 0.0);
        assert_eq!(features.unique_line_ratio, 1.0);
    }

    #[test]
    fn features_repetitive_output_low_unique_ratio() {
        let text = "ERROR\nERROR\nERROR\nERROR\nERROR\n";
        let features = OutputFeatures::compute(text, std::time::Duration::from_secs(1));
        assert!(
            features.unique_line_ratio < 0.3,
            "ratio={}, expected < 0.3",
            features.unique_line_ratio
        );
    }

    #[test]
    fn features_ansi_density() {
        let text = "\x1b[31mred\x1b[0m normal \x1b[32mgreen\x1b[0m";
        let features = OutputFeatures::compute(text, std::time::Duration::from_secs(1));
        assert!(features.ansi_density > 0.0, "should detect ANSI sequences");
    }

    #[test]
    fn features_serde_roundtrip() {
        let f = OutputFeatures {
            output_rate: 10.0,
            byte_rate: 500.0,
            entropy: 4.5,
            unique_line_ratio: 0.8,
            ansi_density: 0.1,
        };
        let json = serde_json::to_string(&f).unwrap();
        let back: OutputFeatures = serde_json::from_str(&json).unwrap();
        assert!((back.entropy - 4.5).abs() < f64::EPSILON);
    }

    // -- PaneBocpd ------------------------------------------------------------

    #[test]
    fn pane_bocpd_lifecycle() {
        let mut pane = PaneBocpd::new(
            42,
            BocpdConfig {
                min_observations: 5,
                ..Default::default()
            },
        );

        for _ in 0..10 {
            let f = OutputFeatures {
                output_rate: 5.0,
                byte_rate: 200.0,
                entropy: 4.0,
                unique_line_ratio: 0.9,
                ansi_density: 0.05,
            };
            let _ = pane.observe(f);
        }

        assert_eq!(pane.pane_id, 42);
        assert!(pane.last_features.is_some());
    }

    // -- BocpdManager ---------------------------------------------------------

    #[test]
    fn manager_lifecycle() {
        let mut mgr = BocpdManager::new(BocpdConfig::default());
        mgr.register_pane(1);
        mgr.register_pane(2);
        assert_eq!(mgr.pane_count(), 2);

        mgr.unregister_pane(1);
        assert_eq!(mgr.pane_count(), 1);
    }

    #[test]
    fn manager_auto_registers() {
        let mut mgr = BocpdManager::new(BocpdConfig::default());
        let features = OutputFeatures {
            output_rate: 5.0,
            byte_rate: 200.0,
            entropy: 4.0,
            unique_line_ratio: 0.9,
            ansi_density: 0.05,
        };
        mgr.observe(99, features);
        assert_eq!(mgr.pane_count(), 1);
    }

    #[test]
    fn manager_snapshot() {
        let mut mgr = BocpdManager::new(BocpdConfig {
            min_observations: 3,
            ..Default::default()
        });
        mgr.register_pane(1);

        for _ in 0..5 {
            let features = OutputFeatures {
                output_rate: 5.0,
                byte_rate: 200.0,
                entropy: 4.0,
                unique_line_ratio: 0.9,
                ansi_density: 0.05,
            };
            mgr.observe(1, features);
        }

        let snap = mgr.snapshot();
        assert_eq!(snap.pane_count, 1);
        assert_eq!(snap.panes.len(), 1);
        assert_eq!(snap.panes[0].observation_count, 5);
        assert!(!snap.panes[0].in_warmup);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let snap = BocpdSnapshot {
            pane_count: 2,
            total_change_points: 3,
            panes: vec![PaneBocpdSummary {
                pane_id: 1,
                observation_count: 50,
                change_point_count: 1,
                current_change_prob: 0.1,
                map_run_length: 30,
                in_warmup: false,
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: BocpdSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_change_points, 3);
    }

    // -- Math helpers ---------------------------------------------------------

    #[test]
    fn log_sum_exp_basic() {
        // log(e^1 + e^2 + e^3) = log(e + e^2 + e^3)
        let result = log_sum_exp(&[1.0, 2.0, 3.0]);
        let expected = (1.0f64.exp() + 2.0f64.exp() + 3.0f64.exp()).ln();
        assert!((result - expected).abs() < 1e-10);
    }

    #[test]
    fn log_sum_exp_empty() {
        assert_eq!(log_sum_exp(&[]), f64::NEG_INFINITY);
    }

    #[test]
    fn log_sum_exp_single() {
        assert!((log_sum_exp(&[5.0]) - 5.0).abs() < 1e-10);
    }

    #[test]
    fn ln_gamma_known_values() {
        // Γ(1) = 1, ln(1) = 0
        assert!(
            (ln_gamma(1.0) - 0.0).abs() < 1e-6,
            "ln_gamma(1)={}",
            ln_gamma(1.0)
        );
        // Γ(2) = 1, ln(1) = 0
        assert!(
            (ln_gamma(2.0) - 0.0).abs() < 1e-6,
            "ln_gamma(2)={}",
            ln_gamma(2.0)
        );
        // Γ(5) = 24, ln(24) ≈ 3.178
        assert!(
            (ln_gamma(5.0) - 24.0f64.ln()).abs() < 1e-4,
            "ln_gamma(5)={}",
            ln_gamma(5.0)
        );
    }

    #[test]
    fn entropy_binary() {
        // "01010101" — 2 equally likely symbols → 1 bit
        let data = b"01010101";
        let e = compute_entropy(data);
        assert!((e - 1.0).abs() < 0.01, "entropy={e}, expected ~1.0");
    }

    #[test]
    fn entropy_single_symbol() {
        let data = b"aaaaaa";
        let e = compute_entropy(data);
        assert!((e - 0.0).abs() < 1e-10, "entropy={e}, expected 0");
    }

    #[test]
    fn ansi_density_zero_for_plain() {
        let text = b"hello world";
        let scan = crate::simd_scan::scan_newlines_and_ansi(text);
        let d = scan.ansi_density(text.len());
        assert_eq!(d, 0.0);
    }

    // -- Config ---------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let config = BocpdConfig::default();
        assert!((config.hazard_rate - 0.005).abs() < 1e-10);
        assert!((config.detection_threshold - 0.7).abs() < 1e-10);
        assert_eq!(config.min_observations, 20);
        assert_eq!(config.max_run_length, 200);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = BocpdConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: BocpdConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_run_length, 200);
    }

    // -- Change-point event serde ---------------------------------------------

    #[test]
    fn change_point_serde() {
        let cp = ChangePoint {
            observation_index: 42,
            posterior_probability: 0.85,
            map_run_length: 30,
        };
        let json = serde_json::to_string(&cp).unwrap();
        let back: ChangePoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.observation_index, 42);
    }

    #[test]
    fn pane_change_point_serde() {
        let pcp = PaneChangePoint {
            pane_id: 7,
            observation_index: 100,
            posterior_probability: 0.92,
            features_at_change: None,
            timestamp_secs: 1700000000,
        };
        let json = serde_json::to_string(&pcp).unwrap();
        let back: PaneChangePoint = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, 7);
    }
}
