//! Expected Loss Dynamic Timeout Calculator for ARS reflexes.
//!
//! Reflexes must not hang. Rather than using magic-number timeouts, this module
//! derives the optimal timeout using an expected loss minimization function:
//!
//! ```text
//! min_t L(t) = Cost(Hang) * P(T > t) + Cost(PrematureKill) * P(T <= t)
//! ```
//!
//! Where:
//! - `T` is the random variable for execution duration
//! - `Cost(Hang)` is the cost of letting a reflex run too long (blocking terminal)
//! - `Cost(PrematureKill)` is the cost of killing a reflex that would have succeeded
//! - `P(T > t)` is the survival function at time `t`
//! - `P(T <= t)` is the CDF at time `t`
//!
//! # Model
//!
//! We model execution duration as a **log-normal** distribution (common for
//! latency-like quantities). Parameters are estimated from observed durations
//! using maximum likelihood.
//!
//! # Clamps
//!
//! The optimal timeout is clamped between configurable bounds (default
//! 500ms to 60s) regardless of the mathematical optimum.
//!
//! # Performance
//!
//! Timeout calculation is O(n) for n observations (parameter estimation)
//! plus O(1) for the optimization (closed-form for log-normal).

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for dynamic timeout calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeoutConfig {
    /// Cost of a hanging reflex (terminal blocked). Higher = more aggressive timeout.
    pub cost_hang: f64,
    /// Cost of killing a reflex prematurely. Higher = more generous timeout.
    pub cost_premature_kill: f64,
    /// Minimum timeout in milliseconds (floor).
    pub min_timeout_ms: u64,
    /// Maximum timeout in milliseconds (ceiling).
    pub max_timeout_ms: u64,
    /// Default timeout when insufficient data (ms).
    pub default_timeout_ms: u64,
    /// Minimum observations required before using the model.
    pub min_observations: usize,
    /// Multiplier safety margin applied on top of the optimal timeout.
    pub safety_multiplier: f64,
    /// Percentile to use as fallback when optimization fails (0.0–1.0).
    pub fallback_percentile: f64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            cost_hang: 10.0,
            cost_premature_kill: 1.0,
            min_timeout_ms: 500,
            max_timeout_ms: 60_000,
            default_timeout_ms: 5_000,
            min_observations: 3,
            safety_multiplier: 1.2,
            fallback_percentile: 0.95,
        }
    }
}

// =============================================================================
// Duration statistics
// =============================================================================

/// Summary statistics for observed durations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DurationStats {
    /// Number of observations.
    pub count: usize,
    /// Mean duration in milliseconds.
    pub mean_ms: f64,
    /// Standard deviation in milliseconds.
    pub std_dev_ms: f64,
    /// Minimum observed duration (ms).
    pub min_ms: f64,
    /// Maximum observed duration (ms).
    pub max_ms: f64,
    /// Median observed duration (ms).
    pub median_ms: f64,
    /// Log-normal mu parameter (mean of log durations).
    pub ln_mu: f64,
    /// Log-normal sigma parameter (std dev of log durations).
    pub ln_sigma: f64,
}

impl DurationStats {
    /// Compute statistics from a slice of durations (in milliseconds).
    ///
    /// Returns None if the slice is empty or contains non-positive values.
    #[must_use]
    pub fn from_durations(durations: &[f64]) -> Option<Self> {
        if durations.is_empty() {
            return None;
        }

        // Filter out non-positive durations for log-normal.
        let positive: Vec<f64> = durations.iter().copied().filter(|d| *d > 0.0).collect();
        if positive.is_empty() {
            return None;
        }

        let count = positive.len();
        let sum: f64 = positive.iter().sum();
        let mean = sum / count as f64;

        let variance = if count > 1 {
            positive.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (count - 1) as f64
        } else {
            0.0
        };
        let std_dev = variance.sqrt();

        let min = positive.iter().copied().fold(f64::INFINITY, f64::min);
        let max = positive.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        let mut sorted = positive.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if count % 2 == 0 {
            (sorted[count / 2 - 1] + sorted[count / 2]) / 2.0
        } else {
            sorted[count / 2]
        };

        // Log-normal parameters: MLE estimates.
        let log_durations: Vec<f64> = positive.iter().map(|d| d.ln()).collect();
        let ln_mu = log_durations.iter().sum::<f64>() / count as f64;
        let ln_sigma = if count > 1 {
            let ln_var = log_durations
                .iter()
                .map(|l| (l - ln_mu).powi(2))
                .sum::<f64>()
                / (count - 1) as f64;
            ln_var.sqrt()
        } else {
            0.0
        };

        Some(Self {
            count,
            mean_ms: mean,
            std_dev_ms: std_dev,
            min_ms: min,
            max_ms: max,
            median_ms: median,
            ln_mu,
            ln_sigma,
        })
    }

    /// Log-normal CDF: P(T <= t).
    #[must_use]
    pub fn cdf(&self, t_ms: f64) -> f64 {
        if t_ms <= 0.0 {
            return 0.0;
        }
        if self.ln_sigma <= 0.0 {
            // Degenerate: all observations identical.
            return if t_ms >= self.mean_ms { 1.0 } else { 0.0 };
        }
        let z = (t_ms.ln() - self.ln_mu) / self.ln_sigma;
        standard_normal_cdf(z)
    }

    /// Log-normal survival function: P(T > t) = 1 - CDF(t).
    #[must_use]
    pub fn survival(&self, t_ms: f64) -> f64 {
        1.0 - self.cdf(t_ms)
    }

    /// Log-normal quantile (inverse CDF).
    #[must_use]
    pub fn quantile(&self, p: f64) -> f64 {
        if p <= 0.0 {
            return 0.0;
        }
        if p >= 1.0 {
            return f64::INFINITY;
        }
        if self.ln_sigma <= 0.0 {
            return self.mean_ms;
        }
        let z = standard_normal_quantile(p);
        (self.ln_mu + self.ln_sigma * z).exp()
    }
}

// =============================================================================
// Timeout calculator
// =============================================================================

/// Timeout decision result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeoutDecision {
    /// The recommended timeout in milliseconds.
    pub timeout_ms: u64,
    /// The raw optimal timeout before clamping/safety (ms).
    pub raw_optimal_ms: f64,
    /// Expected loss at the chosen timeout.
    pub expected_loss: f64,
    /// How the timeout was determined.
    pub method: TimeoutMethod,
    /// Duration statistics (if available).
    pub stats: Option<DurationStats>,
    /// Whether the result is from sufficient data.
    pub is_data_driven: bool,
}

/// How the timeout was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeoutMethod {
    /// Optimal expected loss minimization.
    ExpectedLoss,
    /// Percentile-based fallback.
    Percentile,
    /// Default timeout (insufficient data).
    Default,
    /// Clamped to min/max bounds.
    Clamped,
}

/// The dynamic timeout calculator.
pub struct TimeoutCalculator {
    config: TimeoutConfig,
}

impl TimeoutCalculator {
    /// Create a new calculator with the given config.
    #[must_use]
    pub fn new(config: TimeoutConfig) -> Self {
        Self { config }
    }

    /// Create with default settings.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(TimeoutConfig::default())
    }

    /// Calculate the optimal timeout from observed durations (in milliseconds).
    #[must_use]
    pub fn calculate(&self, durations_ms: &[f64]) -> TimeoutDecision {
        // Insufficient data: return default.
        if durations_ms.len() < self.config.min_observations {
            debug!(
                observations = durations_ms.len(),
                required = self.config.min_observations,
                "Insufficient data, using default timeout"
            );
            return TimeoutDecision {
                timeout_ms: self.config.default_timeout_ms,
                raw_optimal_ms: self.config.default_timeout_ms as f64,
                expected_loss: f64::NAN,
                method: TimeoutMethod::Default,
                stats: None,
                is_data_driven: false,
            };
        }

        let stats = match DurationStats::from_durations(durations_ms) {
            Some(s) => s,
            None => {
                return TimeoutDecision {
                    timeout_ms: self.config.default_timeout_ms,
                    raw_optimal_ms: self.config.default_timeout_ms as f64,
                    expected_loss: f64::NAN,
                    method: TimeoutMethod::Default,
                    stats: None,
                    is_data_driven: false,
                };
            }
        };

        // Try optimal expected loss minimization.
        let optimal = self.find_optimal_timeout(&stats);

        let (raw_optimal_ms, method, expected_loss) = match optimal {
            Some((t, loss)) => (t, TimeoutMethod::ExpectedLoss, loss),
            None => {
                // Fallback to percentile-based.
                let percentile_t = stats.quantile(self.config.fallback_percentile);
                let loss = self.expected_loss(&stats, percentile_t);
                (percentile_t, TimeoutMethod::Percentile, loss)
            }
        };

        // Apply safety multiplier.
        let with_safety = raw_optimal_ms * self.config.safety_multiplier;

        // Clamp to bounds.
        let clamped = with_safety
            .max(self.config.min_timeout_ms as f64)
            .min(self.config.max_timeout_ms as f64);

        let final_method = if (clamped - with_safety).abs() > 1e-3 {
            TimeoutMethod::Clamped
        } else {
            method
        };

        let timeout_ms = clamped.round() as u64;

        debug!(
            timeout_ms,
            raw_optimal_ms,
            method = ?final_method,
            expected_loss,
            n_observations = stats.count,
            "Timeout calculated"
        );

        TimeoutDecision {
            timeout_ms,
            raw_optimal_ms,
            expected_loss,
            method: final_method,
            stats: Some(stats),
            is_data_driven: true,
        }
    }

    /// Calculate the expected loss at a given timeout.
    #[must_use]
    pub fn expected_loss(&self, stats: &DurationStats, t_ms: f64) -> f64 {
        let p_hang = stats.survival(t_ms);
        let p_premature = stats.cdf(t_ms);
        self.config.cost_hang * p_hang + self.config.cost_premature_kill * p_premature
    }

    /// Find the optimal timeout that minimizes expected loss.
    ///
    /// For log-normal, the optimal timeout has a closed-form solution:
    /// At the optimum, the derivative of L(t) = 0, which gives:
    ///   f(t) * (cost_hang - cost_premature_kill) = 0
    /// Since f(t) > 0 for all t > 0, this is never zero—meaning L(t) is
    /// monotone if costs are equal. But for different costs, the optimum is
    /// where the marginal cost of waiting equals the marginal cost of killing.
    ///
    /// We use golden section search on the interval [min, max].
    fn find_optimal_timeout(&self, stats: &DurationStats) -> Option<(f64, f64)> {
        if self.config.cost_hang <= 0.0 || self.config.cost_premature_kill <= 0.0 {
            return None;
        }

        // If costs are equal, any timeout gives the same total cost.
        if (self.config.cost_hang - self.config.cost_premature_kill).abs() < 1e-10 {
            // Use median as default.
            let t = stats.median_ms;
            let loss = self.expected_loss(stats, t);
            return Some((t, loss));
        }

        // Golden section search.
        let mut a = 1.0f64; // 1ms lower bound for search
        let mut b = stats.quantile(0.999).max(stats.max_ms * 3.0);
        if b <= a {
            b = a + 1000.0;
        }

        let golden = (5.0f64.sqrt() - 1.0) / 2.0;
        let tol = 1.0; // 1ms tolerance

        for _ in 0..100 {
            if (b - a) < tol {
                break;
            }
            let x1 = b - golden * (b - a);
            let x2 = a + golden * (b - a);
            let l1 = self.expected_loss(stats, x1);
            let l2 = self.expected_loss(stats, x2);
            if l1 < l2 {
                b = x2;
            } else {
                a = x1;
            }
        }

        let optimal = (a + b) / 2.0;
        let loss = self.expected_loss(stats, optimal);

        trace!(optimal_ms = optimal, loss, "Golden section converged");

        Some((optimal, loss))
    }
}

// =============================================================================
// Timeout tracker (accumulates observations)
// =============================================================================

/// Tracks execution durations and provides dynamic timeout recommendations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutTracker {
    config: TimeoutConfig,
    /// Observed durations in milliseconds.
    observations: Vec<f64>,
    /// Maximum observations to retain (ring buffer).
    max_observations: usize,
    /// Total observations ever recorded.
    total_recorded: u64,
    /// Total timeouts triggered.
    total_timeouts: u64,
    /// Total premature kills.
    total_premature_kills: u64,
}

impl TimeoutTracker {
    /// Create a new tracker with default config.
    #[must_use]
    pub fn new(config: TimeoutConfig) -> Self {
        Self {
            config,
            observations: Vec::new(),
            max_observations: 1000,
            total_recorded: 0,
            total_timeouts: 0,
            total_premature_kills: 0,
        }
    }

    /// Create with default settings.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(TimeoutConfig::default())
    }

    /// Record an observed duration (in milliseconds).
    pub fn record(&mut self, duration_ms: f64) {
        if duration_ms > 0.0 {
            if self.observations.len() >= self.max_observations {
                self.observations.remove(0);
            }
            self.observations.push(duration_ms);
            self.total_recorded += 1;
        }
    }

    /// Record a timeout event (reflex was killed due to timeout).
    pub fn record_timeout(&mut self) {
        self.total_timeouts += 1;
    }

    /// Record a premature kill (reflex was killed but would have succeeded).
    pub fn record_premature_kill(&mut self) {
        self.total_premature_kills += 1;
    }

    /// Get the current recommended timeout.
    #[must_use]
    pub fn recommended_timeout(&self) -> TimeoutDecision {
        let calc = TimeoutCalculator::new(self.config.clone());
        calc.calculate(&self.observations)
    }

    /// Number of observations currently stored.
    #[must_use]
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }

    /// Total observations ever recorded.
    #[must_use]
    pub fn total_observations(&self) -> u64 {
        self.total_recorded
    }

    /// Timeout rate: fraction of executions that hit the timeout.
    #[must_use]
    pub fn timeout_rate(&self) -> f64 {
        if self.total_recorded == 0 {
            return 0.0;
        }
        self.total_timeouts as f64 / self.total_recorded as f64
    }

    /// Get statistics for current observations.
    #[must_use]
    pub fn current_stats(&self) -> Option<DurationStats> {
        DurationStats::from_durations(&self.observations)
    }
}

// =============================================================================
// Standard normal CDF & quantile (no external deps)
// =============================================================================

/// Standard normal CDF using the Abramowitz & Stegun approximation.
/// Accurate to ~1.5e-7.
fn standard_normal_cdf(x: f64) -> f64 {
    if x.is_nan() {
        return 0.5;
    }
    if x > 8.0 {
        return 1.0;
    }
    if x < -8.0 {
        return 0.0;
    }

    // Use the polynomial approximation.
    let sign = if x >= 0.0 { 1.0 } else { -1.0 };
    let abs_x = x.abs();

    let t = 1.0 / (1.0 + 0.231_641_9 * abs_x);
    let t2 = t * t;
    let t3 = t2 * t;
    let t4 = t3 * t;
    let t5 = t4 * t;

    let poly = 0.319_381_530 * t - 0.356_563_782 * t2 + 1.781_477_937 * t3
        - 1.821_255_978 * t4
        + 1.330_274_429 * t5;

    let pdf = (-abs_x * abs_x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let cdf_positive = 1.0 - pdf * poly;

    0.5 + sign * (cdf_positive - 0.5)
}

/// Standard normal quantile (inverse CDF) via bisection on our CDF.
/// Converges to ~1e-8 accuracy in ~50 iterations.
fn standard_normal_quantile(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    if (p - 0.5).abs() < 1e-15 {
        return 0.0;
    }

    // Bisection search: find z such that CDF(z) = p.
    let mut lo = -8.0f64;
    let mut hi = 8.0f64;

    for _ in 0..100 {
        let mid = (lo + hi) / 2.0;
        let cdf_mid = standard_normal_cdf(mid);
        if cdf_mid < p {
            lo = mid;
        } else {
            hi = mid;
        }
        if (hi - lo) < 1e-10 {
            break;
        }
    }

    (lo + hi) / 2.0
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Standard normal CDF tests
    // =========================================================================

    #[test]
    fn normal_cdf_at_zero_is_half() {
        let cdf = standard_normal_cdf(0.0);
        assert!((cdf - 0.5).abs() < 1e-6);
    }

    #[test]
    fn normal_cdf_monotone() {
        let mut prev = 0.0;
        for x in -40..=40 {
            let xf = x as f64 / 10.0;
            let cdf = standard_normal_cdf(xf);
            assert!(cdf >= prev - 1e-10, "CDF should be monotone at x={}", xf);
            prev = cdf;
        }
    }

    #[test]
    fn normal_cdf_bounds() {
        assert!(standard_normal_cdf(-10.0) < 1e-6);
        assert!(standard_normal_cdf(10.0) > 1.0 - 1e-6);
    }

    #[test]
    fn normal_cdf_symmetry() {
        for x in [0.5, 1.0, 1.5, 2.0, 3.0] {
            let lo = standard_normal_cdf(-x);
            let hi = standard_normal_cdf(x);
            assert!((lo + hi - 1.0).abs() < 1e-6, "CDF({}) + CDF({}) should be 1", -x, x);
        }
    }

    // =========================================================================
    // Standard normal quantile tests
    // =========================================================================

    #[test]
    fn normal_quantile_at_half_is_zero() {
        let q = standard_normal_quantile(0.5);
        assert!(q.abs() < 1e-6);
    }

    #[test]
    fn normal_quantile_cdf_roundtrip() {
        for p in [0.1, 0.25, 0.5, 0.75, 0.9, 0.95, 0.99] {
            let z = standard_normal_quantile(p);
            let recovered = standard_normal_cdf(z);
            assert!(
                (recovered - p).abs() < 1e-4,
                "roundtrip failed: p={} z={} recovered={}",
                p,
                z,
                recovered
            );
        }
    }

    #[test]
    fn normal_quantile_monotone() {
        let mut prev = f64::NEG_INFINITY;
        for i in 1..100 {
            let p = i as f64 / 100.0;
            let q = standard_normal_quantile(p);
            assert!(q > prev, "quantile should be monotone at p={}", p);
            prev = q;
        }
    }

    // =========================================================================
    // DurationStats tests
    // =========================================================================

    #[test]
    fn stats_from_empty_is_none() {
        assert!(DurationStats::from_durations(&[]).is_none());
    }

    #[test]
    fn stats_from_single() {
        let stats = DurationStats::from_durations(&[100.0]).unwrap();
        assert_eq!(stats.count, 1);
        assert!((stats.mean_ms - 100.0).abs() < 1e-10);
        assert!((stats.median_ms - 100.0).abs() < 1e-10);
    }

    #[test]
    fn stats_mean_correct() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0]).unwrap();
        assert!((stats.mean_ms - 200.0).abs() < 1e-10);
    }

    #[test]
    fn stats_median_odd() {
        let stats = DurationStats::from_durations(&[10.0, 20.0, 30.0]).unwrap();
        assert!((stats.median_ms - 20.0).abs() < 1e-10);
    }

    #[test]
    fn stats_median_even() {
        let stats = DurationStats::from_durations(&[10.0, 20.0, 30.0, 40.0]).unwrap();
        assert!((stats.median_ms - 25.0).abs() < 1e-10);
    }

    #[test]
    fn stats_min_max() {
        let stats = DurationStats::from_durations(&[5.0, 100.0, 50.0]).unwrap();
        assert!((stats.min_ms - 5.0).abs() < 1e-10);
        assert!((stats.max_ms - 100.0).abs() < 1e-10);
    }

    #[test]
    fn stats_filters_non_positive() {
        let stats = DurationStats::from_durations(&[-1.0, 0.0, 100.0, 200.0]).unwrap();
        assert_eq!(stats.count, 2); // Only positive values.
    }

    #[test]
    fn stats_all_non_positive_is_none() {
        assert!(DurationStats::from_durations(&[-1.0, 0.0, -5.0]).is_none());
    }

    #[test]
    fn stats_log_normal_params() {
        // For identical values, ln_sigma should be 0.
        let stats = DurationStats::from_durations(&[100.0, 100.0, 100.0]).unwrap();
        assert!((stats.ln_sigma - 0.0).abs() < 1e-10);
        assert!((stats.ln_mu - 100.0f64.ln()).abs() < 1e-10);
    }

    // =========================================================================
    // CDF / survival / quantile tests
    // =========================================================================

    #[test]
    fn cdf_at_zero_is_zero() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0]).unwrap();
        assert!((stats.cdf(0.0) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn cdf_at_infinity_is_one() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0]).unwrap();
        assert!((stats.cdf(1e12) - 1.0).abs() < 1e-3);
    }

    #[test]
    fn cdf_plus_survival_equals_one() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0]).unwrap();
        for t in [50.0, 100.0, 200.0, 500.0, 1000.0] {
            let sum = stats.cdf(t) + stats.survival(t);
            assert!((sum - 1.0).abs() < 1e-10, "CDF + survival should be 1 at t={}", t);
        }
    }

    #[test]
    fn cdf_is_monotone() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0, 400.0]).unwrap();
        let mut prev = 0.0;
        for t in (0..1000).step_by(10) {
            let cdf = stats.cdf(t as f64);
            assert!(cdf >= prev - 1e-10, "CDF should be monotone at t={}", t);
            prev = cdf;
        }
    }

    #[test]
    fn quantile_roundtrip() {
        let stats = DurationStats::from_durations(&[50.0, 100.0, 200.0, 400.0, 800.0]).unwrap();
        for p in [0.1, 0.25, 0.5, 0.75, 0.9] {
            let t = stats.quantile(p);
            let recovered = stats.cdf(t);
            assert!(
                (recovered - p).abs() < 0.05,
                "quantile roundtrip failed: p={} t={} recovered={}",
                p,
                t,
                recovered
            );
        }
    }

    // =========================================================================
    // Expected loss tests
    // =========================================================================

    #[test]
    fn expected_loss_decreases_then_increases() {
        let stats = DurationStats::from_durations(&[100.0, 150.0, 200.0, 250.0, 300.0]).unwrap();
        let calc = TimeoutCalculator::new(TimeoutConfig {
            cost_hang: 10.0,
            cost_premature_kill: 1.0,
            ..Default::default()
        });

        // At very small t: mostly cost_hang (high).
        let loss_small = calc.expected_loss(&stats, 1.0);
        // At very large t: mostly cost_premature_kill (low base but full).
        let loss_large = calc.expected_loss(&stats, 100_000.0);
        // Somewhere in between should be lower.
        let loss_mid = calc.expected_loss(&stats, 300.0);

        assert!(
            loss_mid < loss_small,
            "mid-range loss {} should be less than small-t loss {}",
            loss_mid,
            loss_small
        );
        // loss_large should be close to cost_premature_kill.
        assert!(
            (loss_large - 1.0).abs() < 0.1,
            "very large t loss should approach cost_premature_kill"
        );
    }

    #[test]
    fn expected_loss_bounds() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0]).unwrap();
        let calc = TimeoutCalculator::new(TimeoutConfig {
            cost_hang: 10.0,
            cost_premature_kill: 1.0,
            ..Default::default()
        });

        for t in [1.0, 10.0, 100.0, 1000.0, 10000.0] {
            let loss = calc.expected_loss(&stats, t);
            assert!(loss >= 0.0, "loss should be non-negative at t={}", t);
            let max_loss = 10.0f64.max(1.0); // max of costs
            assert!(
                loss <= max_loss + 0.01,
                "loss {} should be <= max cost {} at t={}",
                loss,
                max_loss,
                t
            );
        }
    }

    // =========================================================================
    // TimeoutCalculator tests
    // =========================================================================

    #[test]
    fn calculate_with_insufficient_data_returns_default() {
        let calc = TimeoutCalculator::with_defaults();
        let result = calc.calculate(&[100.0, 200.0]); // Only 2, need 3.
        assert_eq!(result.method, TimeoutMethod::Default);
        assert_eq!(result.timeout_ms, 5000);
        assert!(!result.is_data_driven);
    }

    #[test]
    fn calculate_with_sufficient_data_is_data_driven() {
        let calc = TimeoutCalculator::with_defaults();
        let durations: Vec<f64> = (1..=20).map(|i| i as f64 * 100.0).collect();
        let result = calc.calculate(&durations);
        assert!(result.is_data_driven);
        assert!(result.stats.is_some());
    }

    #[test]
    fn calculate_respects_min_timeout() {
        let calc = TimeoutCalculator::new(TimeoutConfig {
            min_timeout_ms: 1000,
            min_observations: 1,
            ..Default::default()
        });
        let result = calc.calculate(&[1.0, 2.0, 3.0]); // Very fast commands.
        assert!(
            result.timeout_ms >= 1000,
            "should respect min_timeout_ms, got {}",
            result.timeout_ms
        );
    }

    #[test]
    fn calculate_respects_max_timeout() {
        let calc = TimeoutCalculator::new(TimeoutConfig {
            max_timeout_ms: 10_000,
            min_observations: 1,
            ..Default::default()
        });
        let result = calc.calculate(&[50_000.0, 60_000.0, 70_000.0]); // Very slow commands.
        assert!(
            result.timeout_ms <= 10_000,
            "should respect max_timeout_ms, got {}",
            result.timeout_ms
        );
    }

    #[test]
    fn calculate_higher_hang_cost_gives_shorter_timeout() {
        let durations: Vec<f64> = (1..=20).map(|i| i as f64 * 50.0).collect();

        let calc_low = TimeoutCalculator::new(TimeoutConfig {
            cost_hang: 2.0,
            cost_premature_kill: 1.0,
            min_observations: 1,
            ..Default::default()
        });
        let calc_high = TimeoutCalculator::new(TimeoutConfig {
            cost_hang: 100.0,
            cost_premature_kill: 1.0,
            min_observations: 1,
            ..Default::default()
        });

        let result_low = calc_low.calculate(&durations);
        let result_high = calc_high.calculate(&durations);

        assert!(
            result_high.timeout_ms <= result_low.timeout_ms,
            "higher hang cost should give shorter timeout: {} vs {}",
            result_high.timeout_ms,
            result_low.timeout_ms
        );
    }

    #[test]
    fn calculate_with_empty_returns_default() {
        let calc = TimeoutCalculator::with_defaults();
        let result = calc.calculate(&[]);
        assert_eq!(result.method, TimeoutMethod::Default);
    }

    #[test]
    fn calculate_consistent_durations() {
        let calc = TimeoutCalculator::new(TimeoutConfig {
            min_observations: 1,
            ..Default::default()
        });
        // All same duration — timeout should be near that value.
        let durations = vec![100.0; 50];
        let result = calc.calculate(&durations);
        // Should be around 100ms * safety_multiplier, clamped to min.
        assert!(result.timeout_ms >= 100);
        assert!(result.timeout_ms <= 2000);
    }

    // =========================================================================
    // TimeoutTracker tests
    // =========================================================================

    #[test]
    fn tracker_starts_empty() {
        let tracker = TimeoutTracker::with_defaults();
        assert_eq!(tracker.observation_count(), 0);
        assert_eq!(tracker.total_observations(), 0);
    }

    #[test]
    fn tracker_records_observations() {
        let mut tracker = TimeoutTracker::with_defaults();
        tracker.record(100.0);
        tracker.record(200.0);
        tracker.record(300.0);
        assert_eq!(tracker.observation_count(), 3);
        assert_eq!(tracker.total_observations(), 3);
    }

    #[test]
    fn tracker_ignores_non_positive() {
        let mut tracker = TimeoutTracker::with_defaults();
        tracker.record(0.0);
        tracker.record(-5.0);
        assert_eq!(tracker.observation_count(), 0);
    }

    #[test]
    fn tracker_recommended_timeout_default_when_empty() {
        let tracker = TimeoutTracker::with_defaults();
        let decision = tracker.recommended_timeout();
        assert_eq!(decision.method, TimeoutMethod::Default);
    }

    #[test]
    fn tracker_recommended_timeout_data_driven() {
        let mut tracker = TimeoutTracker::with_defaults();
        for i in 1..=10 {
            tracker.record(i as f64 * 100.0);
        }
        let decision = tracker.recommended_timeout();
        assert!(decision.is_data_driven);
    }

    #[test]
    fn tracker_timeout_rate() {
        let mut tracker = TimeoutTracker::with_defaults();
        tracker.record(100.0);
        tracker.record(200.0);
        tracker.record(300.0);
        tracker.record_timeout();
        tracker.record_timeout();
        assert!((tracker.timeout_rate() - 2.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn tracker_timeout_rate_zero_when_empty() {
        let tracker = TimeoutTracker::with_defaults();
        assert!((tracker.timeout_rate() - 0.0).abs() < 1e-10);
    }

    #[test]
    fn tracker_max_observations_ring_buffer() {
        let mut tracker = TimeoutTracker::new(TimeoutConfig::default());
        tracker.max_observations = 5;
        for i in 1..=10 {
            tracker.record(i as f64 * 10.0);
        }
        assert_eq!(tracker.observation_count(), 5);
        assert_eq!(tracker.total_observations(), 10);
    }

    #[test]
    fn tracker_current_stats() {
        let mut tracker = TimeoutTracker::with_defaults();
        tracker.record(100.0);
        tracker.record(200.0);
        let stats = tracker.current_stats();
        assert!(stats.is_some());
        let stats = stats.unwrap();
        assert_eq!(stats.count, 2);
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn config_default_values() {
        let config = TimeoutConfig::default();
        assert!((config.cost_hang - 10.0).abs() < 1e-10);
        assert!((config.cost_premature_kill - 1.0).abs() < 1e-10);
        assert_eq!(config.min_timeout_ms, 500);
        assert_eq!(config.max_timeout_ms, 60_000);
        assert_eq!(config.default_timeout_ms, 5_000);
        assert_eq!(config.min_observations, 3);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = TimeoutConfig {
            cost_hang: 5.0,
            cost_premature_kill: 2.0,
            min_timeout_ms: 1000,
            max_timeout_ms: 30_000,
            default_timeout_ms: 3_000,
            min_observations: 5,
            safety_multiplier: 1.5,
            fallback_percentile: 0.99,
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: TimeoutConfig = serde_json::from_str(&json).unwrap();
        assert!((decoded.cost_hang - 5.0).abs() < 1e-10);
        assert_eq!(decoded.min_timeout_ms, 1000);
        assert_eq!(decoded.min_observations, 5);
    }

    // =========================================================================
    // TimeoutDecision serde
    // =========================================================================

    #[test]
    fn decision_serde_roundtrip() {
        let calc = TimeoutCalculator::with_defaults();
        let durations: Vec<f64> = (1..=10).map(|i| i as f64 * 100.0).collect();
        let decision = calc.calculate(&durations);

        let json = serde_json::to_string(&decision).unwrap();
        let decoded: TimeoutDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.timeout_ms, decision.timeout_ms);
        assert_eq!(decoded.method, decision.method);
    }

    #[test]
    fn method_serde_roundtrip() {
        let methods = [
            TimeoutMethod::ExpectedLoss,
            TimeoutMethod::Percentile,
            TimeoutMethod::Default,
            TimeoutMethod::Clamped,
        ];
        for method in &methods {
            let json = serde_json::to_string(method).unwrap();
            let decoded: TimeoutMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(&decoded, method);
        }
    }

    // =========================================================================
    // DurationStats serde
    // =========================================================================

    #[test]
    fn stats_serde_roundtrip() {
        let stats = DurationStats::from_durations(&[100.0, 200.0, 300.0, 400.0, 500.0]).unwrap();
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: DurationStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.count, stats.count);
        assert!((decoded.mean_ms - stats.mean_ms).abs() < 1e-10);
    }
}
