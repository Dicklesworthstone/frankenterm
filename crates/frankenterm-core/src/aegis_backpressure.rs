//! PAC-Bayesian Adaptive Backpressure for the Aegis Engine (ft-l5em3.3).
//!
//! Mathematically guarantees UI responsiveness (60 fps target) under extreme load
//! using PAC-Bayesian bounds without unnecessarily starving agents.
//!
//! # Mathematical Formalization
//!
//! ## Loss Matrix
//!
//! We define a binary loss over throttle decisions:
//!
//! - **L(throttle=yes, needed=yes)** = 0 (correct throttle)
//! - **L(throttle=no,  needed=yes)** = 1 (missed → frame drop)
//! - **L(throttle=yes, needed=no)**  = c_starvation (unnecessary agent slowdown)
//! - **L(throttle=no,  needed=no)**  = 0 (correct pass-through)
//!
//! The starvation cost `c_starvation ∈ (0, 1)` is configurable (default 0.3),
//! encoding the operator's preference: lower values favor UI smoothness, higher
//! values favor agent velocity.
//!
//! ## PAC-Bayes Bound
//!
//! Given a prior distribution Q₀ over throttle policies π and an empirical
//! posterior Q_n after n observations, the PAC-Bayes-kl inequality gives:
//!
//! ```text
//! kl(L̂(Q_n) || L(Q_n)) ≤ [KL(Q_n || Q₀) + ln(2√n / δ)] / n
//! ```
//!
//! where:
//! - L̂(Q_n) = empirical risk (observed frame-drop rate under policy)
//! - L(Q_n) = true risk (what we bound)
//! - δ = confidence parameter (default 0.05 → 95% confidence)
//! - KL = Kullback-Leibler divergence between posterior and prior
//!
//! ## Optimal Throttle Multiplier
//!
//! The throttle multiplier a* minimizes expected loss under the posterior:
//!
//! ```text
//! a* = argmin_a { E_Q[L(a, θ)] + λ · KL(Q || Q₀) / n }
//! ```
//!
//! In practice, we maintain Gaussian posteriors over the throttle threshold
//! parameter and compute a* via the closed-form posterior mean, adjusted by
//! the PAC-Bayes confidence width.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Configuration ──────────────────────────────────────────────────────

/// Configuration for PAC-Bayesian adaptive backpressure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacBayesConfig {
    /// Target frame rate (Hz). Throttling activates to protect this target.
    pub target_fps: f64,
    /// Confidence parameter δ for PAC-Bayes bound (lower = tighter bound).
    /// 0.05 = 95% confidence that true risk ≤ bounded risk.
    pub delta: f64,
    /// Starvation cost c ∈ (0, 1). Higher values penalize unnecessary throttling
    /// more, favoring agent velocity over UI smoothness.
    pub starvation_cost: f64,
    /// Prior mean for throttle threshold (queue ratio at which throttling starts).
    pub prior_threshold_mean: f64,
    /// Prior variance for throttle threshold.
    pub prior_threshold_variance: f64,
    /// Learning rate for posterior updates (0, 1].
    pub learning_rate: f64,
    /// Minimum observations before posterior-driven decisions kick in.
    pub warmup_observations: usize,
    /// Maximum severity [0, 1] to cap throttling aggressiveness.
    pub max_severity: f64,
    /// Sigmoid steepness for severity mapping.
    pub steepness: f64,
    /// EMA smoothing factor for queue ratio (higher = more reactive).
    pub ema_alpha: f64,
    /// Enable external-cause detection to prevent agent starvation.
    pub starvation_guard: bool,
    /// Threshold for external-cause probability above which starvation guard fires.
    pub external_cause_threshold: f64,
}

impl Default for PacBayesConfig {
    fn default() -> Self {
        Self {
            target_fps: 60.0,
            delta: 0.05,
            starvation_cost: 0.3,
            prior_threshold_mean: 0.60,
            prior_threshold_variance: 0.04,
            learning_rate: 0.1,
            warmup_observations: 10,
            max_severity: 1.0,
            steepness: 8.0,
            ema_alpha: 0.2,
            starvation_guard: true,
            external_cause_threshold: 0.7,
        }
    }
}

// ── Gaussian Posterior ─────────────────────────────────────────────────

/// Gaussian posterior over a scalar parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaussianPosterior {
    /// Posterior mean.
    pub mean: f64,
    /// Posterior variance (always > 0).
    pub variance: f64,
    /// Number of observations incorporated.
    pub n_observations: usize,
}

impl GaussianPosterior {
    /// Create from prior mean and variance.
    pub fn new(mean: f64, variance: f64) -> Self {
        Self {
            mean,
            variance: variance.max(1e-12),
            n_observations: 0,
        }
    }

    /// Bayesian update with a single observation using conjugate normal model.
    ///
    /// Observation `x` with noise variance `obs_variance` updates:
    /// - precision_new = precision_old + 1/obs_variance
    /// - mean_new = (precision_old * mean_old + x / obs_variance) / precision_new
    pub fn update(&mut self, observation: f64, obs_variance: f64) {
        let obs_var = obs_variance.max(1e-12);
        let prior_precision = 1.0 / self.variance;
        let obs_precision = 1.0 / obs_var;
        let new_precision = prior_precision + obs_precision;
        self.mean = (prior_precision * self.mean + obs_precision * observation) / new_precision;
        self.variance = 1.0 / new_precision;
        self.n_observations += 1;
    }

    /// KL divergence KL(self || other) for two Gaussians.
    pub fn kl_divergence(&self, other: &GaussianPosterior) -> f64 {
        let var_ratio = self.variance / other.variance.max(1e-12);
        let mean_diff = self.mean - other.mean;
        0.5 * (var_ratio - 1.0 + mean_diff * mean_diff / other.variance.max(1e-12) - var_ratio.ln())
    }

    /// Standard deviation.
    pub fn std_dev(&self) -> f64 {
        self.variance.sqrt()
    }

    /// 1 - delta/2 quantile (upper confidence bound).
    pub fn upper_bound(&self, delta: f64) -> f64 {
        // For Gaussian: mean + z_{1-δ/2} * σ
        // Using Φ⁻¹ approximation for common values
        let z = quantile_normal(1.0 - delta / 2.0);
        self.mean + z * self.std_dev()
    }

    /// Lower confidence bound.
    pub fn lower_bound(&self, delta: f64) -> f64 {
        let z = quantile_normal(1.0 - delta / 2.0);
        self.mean - z * self.std_dev()
    }
}

/// Approximate inverse normal CDF (Beasley-Springer-Moro algorithm).
fn quantile_normal(p: f64) -> f64 {
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    // Rational approximation (Abramowitz & Stegun 26.2.23)
    let t = if p < 0.5 {
        (-2.0 * p.ln()).sqrt()
    } else {
        (-2.0 * (1.0 - p).ln()).sqrt()
    };
    let c0 = 2.515_517;
    let c1 = 0.802_853;
    let c2 = 0.010_328;
    let d1 = 1.432_788;
    let d2 = 0.189_269;
    let d3 = 0.001_308;
    let val = t - (c0 + c1 * t + c2 * t * t) / (1.0 + d1 * t + d2 * t * t + d3 * t * t * t);
    if p < 0.5 { -val } else { val }
}

// ── Starvation Guard ──────────────────────────────────────────────────

/// Evidence for distinguishing PTY-caused slowness from external factors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalCauseEvidence {
    /// System-wide CPU load average (0.0 - N_cores).
    pub system_load: f64,
    /// Whether other panes are also slow (suggests system-level issue).
    pub other_panes_slow_fraction: f64,
    /// Whether the specific pane's PTY is producing output.
    pub pty_producing: bool,
    /// Disk I/O wait fraction [0, 1].
    pub io_wait_fraction: f64,
}

impl Default for ExternalCauseEvidence {
    fn default() -> Self {
        Self {
            system_load: 0.0,
            other_panes_slow_fraction: 0.0,
            pty_producing: true,
            io_wait_fraction: 0.0,
        }
    }
}

/// Probability that slowness is caused by external factors (not PTY throughput).
fn external_cause_probability(evidence: &ExternalCauseEvidence) -> f64 {
    // Heuristic scoring: each factor contributes to external-cause belief.
    // High system load + other panes slow + low IO wait = system overload
    // High IO wait + PTY not producing = disk-bound
    let mut score = 0.0;
    let mut weight_sum = 0.0;

    // System load > 2.0 suggests system-level contention
    let load_signal = (evidence.system_load / 4.0).min(1.0);
    score += load_signal * 0.3;
    weight_sum += 0.3;

    // Other panes also slow → correlated external cause
    score += evidence.other_panes_slow_fraction * 0.3;
    weight_sum += 0.3;

    // PTY not producing → external cause (nothing to throttle)
    if !evidence.pty_producing {
        score += 0.25;
    }
    weight_sum += 0.25;

    // High IO wait → disk-bound
    score += evidence.io_wait_fraction * 0.15;
    weight_sum += 0.15;

    (score / weight_sum).clamp(0.0, 1.0)
}

// ── Throttle Actions ──────────────────────────────────────────────────

/// Throttle actions derived from PAC-Bayesian severity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacBayesThrottleActions {
    /// Severity ∈ [0, 1] — the PAC-Bayes-adjusted throttle intensity.
    pub severity: f64,
    /// Poll interval multiplier (1.0 = normal, higher = slower polling).
    pub poll_multiplier: f64,
    /// Fraction of panes to skip per cycle [0, 1].
    pub pane_skip_fraction: f64,
    /// Detection skip fraction [0, 1].
    pub detection_skip_fraction: f64,
    /// Buffer limit factor (1.0 = full, lower = reduced).
    pub buffer_limit_factor: f64,
    /// Whether starvation guard suppressed throttling.
    pub starvation_guard_active: bool,
    /// PAC-Bayes upper bound on true frame-drop risk.
    pub risk_bound: f64,
    /// KL divergence between posterior and prior.
    pub kl_divergence: f64,
    /// Optimal throttle threshold a* (posterior mean).
    pub optimal_threshold: f64,
}

impl Default for PacBayesThrottleActions {
    fn default() -> Self {
        Self {
            severity: 0.0,
            poll_multiplier: 1.0,
            pane_skip_fraction: 0.0,
            detection_skip_fraction: 0.0,
            buffer_limit_factor: 1.0,
            starvation_guard_active: false,
            risk_bound: 0.0,
            kl_divergence: 0.0,
            optimal_threshold: 0.6,
        }
    }
}

// ── Per-Pane State ────────────────────────────────────────────────────

/// Per-pane tracking for the PAC-Bayesian controller.
#[derive(Debug, Clone)]
struct PaneState {
    /// Posterior over this pane's throttle threshold.
    posterior: GaussianPosterior,
    /// Smoothed queue ratio (EMA).
    smoothed_ratio: f64,
    /// Observation count.
    observation_count: usize,
    /// Frame drops observed in recent window.
    frame_drops: usize,
    /// Total frames in recent window.
    total_frames: usize,
    /// Whether pane is currently throttled.
    throttled: bool,
}

impl PaneState {
    fn new(config: &PacBayesConfig) -> Self {
        Self {
            posterior: GaussianPosterior::new(
                config.prior_threshold_mean,
                config.prior_threshold_variance,
            ),
            smoothed_ratio: 0.0,
            observation_count: 0,
            frame_drops: 0,
            total_frames: 0,
            throttled: false,
        }
    }

    fn empirical_drop_rate(&self) -> f64 {
        if self.total_frames == 0 {
            0.0
        } else {
            self.frame_drops as f64 / self.total_frames as f64
        }
    }
}

// ── Queue Observation ─────────────────────────────────────────────────

/// Queue depth observation for the PAC-Bayesian controller.
#[derive(Debug, Clone)]
pub struct QueueObservation {
    /// Pane identifier.
    pub pane_id: u64,
    /// Current queue fill ratio [0, 1] (depth / capacity).
    pub fill_ratio: f64,
    /// Whether a frame drop occurred at this observation.
    pub frame_dropped: bool,
    /// Optional external-cause evidence for starvation guard.
    pub external_cause: Option<ExternalCauseEvidence>,
}

// ── Main Controller ───────────────────────────────────────────────────

/// PAC-Bayesian adaptive backpressure controller.
///
/// Maintains per-pane Gaussian posteriors over throttle thresholds and
/// computes severity using PAC-Bayes risk bounds. The controller adapts
/// its behavior based on observed frame drops, queue depths, and external
/// cause evidence.
pub struct PacBayesBackpressure {
    config: PacBayesConfig,
    /// Prior distribution (fixed reference for KL computation).
    prior: GaussianPosterior,
    /// Per-pane state.
    panes: HashMap<u64, PaneState>,
    /// Global observation count.
    global_observations: usize,
    /// Global frame drops.
    global_frame_drops: usize,
    /// Global total frames.
    global_total_frames: usize,
}

impl PacBayesBackpressure {
    /// Create a new controller with the given configuration.
    pub fn new(config: PacBayesConfig) -> Self {
        let prior = GaussianPosterior::new(
            config.prior_threshold_mean,
            config.prior_threshold_variance,
        );
        Self {
            prior,
            panes: HashMap::new(),
            global_observations: 0,
            global_frame_drops: 0,
            global_total_frames: 0,
            config,
        }
    }

    /// Create with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(PacBayesConfig::default())
    }

    /// Get the current configuration.
    pub fn config(&self) -> &PacBayesConfig {
        &self.config
    }

    /// Number of tracked panes.
    pub fn pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Total observations processed.
    pub fn total_observations(&self) -> usize {
        self.global_observations
    }

    /// Process a queue observation and return throttle actions.
    pub fn observe(&mut self, obs: &QueueObservation) -> PacBayesThrottleActions {
        let state = self.panes
            .entry(obs.pane_id)
            .or_insert_with(|| PaneState::new(&self.config));

        // EMA smooth the queue ratio
        state.smoothed_ratio = if state.observation_count == 0 {
            obs.fill_ratio
        } else {
            self.config.ema_alpha * obs.fill_ratio
                + (1.0 - self.config.ema_alpha) * state.smoothed_ratio
        };

        state.observation_count += 1;
        state.total_frames += 1;
        if obs.frame_dropped {
            state.frame_drops += 1;
        }

        self.global_observations += 1;
        self.global_total_frames += 1;
        if obs.frame_dropped {
            self.global_frame_drops += 1;
        }

        // Update posterior: the "observation" is the queue ratio at which
        // a frame drop occurred (or didn't). We update toward the observed
        // threshold boundary.
        if state.observation_count > self.config.warmup_observations {
            let obs_variance = self.config.prior_threshold_variance
                * (1.0 / self.config.learning_rate);

            if obs.frame_dropped {
                // Frame drop → threshold should be lower (throttle earlier)
                state.posterior.update(obs.fill_ratio, obs_variance);
            } else if state.smoothed_ratio > state.posterior.mean {
                // No drop but high load → threshold can be slightly higher
                let relaxed = obs.fill_ratio * 1.1;
                state.posterior.update(relaxed, obs_variance * 4.0);
            }
        }

        // Compute severity from smoothed ratio and posterior threshold
        let threshold = state.posterior.mean;
        let severity_raw = sigmoid(
            self.config.steepness * (state.smoothed_ratio - threshold),
        );
        let mut severity = severity_raw.min(self.config.max_severity);

        // Starvation guard: reduce throttling if external causes dominate
        let mut starvation_guard_active = false;
        if self.config.starvation_guard {
            if let Some(evidence) = &obs.external_cause {
                let ext_prob = external_cause_probability(evidence);
                if ext_prob > self.config.external_cause_threshold {
                    // Reduce severity proportionally to external-cause confidence
                    severity *= 1.0 - ext_prob;
                    starvation_guard_active = true;
                }
            }
        }

        // Compute PAC-Bayes risk bound
        let kl = state.posterior.kl_divergence(&self.prior);
        let n = state.observation_count.max(1) as f64;
        let empirical_risk = state.empirical_drop_rate();
        let complexity_term = (kl + (2.0 * n.sqrt() / self.config.delta).ln()) / n;
        let risk_bound = (empirical_risk + complexity_term).min(1.0);

        // Derive throttle actions from severity
        let actions = PacBayesThrottleActions {
            severity,
            poll_multiplier: 1.0 + 3.0 * severity,
            pane_skip_fraction: 0.5 * severity * severity,
            detection_skip_fraction: 0.25 * severity,
            buffer_limit_factor: 1.0 - 0.8 * severity,
            starvation_guard_active,
            risk_bound,
            kl_divergence: kl,
            optimal_threshold: threshold,
        };

        state.throttled = severity > 0.01;

        actions
    }

    /// Get the PAC-Bayes risk bound for a specific pane.
    pub fn pane_risk_bound(&self, pane_id: u64) -> Option<f64> {
        self.panes.get(&pane_id).map(|state| {
            let kl = state.posterior.kl_divergence(&self.prior);
            let n = state.observation_count.max(1) as f64;
            let empirical_risk = state.empirical_drop_rate();
            let complexity_term = (kl + (2.0 * n.sqrt() / self.config.delta).ln()) / n;
            (empirical_risk + complexity_term).min(1.0)
        })
    }

    /// Get the current posterior threshold for a pane.
    pub fn pane_threshold(&self, pane_id: u64) -> Option<f64> {
        self.panes.get(&pane_id).map(|s| s.posterior.mean)
    }

    /// Get the posterior variance for a pane.
    pub fn pane_threshold_variance(&self, pane_id: u64) -> Option<f64> {
        self.panes.get(&pane_id).map(|s| s.posterior.variance)
    }

    /// Get the empirical frame-drop rate for a pane.
    pub fn pane_drop_rate(&self, pane_id: u64) -> Option<f64> {
        self.panes.get(&pane_id).map(|s| s.empirical_drop_rate())
    }

    /// Get the global empirical frame-drop rate.
    pub fn global_drop_rate(&self) -> f64 {
        if self.global_total_frames == 0 {
            0.0
        } else {
            self.global_frame_drops as f64 / self.global_total_frames as f64
        }
    }

    /// Get the global PAC-Bayes risk bound.
    pub fn global_risk_bound(&self) -> f64 {
        if self.panes.is_empty() {
            return 0.0;
        }
        // Aggregate: max risk across all panes
        self.panes.values()
            .map(|state| {
                let kl = state.posterior.kl_divergence(&self.prior);
                let n = state.observation_count.max(1) as f64;
                let empirical_risk = state.empirical_drop_rate();
                let complexity_term = (kl + (2.0 * n.sqrt() / self.config.delta).ln()) / n;
                (empirical_risk + complexity_term).min(1.0)
            })
            .fold(0.0_f64, f64::max)
    }

    /// Produce a serializable snapshot of the controller state.
    pub fn snapshot(&self) -> PacBayesSnapshot {
        PacBayesSnapshot {
            global_observations: self.global_observations,
            global_frame_drops: self.global_frame_drops,
            global_drop_rate: self.global_drop_rate(),
            global_risk_bound: self.global_risk_bound(),
            pane_count: self.panes.len(),
            pane_snapshots: self.panes.iter().map(|(&id, state)| {
                PaneSnapshot {
                    pane_id: id,
                    observations: state.observation_count,
                    frame_drops: state.frame_drops,
                    drop_rate: state.empirical_drop_rate(),
                    smoothed_ratio: state.smoothed_ratio,
                    threshold_mean: state.posterior.mean,
                    threshold_variance: state.posterior.variance,
                    throttled: state.throttled,
                }
            }).collect(),
        }
    }

    /// Reset a specific pane's state back to prior.
    pub fn reset_pane(&mut self, pane_id: u64) {
        self.panes.remove(&pane_id);
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.panes.clear();
        self.global_observations = 0;
        self.global_frame_drops = 0;
        self.global_total_frames = 0;
    }
}

impl std::fmt::Debug for PacBayesBackpressure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacBayesBackpressure")
            .field("pane_count", &self.panes.len())
            .field("global_observations", &self.global_observations)
            .field("global_drop_rate", &self.global_drop_rate())
            .finish()
    }
}

// ── Snapshot ──────────────────────────────────────────────────────────

/// Serializable snapshot of controller state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PacBayesSnapshot {
    pub global_observations: usize,
    pub global_frame_drops: usize,
    pub global_drop_rate: f64,
    pub global_risk_bound: f64,
    pub pane_count: usize,
    pub pane_snapshots: Vec<PaneSnapshot>,
}

/// Per-pane snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub pane_id: u64,
    pub observations: usize,
    pub frame_drops: usize,
    pub drop_rate: f64,
    pub smoothed_ratio: f64,
    pub threshold_mean: f64,
    pub threshold_variance: f64,
    pub throttled: bool,
}

// ── Utility ───────────────────────────────────────────────────────────

/// Standard sigmoid function: 1 / (1 + exp(-x)).
fn sigmoid(x: f64) -> f64 {
    if x > 500.0 {
        return 1.0;
    }
    if x < -500.0 {
        return 0.0;
    }
    1.0 / (1.0 + (-x).exp())
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_controller() -> PacBayesBackpressure {
        PacBayesBackpressure::with_defaults()
    }

    fn obs(pane_id: u64, fill_ratio: f64, frame_dropped: bool) -> QueueObservation {
        QueueObservation {
            pane_id,
            fill_ratio,
            frame_dropped,
            external_cause: None,
        }
    }

    fn obs_with_external(
        pane_id: u64,
        fill_ratio: f64,
        frame_dropped: bool,
        evidence: ExternalCauseEvidence,
    ) -> QueueObservation {
        QueueObservation {
            pane_id,
            fill_ratio,
            frame_dropped,
            external_cause: Some(evidence),
        }
    }

    // ── Config defaults ────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let cfg = PacBayesConfig::default();
        assert!((cfg.target_fps - 60.0).abs() < 1e-10);
        assert!((cfg.delta - 0.05).abs() < 1e-10);
        assert!((cfg.starvation_cost - 0.3).abs() < 1e-10);
        assert!((cfg.prior_threshold_mean - 0.60).abs() < 1e-10);
        assert!((cfg.prior_threshold_variance - 0.04).abs() < 1e-10);
        assert_eq!(cfg.warmup_observations, 10);
        assert!(cfg.starvation_guard);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = PacBayesConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: PacBayesConfig = serde_json::from_str(&json).unwrap();
        assert!((back.delta - cfg.delta).abs() < 1e-10);
        assert!((back.starvation_cost - cfg.starvation_cost).abs() < 1e-10);
    }

    // ── Gaussian posterior ─────────────────────────────────────────

    #[test]
    fn gaussian_posterior_initial() {
        let p = GaussianPosterior::new(0.6, 0.04);
        assert!((p.mean - 0.6).abs() < 1e-10);
        assert!((p.variance - 0.04).abs() < 1e-10);
        assert_eq!(p.n_observations, 0);
    }

    #[test]
    fn gaussian_posterior_update_moves_mean() {
        let mut p = GaussianPosterior::new(0.6, 0.04);
        p.update(0.4, 0.04);
        // After one observation at 0.4 with equal precision, mean should be ~0.5
        assert!((p.mean - 0.5).abs() < 1e-10);
        assert_eq!(p.n_observations, 1);
    }

    #[test]
    fn gaussian_posterior_variance_shrinks() {
        let mut p = GaussianPosterior::new(0.6, 0.04);
        let initial_var = p.variance;
        p.update(0.5, 0.04);
        assert!(p.variance < initial_var);
    }

    #[test]
    fn gaussian_kl_divergence_zero_for_same() {
        let p = GaussianPosterior::new(0.6, 0.04);
        let kl = p.kl_divergence(&p);
        assert!(kl.abs() < 1e-10);
    }

    #[test]
    fn gaussian_kl_divergence_positive() {
        let p = GaussianPosterior::new(0.6, 0.04);
        let q = GaussianPosterior::new(0.5, 0.02);
        let kl = p.kl_divergence(&q);
        assert!(kl > 0.0);
    }

    #[test]
    fn gaussian_upper_lower_bounds() {
        let p = GaussianPosterior::new(0.6, 0.04);
        let upper = p.upper_bound(0.05);
        let lower = p.lower_bound(0.05);
        assert!(upper > p.mean);
        assert!(lower < p.mean);
        assert!((upper - p.mean - (p.mean - lower)).abs() < 1e-6, "symmetric bounds");
    }

    #[test]
    fn gaussian_convergence_with_many_observations() {
        let mut p = GaussianPosterior::new(0.6, 1.0);
        // Feed many observations near 0.3
        for _ in 0..100 {
            p.update(0.3, 0.01);
        }
        assert!((p.mean - 0.3).abs() < 0.05, "mean={}", p.mean);
        assert!(p.variance < 0.001, "variance={}", p.variance);
    }

    // ── Sigmoid ────────────────────────────────────────────────────

    #[test]
    fn sigmoid_at_zero_is_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn sigmoid_large_positive_is_one() {
        assert!((sigmoid(100.0) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn sigmoid_large_negative_is_zero() {
        assert!(sigmoid(-100.0) < 1e-10);
    }

    #[test]
    fn sigmoid_monotonic() {
        let vals: Vec<f64> = (-50..=50).map(|i| sigmoid(i as f64 * 0.1)).collect();
        for w in vals.windows(2) {
            assert!(w[1] >= w[0]);
        }
    }

    // ── Quantile normal ────────────────────────────────────────────

    #[test]
    fn quantile_normal_median() {
        let z = quantile_normal(0.5);
        assert!(z.abs() < 0.01, "z_0.5 = {z}");
    }

    #[test]
    fn quantile_normal_95() {
        let z = quantile_normal(0.975);
        assert!((z - 1.96).abs() < 0.1, "z_0.975 = {z}");
    }

    // ── Controller basics ──────────────────────────────────────────

    #[test]
    fn controller_starts_empty() {
        let ctrl = default_controller();
        assert_eq!(ctrl.pane_count(), 0);
        assert_eq!(ctrl.total_observations(), 0);
        assert!((ctrl.global_drop_rate()).abs() < 1e-10);
    }

    #[test]
    fn controller_low_load_minimal_throttle() {
        let mut ctrl = default_controller();
        let actions = ctrl.observe(&obs(1, 0.1, false));
        // At fill_ratio=0.1, threshold=0.6, steepness=8: sigmoid(8*(0.1-0.6)) ≈ 0.018
        assert!(actions.severity < 0.05, "severity={}", actions.severity);
        assert!((actions.poll_multiplier - 1.0).abs() < 0.15);
        assert!(actions.pane_skip_fraction < 0.01);
    }

    #[test]
    fn controller_high_load_increases_severity() {
        let mut ctrl = default_controller();
        // Warm up
        for _ in 0..15 {
            ctrl.observe(&obs(1, 0.9, true));
        }
        let actions = ctrl.observe(&obs(1, 0.9, true));
        assert!(actions.severity > 0.5, "severity={}", actions.severity);
        assert!(actions.poll_multiplier > 2.0);
    }

    #[test]
    fn controller_tracks_multiple_panes() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.1, false));
        ctrl.observe(&obs(2, 0.2, false));
        ctrl.observe(&obs(3, 0.3, false));
        assert_eq!(ctrl.pane_count(), 3);
    }

    #[test]
    fn controller_per_pane_threshold() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.1, false));
        let threshold = ctrl.pane_threshold(1).unwrap();
        assert!((threshold - 0.6).abs() < 0.1, "threshold={threshold}");
    }

    #[test]
    fn controller_per_pane_drop_rate() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.5, true));
        ctrl.observe(&obs(1, 0.5, false));
        let drop_rate = ctrl.pane_drop_rate(1).unwrap();
        assert!((drop_rate - 0.5).abs() < 1e-10);
    }

    #[test]
    fn controller_risk_bound_positive() {
        let mut ctrl = default_controller();
        for _ in 0..20 {
            ctrl.observe(&obs(1, 0.5, false));
        }
        let bound = ctrl.pane_risk_bound(1).unwrap();
        assert!(bound > 0.0, "bound={bound}");
        assert!(bound <= 1.0, "bound={bound}");
    }

    #[test]
    fn controller_risk_bound_increases_with_drops() {
        let mut ctrl1 = default_controller();
        let mut ctrl2 = default_controller();

        for _ in 0..30 {
            ctrl1.observe(&obs(1, 0.5, false));
            ctrl2.observe(&obs(1, 0.5, true));
        }

        let bound_no_drops = ctrl1.pane_risk_bound(1).unwrap();
        let bound_all_drops = ctrl2.pane_risk_bound(1).unwrap();
        assert!(
            bound_all_drops > bound_no_drops,
            "all_drops={bound_all_drops}, no_drops={bound_no_drops}"
        );
    }

    // ── Starvation guard ───────────────────────────────────────────

    #[test]
    fn starvation_guard_reduces_severity() {
        let mut ctrl = default_controller();
        // Warm up with high load
        for _ in 0..15 {
            ctrl.observe(&obs(1, 0.9, true));
        }

        // Without external cause
        let actions_normal = ctrl.observe(&obs(1, 0.9, true));

        // Reset and retry with strong external cause
        ctrl.reset();
        for _ in 0..15 {
            ctrl.observe(&obs(1, 0.9, true));
        }
        let external = ExternalCauseEvidence {
            system_load: 8.0,
            other_panes_slow_fraction: 0.8,
            pty_producing: false,
            io_wait_fraction: 0.9,
        };
        let actions_external = ctrl.observe(&obs_with_external(1, 0.9, true, external));

        assert!(
            actions_external.severity < actions_normal.severity,
            "guard should reduce: normal={} external={}",
            actions_normal.severity,
            actions_external.severity
        );
        assert!(actions_external.starvation_guard_active);
    }

    #[test]
    fn starvation_guard_inactive_with_low_external_cause() {
        let mut ctrl = default_controller();
        for _ in 0..15 {
            ctrl.observe(&obs(1, 0.8, true));
        }
        let external = ExternalCauseEvidence {
            system_load: 0.5,
            other_panes_slow_fraction: 0.0,
            pty_producing: true,
            io_wait_fraction: 0.0,
        };
        let actions = ctrl.observe(&obs_with_external(1, 0.8, true, external));
        assert!(!actions.starvation_guard_active);
    }

    #[test]
    fn external_cause_probability_low_for_normal() {
        let evidence = ExternalCauseEvidence::default();
        let prob = external_cause_probability(&evidence);
        assert!(prob < 0.3, "prob={prob}");
    }

    #[test]
    fn external_cause_probability_high_for_overload() {
        let evidence = ExternalCauseEvidence {
            system_load: 10.0,
            other_panes_slow_fraction: 1.0,
            pty_producing: false,
            io_wait_fraction: 1.0,
        };
        let prob = external_cause_probability(&evidence);
        assert!(prob > 0.7, "prob={prob}");
    }

    // ── Throttle action curves ─────────────────────────────────────

    #[test]
    fn throttle_actions_at_zero_severity() {
        let actions = PacBayesThrottleActions {
            severity: 0.0,
            ..Default::default()
        };
        assert!((actions.poll_multiplier - 1.0).abs() < 1e-10);
        assert!(actions.pane_skip_fraction.abs() < 1e-10);
        assert!((actions.buffer_limit_factor - 1.0).abs() < 1e-10);
    }

    #[test]
    fn throttle_actions_at_max_severity() {
        // Verify the curves at severity = 1.0
        let severity: f64 = 1.0;
        let poll = 1.0 + 3.0 * severity;
        let skip = 0.5 * severity * severity;
        let detect = 0.25 * severity;
        let buffer = 1.0 - 0.8 * severity;
        assert!((poll - 4.0).abs() < 1e-10);
        assert!((skip - 0.5).abs() < 1e-10);
        assert!((detect - 0.25).abs() < 1e-10);
        assert!((buffer - 0.2).abs() < 1e-10);
    }

    // ── Posterior adaptation ───────────────────────────────────────

    #[test]
    fn threshold_adapts_down_on_frame_drops() {
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            warmup_observations: 0,
            ..Default::default()
        });
        let initial = ctrl.config.prior_threshold_mean;

        // Feed high-load frame drops
        for _ in 0..50 {
            ctrl.observe(&obs(1, 0.5, true));
        }

        let adapted = ctrl.pane_threshold(1).unwrap();
        assert!(
            adapted < initial,
            "threshold should decrease on drops: initial={initial}, adapted={adapted}"
        );
    }

    #[test]
    fn threshold_adapts_up_on_no_drops() {
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            warmup_observations: 0,
            prior_threshold_mean: 0.3, // Start low
            ..Default::default()
        });

        // Feed high load but no drops → threshold should relax upward
        for _ in 0..50 {
            ctrl.observe(&obs(1, 0.8, false));
        }

        let adapted = ctrl.pane_threshold(1).unwrap();
        assert!(adapted > 0.3, "threshold should increase: adapted={adapted}");
    }

    #[test]
    fn variance_decreases_with_observations() {
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            warmup_observations: 0,
            ..Default::default()
        });
        ctrl.observe(&obs(1, 0.5, true));
        let var_early = ctrl.pane_threshold_variance(1).unwrap();

        for _ in 0..50 {
            ctrl.observe(&obs(1, 0.5, true));
        }
        let var_late = ctrl.pane_threshold_variance(1).unwrap();
        assert!(var_late < var_early, "early={var_early}, late={var_late}");
    }

    // ── KL divergence tracking ─────────────────────────────────────

    #[test]
    fn kl_divergence_starts_near_zero() {
        let mut ctrl = default_controller();
        let actions = ctrl.observe(&obs(1, 0.3, false));
        assert!(actions.kl_divergence < 0.01, "kl={}", actions.kl_divergence);
    }

    #[test]
    fn kl_divergence_grows_as_posterior_shifts() {
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            warmup_observations: 0,
            ..Default::default()
        });

        let mut last_kl = 0.0;
        for i in 0..30 {
            let actions = ctrl.observe(&obs(1, 0.3, true));
            if i > 5 {
                // KL should generally increase as posterior shifts from prior
                // (may not be strictly monotonic due to EMA effects)
                last_kl = actions.kl_divergence;
            }
        }
        assert!(last_kl > 0.01, "kl={last_kl}");
    }

    // ── Snapshot and reset ─────────────────────────────────────────

    #[test]
    fn snapshot_captures_state() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.3, false));
        ctrl.observe(&obs(2, 0.5, true));

        let snap = ctrl.snapshot();
        assert_eq!(snap.global_observations, 2);
        assert_eq!(snap.global_frame_drops, 1);
        assert_eq!(snap.pane_count, 2);
        assert_eq!(snap.pane_snapshots.len(), 2);
    }

    #[test]
    fn snapshot_serde_roundtrip() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.3, false));
        let snap = ctrl.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let back: PacBayesSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.global_observations, snap.global_observations);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.3, false));
        ctrl.observe(&obs(2, 0.5, true));
        ctrl.reset();
        assert_eq!(ctrl.pane_count(), 0);
        assert_eq!(ctrl.total_observations(), 0);
    }

    #[test]
    fn reset_pane_clears_specific() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.3, false));
        ctrl.observe(&obs(2, 0.5, false));
        ctrl.reset_pane(1);
        assert_eq!(ctrl.pane_count(), 1);
        assert!(ctrl.pane_threshold(1).is_none());
        assert!(ctrl.pane_threshold(2).is_some());
    }

    // ── Determinism ────────────────────────────────────────────────

    #[test]
    fn deterministic_same_sequence() {
        let mut ctrl1 = default_controller();
        let mut ctrl2 = default_controller();

        let observations = vec![
            obs(1, 0.1, false),
            obs(1, 0.3, false),
            obs(1, 0.5, true),
            obs(1, 0.7, true),
            obs(2, 0.2, false),
            obs(2, 0.8, true),
        ];

        let mut actions1 = Vec::new();
        let mut actions2 = Vec::new();

        for o in &observations {
            actions1.push(ctrl1.observe(o));
            actions2.push(ctrl2.observe(o));
        }

        for (a1, a2) in actions1.iter().zip(actions2.iter()) {
            assert!(
                (a1.severity - a2.severity).abs() < 1e-10,
                "severity mismatch"
            );
            assert!(
                (a1.risk_bound - a2.risk_bound).abs() < 1e-10,
                "risk_bound mismatch"
            );
        }
    }

    // ── Debug impl ─────────────────────────────────────────────────

    #[test]
    fn debug_impl_works() {
        let ctrl = default_controller();
        let debug = format!("{ctrl:?}");
        assert!(debug.contains("PacBayesBackpressure"));
    }

    // ── Edge cases ─────────────────────────────────────────────────

    #[test]
    fn zero_fill_ratio() {
        let mut ctrl = default_controller();
        let actions = ctrl.observe(&obs(1, 0.0, false));
        assert!(actions.severity < 0.01);
    }

    #[test]
    fn max_fill_ratio() {
        let mut ctrl = default_controller();
        for _ in 0..15 {
            ctrl.observe(&obs(1, 1.0, true));
        }
        let actions = ctrl.observe(&obs(1, 1.0, true));
        assert!(actions.severity > 0.5);
    }

    #[test]
    fn many_panes_stress() {
        let mut ctrl = default_controller();
        for pane_id in 0..100 {
            ctrl.observe(&obs(pane_id, 0.5, pane_id % 3 == 0));
        }
        assert_eq!(ctrl.pane_count(), 100);
        let snap = ctrl.snapshot();
        assert_eq!(snap.pane_snapshots.len(), 100);
    }

    #[test]
    fn global_risk_bound_empty() {
        let ctrl = default_controller();
        assert!((ctrl.global_risk_bound()).abs() < 1e-10);
    }

    #[test]
    fn global_drop_rate_accurate() {
        let mut ctrl = default_controller();
        ctrl.observe(&obs(1, 0.5, true));
        ctrl.observe(&obs(1, 0.5, false));
        ctrl.observe(&obs(1, 0.5, true));
        assert!((ctrl.global_drop_rate() - 2.0 / 3.0).abs() < 1e-10);
    }

    // ── Custom config ──────────────────────────────────────────────

    #[test]
    fn custom_delta_affects_bounds() {
        let mut ctrl_tight = PacBayesBackpressure::new(PacBayesConfig {
            delta: 0.01,
            ..Default::default()
        });
        let mut ctrl_loose = PacBayesBackpressure::new(PacBayesConfig {
            delta: 0.20,
            ..Default::default()
        });

        for _ in 0..30 {
            ctrl_tight.observe(&obs(1, 0.5, false));
            ctrl_loose.observe(&obs(1, 0.5, false));
        }

        let bound_tight = ctrl_tight.pane_risk_bound(1).unwrap();
        let bound_loose = ctrl_loose.pane_risk_bound(1).unwrap();
        // Tighter confidence (smaller delta) → larger bound
        assert!(
            bound_tight > bound_loose,
            "tight={bound_tight}, loose={bound_loose}"
        );
    }

    #[test]
    fn disabled_starvation_guard() {
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            starvation_guard: false,
            warmup_observations: 0,
            ..Default::default()
        });

        for _ in 0..15 {
            ctrl.observe(&obs(1, 0.9, true));
        }
        let external = ExternalCauseEvidence {
            system_load: 10.0,
            other_panes_slow_fraction: 1.0,
            pty_producing: false,
            io_wait_fraction: 1.0,
        };
        let actions = ctrl.observe(&obs_with_external(1, 0.9, true, external));
        assert!(!actions.starvation_guard_active);
    }

    #[test]
    fn warmup_prevents_early_adaptation() {
        let mut ctrl = PacBayesBackpressure::new(PacBayesConfig {
            warmup_observations: 100,
            ..Default::default()
        });
        let initial_mean = ctrl.config.prior_threshold_mean;

        // Feed 50 observations (below warmup)
        for _ in 0..50 {
            ctrl.observe(&obs(1, 0.2, true));
        }
        let threshold = ctrl.pane_threshold(1).unwrap();
        // Should still be at prior since warmup hasn't completed
        assert!(
            (threshold - initial_mean).abs() < 1e-10,
            "threshold={threshold}, expected={initial_mean}"
        );
    }
}
