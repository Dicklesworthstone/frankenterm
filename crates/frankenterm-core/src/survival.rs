//! Survival/hazard model for mux server health prediction.
//!
//! Implements a Weibull proportional hazards model that predicts mux server
//! failure probability in real-time, enabling proactive session saves BEFORE
//! crashes occur.
//!
//! # Model
//!
//! ```text
//! h(t|X) = h₀(t) × exp(β₁·RSS + β₂·pane_count + β₃·output_rate + β₄·uptime + β₅·conn_errors)
//! ```
//!
//! Where h₀(t) = (k/λ)(t/λ)^(k-1) is the Weibull baseline hazard.
//!
//! # Hazard thresholds
//!
//! | Hazard rate | Action                                    |
//! |-------------|-------------------------------------------|
//! | > 0.5       | Increase snapshot frequency to every 5min |
//! | > 0.8       | Trigger immediate full snapshot            |
//! | > 0.95      | Alert user + prepare graceful restart      |

#[cfg(test)]
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::debug;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the survival model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SurvivalConfig {
    /// Minimum observations before the model produces estimates.
    pub warmup_observations: usize,

    /// Learning rate for online parameter updates (0.0–1.0).
    pub learning_rate: f64,

    /// Hazard threshold for increasing snapshot frequency.
    pub snapshot_frequency_threshold: f64,

    /// Hazard threshold for triggering immediate snapshot.
    pub immediate_snapshot_threshold: f64,

    /// Hazard threshold for alerting the user.
    pub alert_threshold: f64,

    /// Maximum number of observations to retain for parameter estimation.
    pub max_observations: usize,

    /// Update interval for hazard computation.
    pub update_interval: Duration,
}

impl Default for SurvivalConfig {
    fn default() -> Self {
        Self {
            warmup_observations: 10,
            learning_rate: 0.01,
            snapshot_frequency_threshold: 0.5,
            immediate_snapshot_threshold: 0.8,
            alert_threshold: 0.95,
            max_observations: 1000,
            update_interval: Duration::from_secs(30),
        }
    }
}

// =============================================================================
// Covariates
// =============================================================================

/// Feature vector for the proportional hazards model.
///
/// Each field is a covariate that contributes to the hazard rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Covariates {
    /// Resident set size in GB.
    pub rss_gb: f64,
    /// Number of active panes.
    pub pane_count: f64,
    /// Aggregate output rate in MB/s across all panes.
    pub output_rate_mbps: f64,
    /// Uptime in hours.
    pub uptime_hours: f64,
    /// Connection error rate (errors per minute).
    pub conn_error_rate: f64,
}

impl Covariates {
    /// Number of covariates in the model.
    pub const COUNT: usize = 5;

    /// Convert to a fixed-size array for linear algebra.
    #[must_use]
    pub fn to_array(&self) -> [f64; Self::COUNT] {
        [
            self.rss_gb,
            self.pane_count,
            self.output_rate_mbps,
            self.uptime_hours,
            self.conn_error_rate,
        ]
    }

    /// Covariate names for reporting.
    #[must_use]
    pub fn names() -> [&'static str; Self::COUNT] {
        [
            "rss_gb",
            "pane_count",
            "output_rate_mbps",
            "uptime_hours",
            "conn_error_rate",
        ]
    }

    /// Dot product with a coefficient vector.
    #[must_use]
    pub fn dot(&self, beta: &[f64; Self::COUNT]) -> f64 {
        let x = self.to_array();
        x.iter().zip(beta.iter()).map(|(a, b)| a * b).sum()
    }
}

impl Default for Covariates {
    fn default() -> Self {
        Self {
            rss_gb: 0.0,
            pane_count: 0.0,
            output_rate_mbps: 0.0,
            uptime_hours: 0.0,
            conn_error_rate: 0.0,
        }
    }
}

// =============================================================================
// Observation
// =============================================================================

/// A single survival observation (potentially right-censored).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Time-to-event (or censoring time) in hours.
    pub time: f64,
    /// Whether the event (failure) was observed.
    /// `true` = actual failure, `false` = right-censored (still running).
    pub event_observed: bool,
    /// Covariates at observation time.
    pub covariates: Covariates,
    /// Unix timestamp of observation.
    pub timestamp_secs: u64,
}

// =============================================================================
// Weibull parameters
// =============================================================================

/// Weibull distribution parameters plus regression coefficients.
///
/// The hazard function is:
///   h(t|X) = (k/λ)(t/λ)^(k-1) × exp(β·X)
///
/// The survival function is:
///   S(t|X) = exp(-(t/λ)^k × exp(β·X))
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeibullParams {
    /// Shape parameter (k > 0). k > 1 means increasing hazard with time.
    pub shape: f64,
    /// Scale parameter (λ > 0). Characteristic life.
    pub scale: f64,
    /// Regression coefficients for covariates.
    pub beta: [f64; Covariates::COUNT],
}

impl Default for WeibullParams {
    fn default() -> Self {
        Self {
            // k > 1 reflects the empirical reality that mux servers degrade over time
            shape: 1.5,
            // λ chosen so baseline median life ≈ 168 hours (1 week)
            scale: 168.0,
            // Start with zero coefficients (no covariate effect)
            beta: [0.0; Covariates::COUNT],
        }
    }
}

impl WeibullParams {
    /// Baseline hazard at time t: h₀(t) = (k/λ)(t/λ)^(k-1).
    #[must_use]
    pub fn baseline_hazard(&self, t: f64) -> f64 {
        if t <= 0.0 || self.scale <= 0.0 || self.shape <= 0.0 {
            return 0.0;
        }
        let k = self.shape;
        let lam = self.scale;
        (k / lam) * (t / lam).powf(k - 1.0)
    }

    /// Full hazard rate: h(t|X) = h₀(t) × exp(β·X).
    #[must_use]
    pub fn hazard(&self, t: f64, covariates: &Covariates) -> f64 {
        let h0 = self.baseline_hazard(t);
        let linear_pred = covariates.dot(&self.beta);
        h0 * linear_pred.exp()
    }

    /// Cumulative hazard: H(t|X) = (t/λ)^k × exp(β·X).
    #[must_use]
    pub fn cumulative_hazard(&self, t: f64, covariates: &Covariates) -> f64 {
        if t <= 0.0 || self.scale <= 0.0 || self.shape <= 0.0 {
            return 0.0;
        }
        let linear_pred = covariates.dot(&self.beta);
        (t / self.scale).powf(self.shape) * linear_pred.exp()
    }

    /// Survival probability: S(t|X) = exp(-H(t|X)).
    #[must_use]
    pub fn survival_probability(&self, t: f64, covariates: &Covariates) -> f64 {
        (-self.cumulative_hazard(t, covariates)).exp()
    }

    /// Failure probability: F(t|X) = 1 - S(t|X).
    #[must_use]
    pub fn failure_probability(&self, t: f64, covariates: &Covariates) -> f64 {
        1.0 - self.survival_probability(t, covariates)
    }

    /// Log-likelihood contribution for a single observation.
    ///
    /// For observed events: log(h(t|X)) - H(t|X)
    /// For censored:        -H(t|X)
    #[must_use]
    pub fn log_likelihood_single(&self, obs: &Observation) -> f64 {
        let cum_h = self.cumulative_hazard(obs.time, &obs.covariates);
        if obs.event_observed {
            let h = self.hazard(obs.time, &obs.covariates);
            if h > 0.0 {
                h.ln() - cum_h
            } else {
                f64::NEG_INFINITY
            }
        } else {
            -cum_h
        }
    }
}

// =============================================================================
// Hazard action
// =============================================================================

/// Action recommended based on current hazard level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HazardAction {
    /// Normal operation, no action needed.
    None,
    /// Increase snapshot frequency (hazard > 0.5).
    IncreaseSnapshotFrequency,
    /// Trigger an immediate full snapshot (hazard > 0.8).
    ImmediateSnapshot,
    /// Alert the user and prepare for graceful restart (hazard > 0.95).
    AlertAndPrepareRestart,
}

impl std::fmt::Display for HazardAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::IncreaseSnapshotFrequency => write!(f, "increase_snapshot_frequency"),
            Self::ImmediateSnapshot => write!(f, "immediate_snapshot"),
            Self::AlertAndPrepareRestart => write!(f, "alert_and_prepare_restart"),
        }
    }
}

// =============================================================================
// Risk factor report
// =============================================================================

/// Individual covariate's contribution to the current hazard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFactor {
    /// Covariate name.
    pub name: String,
    /// Current covariate value.
    pub value: f64,
    /// Regression coefficient.
    pub coefficient: f64,
    /// Contribution to log-hazard (β × x).
    pub contribution: f64,
    /// Fraction of total risk attributable to this factor.
    pub risk_fraction: f64,
}

/// Comprehensive hazard assessment at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HazardReport {
    /// Unix timestamp.
    pub timestamp_secs: u64,
    /// Current hazard rate.
    pub hazard_rate: f64,
    /// Current survival probability.
    pub survival_probability: f64,
    /// Current failure probability.
    pub failure_probability: f64,
    /// Recommended action.
    pub action: HazardAction,
    /// Risk factor breakdown.
    pub risk_factors: Vec<RiskFactor>,
    /// Model parameters.
    pub params: WeibullParams,
    /// Whether the model is in warmup phase.
    pub in_warmup: bool,
    /// Number of observations used for fitting.
    pub observation_count: usize,
}

// =============================================================================
// Survival model
// =============================================================================

/// Online survival model for mux server health prediction.
///
/// Maintains Weibull parameters and updates them incrementally as new
/// observations arrive. Provides real-time hazard rate estimates and
/// recommended actions.
pub struct SurvivalModel {
    config: SurvivalConfig,
    params: RwLock<WeibullParams>,
    observations: RwLock<Vec<Observation>>,
    shutdown: AtomicBool,
    observation_count: AtomicU64,
}

impl SurvivalModel {
    /// Create a new survival model with default parameters.
    #[must_use]
    pub fn new(config: SurvivalConfig) -> Self {
        Self {
            config,
            params: RwLock::new(WeibullParams::default()),
            observations: RwLock::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            observation_count: AtomicU64::new(0),
        }
    }

    /// Create with specific initial parameters.
    #[must_use]
    pub fn with_params(config: SurvivalConfig, params: WeibullParams) -> Self {
        Self {
            config,
            params: RwLock::new(params),
            observations: RwLock::new(Vec::new()),
            shutdown: AtomicBool::new(false),
            observation_count: AtomicU64::new(0),
        }
    }

    /// Record a new observation and update model parameters.
    pub fn observe(&self, obs: Observation) {
        {
            let mut observations = self.observations.write().expect("obs lock poisoned");
            observations.push(obs);

            // Trim to max capacity (keep most recent)
            if observations.len() > self.config.max_observations {
                let excess = observations.len() - self.config.max_observations;
                observations.drain(0..excess);
            }
        }

        self.observation_count.fetch_add(1, Ordering::Relaxed);

        // Only update parameters if we have enough data
        if self.observation_count() as usize >= self.config.warmup_observations {
            self.update_parameters();
        }
    }

    /// Compute the current hazard rate given covariates and time.
    #[must_use]
    pub fn hazard_rate(&self, t: f64, covariates: &Covariates) -> f64 {
        if self.in_warmup() {
            return 0.0;
        }
        let params = self.params.read().expect("params lock poisoned");
        params.hazard(t, covariates)
    }

    /// Compute survival probability.
    #[must_use]
    pub fn survival_probability(&self, t: f64, covariates: &Covariates) -> f64 {
        if self.in_warmup() {
            return 1.0;
        }
        let params = self.params.read().expect("params lock poisoned");
        params.survival_probability(t, covariates)
    }

    /// Determine the recommended action for current hazard level.
    #[must_use]
    pub fn evaluate_action(&self, t: f64, covariates: &Covariates) -> HazardAction {
        let hazard = self.hazard_rate(t, covariates);
        self.classify_hazard(hazard)
    }

    /// Classify a hazard rate into an action.
    #[must_use]
    fn classify_hazard(&self, hazard: f64) -> HazardAction {
        if hazard >= self.config.alert_threshold {
            HazardAction::AlertAndPrepareRestart
        } else if hazard >= self.config.immediate_snapshot_threshold {
            HazardAction::ImmediateSnapshot
        } else if hazard >= self.config.snapshot_frequency_threshold {
            HazardAction::IncreaseSnapshotFrequency
        } else {
            HazardAction::None
        }
    }

    /// Produce a comprehensive hazard report.
    #[must_use]
    pub fn report(&self, t: f64, covariates: &Covariates) -> HazardReport {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());

        let params = self.params.read().expect("params lock poisoned").clone();
        let hazard = if self.in_warmup() {
            0.0
        } else {
            params.hazard(t, covariates)
        };
        let survival = if self.in_warmup() {
            1.0
        } else {
            params.survival_probability(t, covariates)
        };
        let failure = 1.0 - survival;
        let action = self.classify_hazard(hazard);

        // Build risk factor breakdown
        let x = covariates.to_array();
        let names = Covariates::names();
        let total_contribution: f64 = x
            .iter()
            .zip(params.beta.iter())
            .map(|(xi, bi)| (xi * bi).abs())
            .sum();

        let risk_factors: Vec<RiskFactor> = x
            .iter()
            .zip(params.beta.iter())
            .zip(names.iter())
            .map(|((xi, bi), name)| {
                let contribution = xi * bi;
                let risk_fraction = if total_contribution > 0.0 {
                    contribution.abs() / total_contribution
                } else {
                    0.0
                };
                RiskFactor {
                    name: name.to_string(),
                    value: *xi,
                    coefficient: *bi,
                    contribution,
                    risk_fraction,
                }
            })
            .collect();

        HazardReport {
            timestamp_secs: now,
            hazard_rate: hazard,
            survival_probability: survival,
            failure_probability: failure,
            action,
            risk_factors,
            params,
            in_warmup: self.in_warmup(),
            observation_count: self.observation_count() as usize,
        }
    }

    /// Whether the model is still in warmup phase.
    #[must_use]
    pub fn in_warmup(&self) -> bool {
        (self.observation_count() as usize) < self.config.warmup_observations
    }

    /// Total observations recorded.
    #[must_use]
    pub fn observation_count(&self) -> u64 {
        self.observation_count.load(Ordering::Relaxed)
    }

    /// Current model parameters.
    #[must_use]
    pub fn params(&self) -> WeibullParams {
        self.params.read().expect("params lock poisoned").clone()
    }

    /// Signal shutdown.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }

    /// Whether shutdown has been signaled.
    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Run the model update loop (call from async context).
    pub async fn run(&self) {
        let interval = self.config.update_interval.max(Duration::from_secs(1));
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;
            if self.shutdown.load(Ordering::SeqCst) {
                debug!("Survival model shutting down");
                break;
            }

            if !self.in_warmup() {
                self.update_parameters();
            }
        }
    }

    // ── Online parameter estimation ─────────────────────────────────────

    /// Update Weibull parameters via gradient ascent on the log-likelihood.
    ///
    /// Uses a single gradient step per call (online learning).
    fn update_parameters(&self) {
        let observations = self.observations.read().expect("obs lock poisoned");
        if observations.is_empty() {
            return;
        }

        let mut params = self.params.write().expect("params lock poisoned");
        let lr = self.config.learning_rate;

        // Compute gradient of log-likelihood w.r.t. beta
        let mut grad_beta = [0.0f64; Covariates::COUNT];
        let mut grad_log_shape = 0.0f64;
        let mut grad_log_scale = 0.0f64;

        let k = params.shape;
        let lam = params.scale;

        for obs in observations.iter() {
            let x = obs.covariates.to_array();
            let lin = obs.covariates.dot(&params.beta);
            let exp_lin = lin.exp();
            let t = obs.time.max(1e-6); // avoid log(0)
            let t_over_lam_k = (t / lam).powf(k);

            // Gradient of log-likelihood w.r.t. each beta_j:
            //   δ_j = event * x_j - t_over_lam^k * exp(β·x) * x_j
            for j in 0..Covariates::COUNT {
                let event_term = if obs.event_observed { x[j] } else { 0.0 };
                grad_beta[j] += (t_over_lam_k * exp_lin).mul_add(-x[j], event_term);
            }

            // Gradient w.r.t. log(k):
            //   event * (1 + k*ln(t/λ)) - k * ln(t/λ) * t_over_lam^k * exp(β·x)
            let ln_t_lam = (t / lam).ln();
            let event_k = if obs.event_observed {
                k.mul_add(ln_t_lam, 1.0)
            } else {
                0.0
            };
            grad_log_shape += (k * ln_t_lam * t_over_lam_k).mul_add(-exp_lin, event_k);

            // Gradient w.r.t. log(λ):
            //   event * (-k) + k * t_over_lam^k * exp(β·x)
            let event_lam = if obs.event_observed { -k } else { 0.0 };
            grad_log_scale += (k * t_over_lam_k).mul_add(exp_lin, event_lam);
        }

        // Normalize by observation count
        let n = observations.len() as f64;
        for g in &mut grad_beta {
            *g /= n;
        }
        grad_log_shape /= n;
        grad_log_scale /= n;

        // Gradient step for betas
        for (beta, grad) in params.beta.iter_mut().zip(grad_beta.iter()) {
            *beta += lr * *grad;
        }

        // Update shape and scale in log-space to maintain positivity
        let new_log_k = lr.mul_add(grad_log_shape, k.ln());
        let new_log_lam = lr.mul_add(grad_log_scale, lam.ln());

        // Clamp to reasonable ranges
        params.shape = new_log_k.exp().clamp(0.1, 10.0);
        params.scale = new_log_lam.exp().clamp(1.0, 10000.0);
    }
}

impl std::fmt::Debug for SurvivalModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SurvivalModel")
            .field("config", &self.config)
            .field("observation_count", &self.observation_count())
            .field("in_warmup", &self.in_warmup())
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Weibull parameters ---------------------------------------------------

    #[test]
    fn baseline_hazard_increases_with_time() {
        let params = WeibullParams {
            shape: 2.0, // k > 1 → increasing hazard
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        let h1 = params.baseline_hazard(10.0);
        let h2 = params.baseline_hazard(50.0);
        let h3 = params.baseline_hazard(100.0);
        assert!(h1 > 0.0);
        assert!(h2 > h1, "h2={h2} should be > h1={h1}");
        assert!(h3 > h2, "h3={h3} should be > h2={h2}");
    }

    #[test]
    fn baseline_hazard_constant_when_k_equals_1() {
        // k=1 → exponential distribution → constant hazard
        let params = WeibullParams {
            shape: 1.0,
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        let h1 = params.baseline_hazard(10.0);
        let h2 = params.baseline_hazard(50.0);
        assert!((h1 - h2).abs() < 1e-10, "constant hazard: h1={h1}, h2={h2}");
        assert!((h1 - 0.01).abs() < 1e-10, "h(t)=k/λ=1/100=0.01");
    }

    #[test]
    fn baseline_hazard_zero_at_negative_time() {
        let params = WeibullParams::default();
        assert_eq!(params.baseline_hazard(-1.0), 0.0);
        assert_eq!(params.baseline_hazard(0.0), 0.0);
    }

    #[test]
    fn covariates_increase_hazard() {
        let params = WeibullParams {
            shape: 1.5,
            scale: 168.0,
            beta: [0.5, 0.01, 0.1, 0.02, 0.3], // positive coefficients
        };
        let zero = Covariates::default();
        let risky = Covariates {
            rss_gb: 10.0,
            pane_count: 50.0,
            output_rate_mbps: 5.0,
            uptime_hours: 100.0,
            conn_error_rate: 2.0,
        };

        let h_zero = params.hazard(24.0, &zero);
        let h_risky = params.hazard(24.0, &risky);
        assert!(
            h_risky > h_zero,
            "risky={h_risky} should be > zero={h_zero}"
        );
    }

    #[test]
    fn survival_probability_decreases_with_time() {
        let params = WeibullParams::default();
        let cov = Covariates::default();
        let s1 = params.survival_probability(1.0, &cov);
        let s2 = params.survival_probability(10.0, &cov);
        let s3 = params.survival_probability(100.0, &cov);
        assert!(s1 > s2, "s1={s1} > s2={s2}");
        assert!(s2 > s3, "s2={s2} > s3={s3}");
        assert!(s1 <= 1.0 && s1 >= 0.0);
        assert!(s3 <= 1.0 && s3 >= 0.0);
    }

    #[test]
    fn survival_at_zero_is_one() {
        let params = WeibullParams::default();
        let cov = Covariates::default();
        let s = params.survival_probability(0.0, &cov);
        assert!((s - 1.0).abs() < 1e-10, "S(0) should be 1.0, got {s}");
    }

    #[test]
    fn failure_plus_survival_equals_one() {
        let params = WeibullParams::default();
        let cov = Covariates {
            rss_gb: 5.0,
            pane_count: 20.0,
            ..Default::default()
        };
        for t in [1.0, 10.0, 50.0, 100.0, 200.0] {
            let s = params.survival_probability(t, &cov);
            let f = params.failure_probability(t, &cov);
            assert!(
                (s + f - 1.0).abs() < 1e-10,
                "S({t})+F({t})={}, expected 1.0",
                s + f
            );
        }
    }

    #[test]
    fn cumulative_hazard_non_negative() {
        let params = WeibullParams::default();
        let cov = Covariates {
            rss_gb: 2.0,
            pane_count: 10.0,
            output_rate_mbps: 1.0,
            uptime_hours: 24.0,
            conn_error_rate: 0.5,
        };
        for t in [0.0, 1.0, 10.0, 100.0, 1000.0] {
            let h = params.cumulative_hazard(t, &cov);
            assert!(h >= 0.0, "H({t})={h} should be >= 0");
        }
    }

    // -- Covariates -----------------------------------------------------------

    #[test]
    fn covariates_dot_product() {
        let cov = Covariates {
            rss_gb: 2.0,
            pane_count: 3.0,
            output_rate_mbps: 4.0,
            uptime_hours: 5.0,
            conn_error_rate: 6.0,
        };
        let beta = [1.0, 2.0, 3.0, 4.0, 5.0];
        // 2*1 + 3*2 + 4*3 + 5*4 + 6*5 = 2 + 6 + 12 + 20 + 30 = 70
        assert!((cov.dot(&beta) - 70.0).abs() < 1e-10);
    }

    #[test]
    fn covariates_to_array_roundtrip() {
        let cov = Covariates {
            rss_gb: 1.0,
            pane_count: 2.0,
            output_rate_mbps: 3.0,
            uptime_hours: 4.0,
            conn_error_rate: 5.0,
        };
        let arr = cov.to_array();
        assert_eq!(arr, [1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn covariates_serde_roundtrip() {
        let cov = Covariates {
            rss_gb: 3.14,
            pane_count: 42.0,
            output_rate_mbps: 2.71,
            uptime_hours: 100.0,
            conn_error_rate: 0.5,
        };
        let json = serde_json::to_string(&cov).unwrap();
        let back: Covariates = serde_json::from_str(&json).unwrap();
        assert!((back.rss_gb - 3.14).abs() < 1e-10);
    }

    // -- Log-likelihood -------------------------------------------------------

    #[test]
    fn log_likelihood_observed_event() {
        let params = WeibullParams {
            shape: 2.0,
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        let obs = Observation {
            time: 50.0,
            event_observed: true,
            covariates: Covariates::default(),
            timestamp_secs: 0,
        };
        let ll = params.log_likelihood_single(&obs);
        // With zero betas and default covariates: exp(0) = 1
        // h(50) = (2/100)(50/100)^1 = 0.01
        // H(50) = (50/100)^2 = 0.25
        // ll = ln(0.01) - 0.25 = -4.605... - 0.25 = -4.855...
        assert!(ll.is_finite());
        assert!(ll < 0.0);
    }

    #[test]
    fn log_likelihood_censored() {
        let params = WeibullParams {
            shape: 2.0,
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        let obs = Observation {
            time: 50.0,
            event_observed: false,
            covariates: Covariates::default(),
            timestamp_secs: 0,
        };
        let ll = params.log_likelihood_single(&obs);
        // Censored: ll = -H(50) = -(50/100)^2 = -0.25
        assert!((ll - (-0.25)).abs() < 1e-10, "ll={ll}, expected -0.25");
    }

    // -- HazardAction ---------------------------------------------------------

    #[test]
    fn hazard_action_ordering() {
        assert!(HazardAction::None < HazardAction::IncreaseSnapshotFrequency);
        assert!(HazardAction::IncreaseSnapshotFrequency < HazardAction::ImmediateSnapshot);
        assert!(HazardAction::ImmediateSnapshot < HazardAction::AlertAndPrepareRestart);
    }

    #[test]
    fn hazard_action_display() {
        assert_eq!(HazardAction::None.to_string(), "none");
        assert_eq!(
            HazardAction::AlertAndPrepareRestart.to_string(),
            "alert_and_prepare_restart"
        );
    }

    // -- SurvivalModel --------------------------------------------------------

    #[test]
    fn model_warmup() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 5,
            ..Default::default()
        });
        assert!(model.in_warmup());
        assert_eq!(model.observation_count(), 0);

        // During warmup, hazard should be 0 and survival should be 1
        let cov = Covariates::default();
        assert_eq!(model.hazard_rate(10.0, &cov), 0.0);
        assert_eq!(model.survival_probability(10.0, &cov), 1.0);
        assert_eq!(model.evaluate_action(10.0, &cov), HazardAction::None);
    }

    #[test]
    fn model_exits_warmup_after_enough_observations() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 3,
            ..Default::default()
        });

        for i in 0..3 {
            model.observe(Observation {
                time: (i + 1) as f64 * 10.0,
                event_observed: false,
                covariates: Covariates::default(),
                timestamp_secs: 0,
            });
        }

        assert!(!model.in_warmup());
        assert_eq!(model.observation_count(), 3);
    }

    #[test]
    fn model_hazard_positive_after_warmup() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 2,
            learning_rate: 0.0, // don't update params, use defaults
            ..Default::default()
        });

        // Feed enough observations to exit warmup
        for i in 0..3 {
            model.observe(Observation {
                time: (i + 1) as f64 * 24.0,
                event_observed: false,
                covariates: Covariates::default(),
                timestamp_secs: 0,
            });
        }

        let cov = Covariates::default();
        let h = model.hazard_rate(48.0, &cov);
        assert!(h > 0.0, "hazard should be positive after warmup: {h}");
    }

    #[test]
    fn model_action_thresholds() {
        let config = SurvivalConfig {
            warmup_observations: 0,
            snapshot_frequency_threshold: 0.5,
            immediate_snapshot_threshold: 0.8,
            alert_threshold: 0.95,
            ..Default::default()
        };

        // Use custom params that produce known hazard values
        let model = SurvivalModel::with_params(
            config,
            WeibullParams {
                shape: 1.0,
                scale: 1.0,                      // h₀(t) = 1.0 for all t
                beta: [1.0, 0.0, 0.0, 0.0, 0.0], // only RSS matters
            },
        );

        // At RSS=0: h = 1.0 * exp(0) = 1.0 → AlertAndPrepareRestart
        let cov_zero = Covariates::default();
        assert_eq!(
            model.evaluate_action(1.0, &cov_zero),
            HazardAction::AlertAndPrepareRestart,
            "h=1.0 should trigger alert"
        );

        // Very low hazard with shape=1, scale=1000, zero covariates
        let model2 = SurvivalModel::with_params(
            SurvivalConfig {
                warmup_observations: 0,
                ..Default::default()
            },
            WeibullParams {
                shape: 1.0,
                scale: 1000.0, // h₀ = 0.001
                beta: [0.0; Covariates::COUNT],
            },
        );
        assert_eq!(
            model2.evaluate_action(1.0, &cov_zero),
            HazardAction::None,
            "h=0.001 should be None"
        );
    }

    #[test]
    fn model_report_structure() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 0,
            ..Default::default()
        });

        let cov = Covariates {
            rss_gb: 5.0,
            pane_count: 30.0,
            output_rate_mbps: 2.0,
            uptime_hours: 48.0,
            conn_error_rate: 0.1,
        };

        let report = model.report(48.0, &cov);
        assert!(report.timestamp_secs > 0);
        assert_eq!(report.risk_factors.len(), Covariates::COUNT);
        assert!(!report.in_warmup);

        // Check risk factor names match
        let names: Vec<&str> = report
            .risk_factors
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "rss_gb",
                "pane_count",
                "output_rate_mbps",
                "uptime_hours",
                "conn_error_rate"
            ]
        );
    }

    #[test]
    fn model_report_in_warmup() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 10,
            ..Default::default()
        });

        let report = model.report(10.0, &Covariates::default());
        assert!(report.in_warmup);
        assert_eq!(report.hazard_rate, 0.0);
        assert_eq!(report.survival_probability, 1.0);
        assert_eq!(report.action, HazardAction::None);
    }

    #[test]
    fn model_observation_trimming() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 0,
            max_observations: 5,
            learning_rate: 0.0,
            ..Default::default()
        });

        for i in 0..10 {
            model.observe(Observation {
                time: (i + 1) as f64,
                event_observed: false,
                covariates: Covariates::default(),
                timestamp_secs: i as u64,
            });
        }

        assert_eq!(model.observation_count(), 10);
        let obs = model.observations.read().unwrap();
        assert_eq!(obs.len(), 5); // trimmed to max
        assert_eq!(obs[0].timestamp_secs, 5); // oldest is obs #5
    }

    #[test]
    fn model_parameter_learning() {
        // Create model with non-zero learning rate
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 2,
            learning_rate: 0.01,
            max_observations: 100,
            ..Default::default()
        });

        let initial_params = model.params();

        // Feed failure observations with high RSS
        for i in 0..20 {
            model.observe(Observation {
                time: (i + 1) as f64 * 5.0,
                event_observed: i % 3 == 0, // some failures
                covariates: Covariates {
                    rss_gb: 10.0 + i as f64,
                    pane_count: 50.0,
                    ..Default::default()
                },
                timestamp_secs: i as u64,
            });
        }

        let updated_params = model.params();
        // Parameters should have changed from initial
        let beta_changed = initial_params
            .beta
            .iter()
            .zip(updated_params.beta.iter())
            .any(|(a, b)| (a - b).abs() > 1e-15);
        assert!(
            beta_changed,
            "beta should change after learning: initial={:?}, updated={:?}",
            initial_params.beta, updated_params.beta
        );
    }

    #[test]
    fn model_shutdown() {
        let model = SurvivalModel::new(SurvivalConfig::default());
        assert!(!model.is_shutdown());
        model.shutdown();
        assert!(model.is_shutdown());
    }

    // -- Configuration --------------------------------------------------------

    #[test]
    fn config_defaults() {
        let config = SurvivalConfig::default();
        assert_eq!(config.warmup_observations, 10);
        assert!((config.learning_rate - 0.01).abs() < 1e-10);
        assert!((config.snapshot_frequency_threshold - 0.5).abs() < 1e-10);
        assert!((config.immediate_snapshot_threshold - 0.8).abs() < 1e-10);
        assert!((config.alert_threshold - 0.95).abs() < 1e-10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = SurvivalConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let back: SurvivalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.warmup_observations, config.warmup_observations);
    }

    #[test]
    fn params_serde_roundtrip() {
        let params = WeibullParams {
            shape: 2.5,
            scale: 200.0,
            beta: [0.1, 0.2, 0.3, 0.4, 0.5],
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: WeibullParams = serde_json::from_str(&json).unwrap();
        assert!((back.shape - 2.5).abs() < 1e-10);
        assert_eq!(back.beta, [0.1, 0.2, 0.3, 0.4, 0.5]);
    }

    #[test]
    fn observation_serde_roundtrip() {
        let obs = Observation {
            time: 42.0,
            event_observed: true,
            covariates: Covariates {
                rss_gb: 5.0,
                ..Default::default()
            },
            timestamp_secs: 1700000000,
        };
        let json = serde_json::to_string(&obs).unwrap();
        let back: Observation = serde_json::from_str(&json).unwrap();
        assert!((back.time - 42.0).abs() < 1e-10);
        assert!(back.event_observed);
    }

    #[test]
    fn hazard_report_serde_roundtrip() {
        let report = HazardReport {
            timestamp_secs: 1700000000,
            hazard_rate: 0.75,
            survival_probability: 0.47,
            failure_probability: 0.53,
            action: HazardAction::ImmediateSnapshot,
            risk_factors: vec![RiskFactor {
                name: "rss_gb".to_string(),
                value: 8.0,
                coefficient: 0.5,
                contribution: 4.0,
                risk_fraction: 0.6,
            }],
            params: WeibullParams::default(),
            in_warmup: false,
            observation_count: 50,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: HazardReport = serde_json::from_str(&json).unwrap();
        assert!((back.hazard_rate - 0.75).abs() < 1e-10);
        assert_eq!(back.action, HazardAction::ImmediateSnapshot);
    }

    // -- Async run test -------------------------------------------------------

    #[tokio::test]
    async fn model_run_and_shutdown() {
        let model = Arc::new(SurvivalModel::new(SurvivalConfig {
            warmup_observations: 0,
            update_interval: Duration::from_millis(50),
            ..Default::default()
        }));

        // Add some observations
        for i in 0..5 {
            model.observe(Observation {
                time: (i + 1) as f64 * 10.0,
                event_observed: false,
                covariates: Covariates::default(),
                timestamp_secs: 0,
            });
        }

        let m = Arc::clone(&model);
        let handle = tokio::spawn(async move {
            m.run().await;
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        model.shutdown();
        handle.await.unwrap();
    }
}
