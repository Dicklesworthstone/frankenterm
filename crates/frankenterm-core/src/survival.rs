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

use crate::runtime_compat::sleep;
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
// Restart scheduling
// =============================================================================

/// Scheduling mode for automatic mux restart planning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RestartMode {
    /// Fully automatic scheduling; execute when score exceeds `min_score`.
    Automatic { min_score: f64 },
    /// Compute recommendations but do not execute.
    #[default]
    Advisory,
    /// Disable scheduler decisions (manual restart only).
    Manual,
}

/// Configuration for restart window selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestartSchedulerConfig {
    /// Scheduler mode (automatic/advisory/manual).
    pub mode: RestartMode,
    /// Hazard threshold used by urgency sigmoid.
    pub hazard_threshold: f64,
    /// Sigmoid steepness for hazard urgency.
    pub urgency_steepness: f64,
    /// Hard minimum time between restarts.
    pub cooldown_hours: f64,
    /// Planning horizon in minutes.
    pub schedule_horizon_minutes: u32,
    /// EWMA alpha for hourly activity profile learning.
    pub activity_ewma_alpha: f64,
    /// Initial activity level (0.0-1.0) for all hours before learning.
    pub default_activity: f64,
    /// Whether a pre-restart snapshot should be required.
    pub pre_restart_snapshot: bool,
    /// Advance warning lead time.
    pub advance_warning_minutes: u32,
}

impl Default for RestartSchedulerConfig {
    fn default() -> Self {
        Self {
            mode: RestartMode::Advisory,
            hazard_threshold: 0.8,
            urgency_steepness: 8.0,
            cooldown_hours: 12.0,
            schedule_horizon_minutes: 24 * 60,
            activity_ewma_alpha: 0.2,
            default_activity: 0.5,
            pre_restart_snapshot: true,
            advance_warning_minutes: 30,
        }
    }
}

/// 24-hour activity profile with per-hour EWMA buckets (UTC).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityProfile {
    alpha: f64,
    hourly_ewma: [f64; 24],
    sample_count: [u64; 24],
}

impl ActivityProfile {
    /// Create a new profile.
    #[must_use]
    pub fn new(alpha: f64, default_activity: f64) -> Self {
        let alpha = alpha.clamp(0.0, 1.0);
        let default_activity = default_activity.clamp(0.0, 1.0);
        Self {
            alpha,
            hourly_ewma: [default_activity; 24],
            sample_count: [0; 24],
        }
    }

    /// Update using a UTC timestamped activity sample.
    pub fn update(&mut self, observed_at: SystemTime, normalized_activity: f64) {
        let hour = hour_of_day_utc(observed_at);
        self.update_hour(hour, normalized_activity);
    }

    /// Update a specific hour bucket directly.
    pub fn update_hour(&mut self, hour: u8, normalized_activity: f64) {
        let index = usize::from(hour % 24);
        let sample = normalized_activity.clamp(0.0, 1.0);
        let prev = self.hourly_ewma[index];
        let next = if self.sample_count[index] == 0 {
            sample
        } else {
            self.alpha.mul_add(sample, (1.0 - self.alpha) * prev)
        };
        self.hourly_ewma[index] = next.clamp(0.0, 1.0);
        self.sample_count[index] = self.sample_count[index].saturating_add(1);
    }

    /// Predict activity level for a timestamp.
    #[must_use]
    pub fn predict(&self, at: SystemTime) -> f64 {
        self.predict_hour(hour_of_day_utc(at))
    }

    /// Predict activity for a UTC hour bucket.
    #[must_use]
    pub fn predict_hour(&self, hour: u8) -> f64 {
        self.hourly_ewma[usize::from(hour % 24)]
    }

    /// Number of samples ingested for a UTC hour bucket.
    #[must_use]
    pub fn sample_count(&self, hour: u8) -> u64 {
        self.sample_count[usize::from(hour % 24)]
    }

    /// Snapshot of hourly EWMA values.
    #[must_use]
    pub fn hourly_snapshot(&self) -> [f64; 24] {
        self.hourly_ewma
    }
}

/// Hazard forecast point for a candidate restart window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct HazardForecastPoint {
    /// Minutes from `now` to this candidate.
    pub offset_minutes: u32,
    /// Predicted hazard rate at candidate time.
    pub hazard_rate: f64,
    /// Optional precomputed activity estimate (0.0-1.0); if absent, use profile.
    pub predicted_activity: Option<f64>,
}

/// Detailed score components for restart candidate evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RestartScoreBreakdown {
    /// Sigmoid urgency from hazard rate.
    pub hazard_urgency: f64,
    /// Inverse normalized activity (prefer low activity windows).
    pub activity_minimum: f64,
    /// Cooldown-sensitive recency factor.
    pub recency_penalty: f64,
    /// Final composite score.
    pub score: f64,
}

/// Scheduler recommendation for a restart window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestartRecommendation {
    /// Recommended execution timestamp.
    pub scheduled_for_epoch_secs: u64,
    /// Candidate offset from evaluation time.
    pub offset_minutes: u32,
    /// Predicted hazard at selected time.
    pub hazard_rate: f64,
    /// Predicted activity at selected time.
    pub predicted_activity: f64,
    /// Scoring breakdown.
    pub breakdown: RestartScoreBreakdown,
    /// Whether this recommendation should execute automatically.
    pub should_execute_automatically: bool,
    /// Optional warning timestamp.
    pub warning_epoch_secs: Option<u64>,
    /// Optional pre-restart snapshot timestamp.
    pub snapshot_epoch_secs: Option<u64>,
}

/// Restart scheduler combining hazard urgency and activity minima.
#[derive(Debug, Clone)]
pub struct RestartScheduler {
    config: RestartSchedulerConfig,
    activity_profile: ActivityProfile,
    last_restart_at: Option<SystemTime>,
}

impl RestartScheduler {
    /// Create a new scheduler with learned activity profile support.
    #[must_use]
    pub fn new(config: RestartSchedulerConfig) -> Self {
        let activity_profile =
            ActivityProfile::new(config.activity_ewma_alpha, config.default_activity);
        Self {
            config,
            activity_profile,
            last_restart_at: None,
        }
    }

    /// Immutable scheduler configuration.
    #[must_use]
    pub fn config(&self) -> &RestartSchedulerConfig {
        &self.config
    }

    /// Immutable access to the activity profile.
    #[must_use]
    pub fn activity_profile(&self) -> &ActivityProfile {
        &self.activity_profile
    }

    /// Mutable access to the activity profile.
    pub fn activity_profile_mut(&mut self) -> &mut ActivityProfile {
        &mut self.activity_profile
    }

    /// Update activity profile from observed load.
    pub fn record_activity(&mut self, observed_at: SystemTime, normalized_activity: f64) {
        self.activity_profile
            .update(observed_at, normalized_activity);
    }

    /// Set the last known restart timestamp.
    pub fn set_last_restart_at(&mut self, at: Option<SystemTime>) {
        self.last_restart_at = at;
    }

    /// Return the last known restart timestamp.
    #[must_use]
    pub fn last_restart_at(&self) -> Option<SystemTime> {
        self.last_restart_at
    }

    /// Record that a restart just happened at `at`.
    pub fn record_restart(&mut self, at: SystemTime) {
        self.last_restart_at = Some(at);
    }

    /// Compute score components for a candidate restart window.
    #[must_use]
    pub fn score_components(
        &self,
        hazard_rate: f64,
        predicted_activity: f64,
        elapsed_since_last_restart: Option<Duration>,
    ) -> RestartScoreBreakdown {
        let steepness = self.config.urgency_steepness.max(1e-6);
        let hazard_urgency = sigmoid((hazard_rate - self.config.hazard_threshold) * steepness);
        let activity_minimum = (1.0 - predicted_activity.clamp(0.0, 1.0)).clamp(0.0, 1.0);

        let recency_penalty = if let Some(elapsed) = elapsed_since_last_restart {
            let cooldown_hours = self.config.cooldown_hours.max(1e-6);
            let elapsed_hours = elapsed.as_secs_f64() / 3600.0;
            (1.0 - (-(elapsed_hours / cooldown_hours)).exp()).clamp(0.0, 1.0)
        } else {
            1.0
        };

        let score = (hazard_urgency * activity_minimum * recency_penalty).clamp(0.0, 1.0);
        RestartScoreBreakdown {
            hazard_urgency,
            activity_minimum,
            recency_penalty,
            score,
        }
    }

    /// Recommend the best restart window from forecast points.
    #[must_use]
    pub fn recommend(
        &self,
        now: SystemTime,
        forecast: &[HazardForecastPoint],
    ) -> Option<RestartRecommendation> {
        if forecast.is_empty() || matches!(self.config.mode, RestartMode::Manual) {
            return None;
        }

        let mut best: Option<(HazardForecastPoint, SystemTime, RestartScoreBreakdown, f64)> = None;

        for point in forecast {
            if point.offset_minutes > self.config.schedule_horizon_minutes {
                continue;
            }

            let candidate_at =
                now.checked_add(Duration::from_secs(u64::from(point.offset_minutes) * 60))?;
            if !self.is_candidate_eligible(candidate_at) {
                continue;
            }

            let activity = point
                .predicted_activity
                .unwrap_or_else(|| self.activity_profile.predict(candidate_at))
                .clamp(0.0, 1.0);
            let elapsed = self
                .last_restart_at
                .and_then(|last| candidate_at.duration_since(last).ok());
            let breakdown = self.score_components(point.hazard_rate, activity, elapsed);

            match best {
                None => {
                    best = Some((*point, candidate_at, breakdown, activity));
                }
                Some((best_point, _, best_breakdown, _)) => {
                    let better_score = breakdown.score > best_breakdown.score + f64::EPSILON;
                    let tie_break_earlier = (breakdown.score - best_breakdown.score).abs()
                        <= f64::EPSILON
                        && point.offset_minutes < best_point.offset_minutes;
                    if better_score || tie_break_earlier {
                        best = Some((*point, candidate_at, breakdown, activity));
                    }
                }
            }
        }

        let (point, candidate_at, breakdown, activity) = best?;
        let scheduled_for_epoch_secs = epoch_secs(candidate_at)?;
        let should_execute_automatically = match self.config.mode {
            RestartMode::Automatic { min_score } => breakdown.score >= min_score,
            RestartMode::Advisory | RestartMode::Manual => false,
        };

        let warning_epoch_secs = if self.config.advance_warning_minutes == 0 {
            None
        } else {
            candidate_at
                .checked_sub(Duration::from_secs(
                    u64::from(self.config.advance_warning_minutes) * 60,
                ))
                .and_then(epoch_secs)
        };

        let snapshot_epoch_secs = self
            .config
            .pre_restart_snapshot
            .then_some(scheduled_for_epoch_secs);

        Some(RestartRecommendation {
            scheduled_for_epoch_secs,
            offset_minutes: point.offset_minutes,
            hazard_rate: point.hazard_rate,
            predicted_activity: activity,
            breakdown,
            should_execute_automatically,
            warning_epoch_secs,
            snapshot_epoch_secs,
        })
    }

    fn cooldown_duration(&self) -> Duration {
        Duration::from_secs_f64(self.config.cooldown_hours.max(0.0) * 3600.0)
    }

    fn is_candidate_eligible(&self, candidate_at: SystemTime) -> bool {
        let Some(last_restart) = self.last_restart_at else {
            return true;
        };

        let cooldown = self.cooldown_duration();
        if cooldown.is_zero() {
            return true;
        }

        match last_restart.checked_add(cooldown) {
            Some(next_allowed) => candidate_at >= next_allowed,
            None => false,
        }
    }
}

#[must_use]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

#[must_use]
fn hour_of_day_utc(at: SystemTime) -> u8 {
    let secs = at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    ((secs / 3600) % 24) as u8
}

#[must_use]
fn epoch_secs(at: SystemTime) -> Option<u64> {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
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
            let mut observations = self.observations.write().unwrap_or_else(|e| e.into_inner());
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
        let params = self.params.read().unwrap_or_else(|e| e.into_inner());
        params.hazard(t, covariates)
    }

    /// Compute survival probability.
    #[must_use]
    pub fn survival_probability(&self, t: f64, covariates: &Covariates) -> f64 {
        if self.in_warmup() {
            return 1.0;
        }
        let params = self.params.read().unwrap_or_else(|e| e.into_inner());
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

        let params = self.params.read().unwrap_or_else(|e| e.into_inner()).clone();
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
        self.params.read().unwrap_or_else(|e| e.into_inner()).clone()
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
        let mut first_tick = true;

        loop {
            if !first_tick {
                sleep(interval).await;
            }
            first_tick = false;

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
        let observations = self.observations.read().unwrap_or_else(|e| e.into_inner());
        if observations.is_empty() {
            return;
        }

        let mut params = self.params.write().unwrap_or_else(|e| e.into_inner());
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
    #![allow(clippy::float_cmp)]

    use super::*;
    use proptest::prelude::*;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build current-thread runtime");
        crate::runtime_compat::CompatRuntime::block_on(&runtime, future);
    }

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
        assert!((0.0..=1.0).contains(&s1));
        assert!((0.0..=1.0).contains(&s3));
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
            rss_gb: 3.25,
            pane_count: 42.0,
            output_rate_mbps: 2.71,
            uptime_hours: 100.0,
            conn_error_rate: 0.5,
        };
        let json = serde_json::to_string(&cov).unwrap();
        let back: Covariates = serde_json::from_str(&json).unwrap();
        assert!((back.rss_gb - 3.25).abs() < 1e-10);
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

    // -- Restart scheduler ----------------------------------------------------

    fn scheduler_config(mode: RestartMode) -> RestartSchedulerConfig {
        RestartSchedulerConfig {
            mode,
            ..RestartSchedulerConfig::default()
        }
    }

    #[test]
    fn restart_scheduler_prefers_high_hazard_low_activity() {
        let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Advisory));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2 * 3600);
        let forecast = vec![
            HazardForecastPoint {
                offset_minutes: 60,
                hazard_rate: 0.9,
                predicted_activity: Some(0.8),
            },
            HazardForecastPoint {
                offset_minutes: 120,
                hazard_rate: 1.2,
                predicted_activity: Some(0.1),
            },
        ];

        let recommendation = scheduler.recommend(now, &forecast).expect("recommendation");
        assert_eq!(recommendation.offset_minutes, 120);
        assert!(recommendation.breakdown.score > 0.0);
        assert!(!recommendation.should_execute_automatically);
    }

    #[test]
    fn restart_scheduler_enforces_cooldown() {
        let mut scheduler =
            RestartScheduler::new(scheduler_config(RestartMode::Automatic { min_score: 0.0 }));
        scheduler.set_last_restart_at(Some(SystemTime::UNIX_EPOCH + Duration::from_secs(3600)));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2 * 3600);
        let forecast = vec![
            HazardForecastPoint {
                offset_minutes: 30,
                hazard_rate: 1.3,
                predicted_activity: Some(0.0),
            },
            HazardForecastPoint {
                offset_minutes: 11 * 60,
                hazard_rate: 1.3,
                predicted_activity: Some(0.0),
            },
            HazardForecastPoint {
                offset_minutes: 12 * 60,
                hazard_rate: 1.3,
                predicted_activity: Some(0.0),
            },
        ];

        let recommendation = scheduler.recommend(now, &forecast).expect("recommendation");
        assert_eq!(recommendation.offset_minutes, 12 * 60);
    }

    #[test]
    fn restart_scheduler_manual_mode_returns_none() {
        let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Manual));
        let now = SystemTime::UNIX_EPOCH;
        let forecast = vec![HazardForecastPoint {
            offset_minutes: 10,
            hazard_rate: 2.0,
            predicted_activity: Some(0.0),
        }];
        assert!(scheduler.recommend(now, &forecast).is_none());
    }

    #[test]
    fn restart_scheduler_advisory_and_automatic_choose_same_window() {
        let forecast = vec![
            HazardForecastPoint {
                offset_minutes: 15,
                hazard_rate: 0.9,
                predicted_activity: Some(0.4),
            },
            HazardForecastPoint {
                offset_minutes: 45,
                hazard_rate: 1.1,
                predicted_activity: Some(0.2),
            },
        ];
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(9 * 3600);

        let advisory = RestartScheduler::new(scheduler_config(RestartMode::Advisory))
            .recommend(now, &forecast)
            .expect("advisory recommendation");
        let automatic =
            RestartScheduler::new(scheduler_config(RestartMode::Automatic { min_score: 0.0 }))
                .recommend(now, &forecast)
                .expect("automatic recommendation");

        assert_eq!(advisory.offset_minutes, automatic.offset_minutes);
    }

    #[test]
    fn activity_profile_updates_hourly_ewma() {
        let mut profile = ActivityProfile::new(0.5, 0.2);
        profile.update_hour(3, 0.8);
        profile.update_hour(3, 0.6);
        // first sample seeds 0.8, second sample EWMA(0.5)=0.7
        assert!((profile.predict_hour(3) - 0.7).abs() < 1e-10);
        assert_eq!(profile.sample_count(3), 2);
    }

    proptest! {
        #[test]
        fn restart_score_monotonic_in_hazard_for_equal_activity(
            hazard_a in 0.0f64..2.0,
            hazard_b in 0.0f64..2.0,
            activity in 0.0f64..1.0
        ) {
            let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Advisory));
            let (low, high) = if hazard_a <= hazard_b {
                (hazard_a, hazard_b)
            } else {
                (hazard_b, hazard_a)
            };
            let elapsed = Some(Duration::from_secs((24.0 * 3600.0) as u64));
            let low_score = scheduler
                .score_components(low, activity, elapsed)
                .score;
            let high_score = scheduler
                .score_components(high, activity, elapsed)
                .score;
            prop_assert!(high_score + 1e-12 >= low_score);
        }
    }

    proptest! {
        #[test]
        fn restart_recommendation_matches_bruteforce_argmax(
            points in prop::collection::vec((0.0f64..2.0, 0.0f64..1.0), 1..24)
        ) {
            let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Automatic { min_score: 0.0 }));
            let now = SystemTime::UNIX_EPOCH + Duration::from_secs(12 * 3600);
            let forecast: Vec<HazardForecastPoint> = points
                .iter()
                .enumerate()
                .map(|(idx, (hazard, activity))| HazardForecastPoint {
                    offset_minutes: (idx as u32) * 30,
                    hazard_rate: *hazard,
                    predicted_activity: Some(*activity),
                })
                .collect();

            let recommendation = scheduler.recommend(now, &forecast).expect("recommendation");

            let mut best_offset = 0u32;
            let mut best_score = f64::NEG_INFINITY;
            for point in &forecast {
                let score = scheduler
                    .score_components(
                        point.hazard_rate,
                        point.predicted_activity.unwrap_or(0.5),
                        None
                    )
                    .score;
                let better = score > best_score + f64::EPSILON;
                let tie_break = (score - best_score).abs() <= f64::EPSILON
                    && point.offset_minutes < best_offset;
                if better || tie_break {
                    best_offset = point.offset_minutes;
                    best_score = score;
                }
            }

            prop_assert_eq!(recommendation.offset_minutes, best_offset);
        }
    }

    proptest! {
        #[test]
        fn restart_scheduler_cooldown_never_violated(
            deltas in prop::collection::vec(1u32..180u32, 1..40)
        ) {
            let mut scheduler = RestartScheduler::new(RestartSchedulerConfig {
                mode: RestartMode::Automatic { min_score: 0.0 },
                cooldown_hours: 6.0,
                ..RestartSchedulerConfig::default()
            });

            let forecast = vec![HazardForecastPoint {
                offset_minutes: 0,
                hazard_rate: 2.0,
                predicted_activity: Some(0.0),
            }];

            let mut now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
            let mut restart_times = Vec::new();

            for delta_minutes in deltas {
                now = now
                    .checked_add(Duration::from_secs(u64::from(delta_minutes) * 60))
                    .expect("time overflow");

                if let Some(rec) = scheduler.recommend(now, &forecast)
                    && rec.should_execute_automatically
                {
                    let scheduled_at = SystemTime::UNIX_EPOCH
                        .checked_add(Duration::from_secs(rec.scheduled_for_epoch_secs))
                        .expect("time overflow");
                    scheduler.record_restart(scheduled_at);
                    restart_times.push(rec.scheduled_for_epoch_secs);
                }
            }

            let cooldown_secs = (6.0 * 3600.0) as u64;
            for pair in restart_times.windows(2) {
                prop_assert!(pair[1] >= pair[0] + cooldown_secs);
            }
        }
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

    #[test]
    fn model_run_and_shutdown() {
        run_async_test(async {
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
            let handle = crate::runtime_compat::task::spawn(async move {
                m.run().await;
            });

            crate::runtime_compat::sleep(Duration::from_millis(200)).await;
            model.shutdown();
            handle.await.unwrap();
        });
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    #[test]
    fn covariates_names_returns_correct_labels() {
        let names = Covariates::names();
        assert_eq!(names.len(), Covariates::COUNT);
        assert_eq!(names[0], "rss_gb");
        assert_eq!(names[1], "pane_count");
        assert_eq!(names[2], "output_rate_mbps");
        assert_eq!(names[3], "uptime_hours");
        assert_eq!(names[4], "conn_error_rate");
    }

    #[test]
    fn covariates_dot_with_zero_beta() {
        let cov = Covariates {
            rss_gb: 99.0,
            pane_count: 50.0,
            output_rate_mbps: 10.0,
            uptime_hours: 1000.0,
            conn_error_rate: 5.0,
        };
        let zero_beta = [0.0; Covariates::COUNT];
        assert!((cov.dot(&zero_beta)).abs() < 1e-15);
    }

    #[test]
    fn covariates_default_is_all_zeros() {
        let cov = Covariates::default();
        let arr = cov.to_array();
        for val in &arr {
            assert!(val.abs() < 1e-15, "expected 0.0 got {}", val);
        }
    }

    #[test]
    fn weibull_params_default_values() {
        let params = WeibullParams::default();
        assert!((params.shape - 1.5).abs() < 1e-10);
        assert!((params.scale - 168.0).abs() < 1e-10);
        for b in &params.beta {
            assert!(b.abs() < 1e-15, "default beta should be 0, got {}", b);
        }
    }

    #[test]
    fn baseline_hazard_zero_for_zero_scale() {
        let params = WeibullParams {
            shape: 2.0,
            scale: 0.0,
            beta: [0.0; Covariates::COUNT],
        };
        assert_eq!(params.baseline_hazard(10.0), 0.0);
    }

    #[test]
    fn baseline_hazard_zero_for_negative_shape() {
        let params = WeibullParams {
            shape: -1.0,
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        assert_eq!(params.baseline_hazard(10.0), 0.0);
    }

    #[test]
    fn cumulative_hazard_zero_at_t_zero() {
        let params = WeibullParams {
            shape: 2.0,
            scale: 100.0,
            beta: [0.1, 0.2, 0.0, 0.0, 0.0],
        };
        let cov = Covariates {
            rss_gb: 5.0,
            pane_count: 10.0,
            ..Default::default()
        };
        assert_eq!(params.cumulative_hazard(0.0, &cov), 0.0);
        assert_eq!(params.cumulative_hazard(-5.0, &cov), 0.0);
    }

    #[test]
    fn failure_probability_increases_with_time() {
        let params = WeibullParams::default();
        let cov = Covariates::default();
        let f1 = params.failure_probability(1.0, &cov);
        let f2 = params.failure_probability(50.0, &cov);
        let f3 = params.failure_probability(200.0, &cov);
        assert!(f1 < f2, "F(1)={} should be < F(50)={}", f1, f2);
        assert!(f2 < f3, "F(50)={} should be < F(200)={}", f2, f3);
        assert!((0.0..=1.0).contains(&f1));
        assert!((0.0..=1.0).contains(&f3));
    }

    #[test]
    fn log_likelihood_at_zero_time_event_is_neg_infinity() {
        // t=0 → baseline_hazard=0 → hazard=0 → event branch returns NEG_INFINITY
        let params = WeibullParams {
            shape: 2.0,
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        let obs = Observation {
            time: 0.0,
            event_observed: true,
            covariates: Covariates::default(),
            timestamp_secs: 0,
        };
        let ll = params.log_likelihood_single(&obs);
        assert!(
            ll.is_infinite() && ll < 0.0,
            "expected NEG_INFINITY, got {}",
            ll
        );
    }

    #[test]
    fn hazard_action_display_all_variants() {
        assert_eq!(HazardAction::None.to_string(), "none");
        assert_eq!(
            HazardAction::IncreaseSnapshotFrequency.to_string(),
            "increase_snapshot_frequency"
        );
        assert_eq!(
            HazardAction::ImmediateSnapshot.to_string(),
            "immediate_snapshot"
        );
        assert_eq!(
            HazardAction::AlertAndPrepareRestart.to_string(),
            "alert_and_prepare_restart"
        );
    }

    #[test]
    fn hazard_action_serde_roundtrip() {
        for action in [
            HazardAction::None,
            HazardAction::IncreaseSnapshotFrequency,
            HazardAction::ImmediateSnapshot,
            HazardAction::AlertAndPrepareRestart,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let back: HazardAction = serde_json::from_str(&json).unwrap();
            assert_eq!(back, action, "roundtrip failed for {:?}", action);
        }
    }

    #[test]
    fn restart_mode_default_is_advisory() {
        let mode = RestartMode::default();
        assert_eq!(mode, RestartMode::Advisory);
    }

    #[test]
    fn restart_scheduler_config_defaults() {
        let cfg = RestartSchedulerConfig::default();
        assert_eq!(cfg.mode, RestartMode::Advisory);
        assert!((cfg.hazard_threshold - 0.8).abs() < 1e-10);
        assert!((cfg.urgency_steepness - 8.0).abs() < 1e-10);
        assert!((cfg.cooldown_hours - 12.0).abs() < 1e-10);
        assert_eq!(cfg.schedule_horizon_minutes, 24 * 60);
        assert!((cfg.activity_ewma_alpha - 0.2).abs() < 1e-10);
        assert!((cfg.default_activity - 0.5).abs() < 1e-10);
        assert!(cfg.pre_restart_snapshot);
        assert_eq!(cfg.advance_warning_minutes, 30);
    }

    #[test]
    fn activity_profile_alpha_clamping() {
        let profile_high = ActivityProfile::new(5.0, 0.5);
        // alpha should be clamped to 1.0
        // With alpha=1.0: new sample completely replaces old
        let _ = profile_high.predict_hour(0); // just ensure it doesn't panic

        let profile_low = ActivityProfile::new(-2.0, 0.5);
        // alpha should be clamped to 0.0
        let _ = profile_low.predict_hour(0);

        // default_activity clamping
        let profile_act = ActivityProfile::new(0.5, 2.0);
        // should be clamped to 1.0
        assert!((profile_act.predict_hour(0) - 1.0).abs() < 1e-10);

        let profile_act_neg = ActivityProfile::new(0.5, -1.0);
        // should be clamped to 0.0
        assert!((profile_act_neg.predict_hour(0)).abs() < 1e-10);
    }

    #[test]
    fn activity_profile_hourly_snapshot_returns_all_24() {
        let mut profile = ActivityProfile::new(0.3, 0.4);
        profile.update_hour(5, 0.9);
        profile.update_hour(23, 0.1);
        let snap = profile.hourly_snapshot();
        assert_eq!(snap.len(), 24);
        // hour 5 was updated: first sample seeds to 0.9
        assert!((snap[5] - 0.9).abs() < 1e-10);
        // hour 23 was updated: first sample seeds to 0.1
        assert!((snap[23] - 0.1).abs() < 1e-10);
        // untouched hours stay at default 0.4
        assert!((snap[0] - 0.4).abs() < 1e-10);
    }

    #[test]
    fn activity_profile_update_via_system_time() {
        let mut profile = ActivityProfile::new(0.5, 0.3);
        // 7200 seconds = 2 hours from epoch → hour 2 UTC
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(7200);
        profile.update(at, 0.8);
        assert_eq!(profile.sample_count(2), 1);
        assert!((profile.predict_hour(2) - 0.8).abs() < 1e-10);
    }

    #[test]
    fn activity_profile_predict_via_system_time() {
        let mut profile = ActivityProfile::new(0.5, 0.5);
        profile.update_hour(14, 0.9);
        // 14 * 3600 = 50400 seconds from epoch → hour 14 UTC
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(14 * 3600);
        assert!((profile.predict(at) - 0.9).abs() < 1e-10);
    }

    #[test]
    fn restart_scheduler_accessors() {
        let cfg = RestartSchedulerConfig {
            cooldown_hours: 8.0,
            ..RestartSchedulerConfig::default()
        };
        let mut scheduler = RestartScheduler::new(cfg.clone());

        // config() accessor
        assert!((scheduler.config().cooldown_hours - 8.0).abs() < 1e-10);

        // last_restart_at() starts as None
        assert!(scheduler.last_restart_at().is_none());

        // record_restart sets it
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        scheduler.record_restart(ts);
        assert_eq!(scheduler.last_restart_at(), Some(ts));

        // activity_profile() / activity_profile_mut()
        let profile = scheduler.activity_profile();
        assert!((profile.predict_hour(0) - 0.5).abs() < 1e-10);

        scheduler.activity_profile_mut().update_hour(12, 0.7);
        assert!((scheduler.activity_profile().predict_hour(12) - 0.7).abs() < 1e-10);
    }

    #[test]
    fn restart_scheduler_record_activity() {
        let mut scheduler = RestartScheduler::new(RestartSchedulerConfig::default());
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(3 * 3600); // hour 3
        scheduler.record_activity(at, 0.95);
        assert!((scheduler.activity_profile().predict_hour(3) - 0.95).abs() < 1e-10);
    }

    #[test]
    fn restart_scheduler_empty_forecast_returns_none() {
        let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Advisory));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        assert!(scheduler.recommend(now, &[]).is_none());
    }

    #[test]
    fn restart_scheduler_beyond_horizon_skipped() {
        let scheduler = RestartScheduler::new(RestartSchedulerConfig {
            mode: RestartMode::Advisory,
            schedule_horizon_minutes: 60, // only 1 hour
            ..RestartSchedulerConfig::default()
        });
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        let forecast = vec![HazardForecastPoint {
            offset_minutes: 120, // 2 hours — beyond horizon
            hazard_rate: 2.0,
            predicted_activity: Some(0.0),
        }];
        // All candidates beyond horizon → None
        assert!(scheduler.recommend(now, &forecast).is_none());
    }

    #[test]
    fn score_components_no_elapsed_gives_full_recency() {
        let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Advisory));
        let breakdown = scheduler.score_components(1.5, 0.0, None);
        // With elapsed=None, recency_penalty should be 1.0
        assert!(
            (breakdown.recency_penalty - 1.0).abs() < 1e-10,
            "expected recency=1.0, got {}",
            breakdown.recency_penalty
        );
    }

    #[test]
    fn score_components_zero_elapsed_gives_zero_recency() {
        let scheduler = RestartScheduler::new(scheduler_config(RestartMode::Advisory));
        let breakdown = scheduler.score_components(1.5, 0.0, Some(Duration::ZERO));
        // With elapsed=0, recency_penalty should be ~0
        assert!(
            breakdown.recency_penalty < 1e-6,
            "expected recency near 0, got {}",
            breakdown.recency_penalty
        );
    }

    #[test]
    fn model_with_params_uses_custom_params() {
        let custom = WeibullParams {
            shape: 3.0,
            scale: 50.0,
            beta: [0.1, 0.2, 0.3, 0.4, 0.5],
        };
        let model = SurvivalModel::with_params(
            SurvivalConfig {
                warmup_observations: 0,
                ..Default::default()
            },
            custom.clone(),
        );
        let p = model.params();
        assert!((p.shape - 3.0).abs() < 1e-10);
        assert!((p.scale - 50.0).abs() < 1e-10);
        assert_eq!(p.beta, [0.1, 0.2, 0.3, 0.4, 0.5]);
    }

    #[test]
    fn model_debug_format() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 5,
            ..Default::default()
        });
        let debug_str = format!("{:?}", model);
        assert!(debug_str.contains("SurvivalModel"));
        assert!(debug_str.contains("observation_count"));
        assert!(debug_str.contains("in_warmup"));
    }

    #[test]
    fn sigmoid_properties() {
        // sigmoid(0) = 0.5
        let s0 = sigmoid(0.0);
        assert!(
            (s0 - 0.5).abs() < 1e-10,
            "sigmoid(0) should be 0.5, got {}",
            s0
        );

        // sigmoid is monotonically increasing
        let s_neg = sigmoid(-5.0);
        let s_pos = sigmoid(5.0);
        assert!(s_neg < s0, "sigmoid(-5) should be < sigmoid(0)");
        assert!(s_pos > s0, "sigmoid(5) should be > sigmoid(0)");

        // sigmoid output in (0,1)
        assert!(s_neg > 0.0);
        assert!(s_pos < 1.0);

        // sigmoid symmetry: sigmoid(x) + sigmoid(-x) = 1
        for x in [0.5, 1.0, 3.0, 10.0] {
            let sum = sigmoid(x) + sigmoid(-x);
            assert!((sum - 1.0).abs() < 1e-10, "symmetry failed at x={}", x);
        }
    }

    #[test]
    fn hour_of_day_utc_known_values() {
        // UNIX_EPOCH = hour 0
        assert_eq!(hour_of_day_utc(SystemTime::UNIX_EPOCH), 0);

        // 3600s = 1 hour
        let h1 = SystemTime::UNIX_EPOCH + Duration::from_secs(3600);
        assert_eq!(hour_of_day_utc(h1), 1);

        // 23 * 3600 = hour 23
        let h23 = SystemTime::UNIX_EPOCH + Duration::from_secs(23 * 3600);
        assert_eq!(hour_of_day_utc(h23), 23);

        // 24 * 3600 wraps to hour 0
        let h24 = SystemTime::UNIX_EPOCH + Duration::from_secs(24 * 3600);
        assert_eq!(hour_of_day_utc(h24), 0);

        // 25 * 3600 = hour 1 next day
        let h25 = SystemTime::UNIX_EPOCH + Duration::from_secs(25 * 3600);
        assert_eq!(hour_of_day_utc(h25), 1);
    }

    #[test]
    fn epoch_secs_known_values() {
        assert_eq!(epoch_secs(SystemTime::UNIX_EPOCH), Some(0));
        let ts = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(epoch_secs(ts), Some(1_700_000_000));
    }

    #[test]
    fn restart_recommendation_warning_and_snapshot_fields() {
        let cfg = RestartSchedulerConfig {
            mode: RestartMode::Automatic { min_score: 0.0 },
            pre_restart_snapshot: true,
            advance_warning_minutes: 15,
            cooldown_hours: 0.0,
            ..RestartSchedulerConfig::default()
        };
        let scheduler = RestartScheduler::new(cfg);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let forecast = vec![HazardForecastPoint {
            offset_minutes: 60,
            hazard_rate: 1.5,
            predicted_activity: Some(0.1),
        }];
        let rec = scheduler.recommend(now, &forecast).expect("recommendation");
        // snapshot_epoch_secs should equal scheduled time when pre_restart_snapshot=true
        assert_eq!(rec.snapshot_epoch_secs, Some(rec.scheduled_for_epoch_secs));
        // warning should be scheduled_for - 15 minutes
        let expected_warning = rec.scheduled_for_epoch_secs - 15 * 60;
        assert_eq!(rec.warning_epoch_secs, Some(expected_warning));
        assert!(rec.should_execute_automatically);
    }

    #[test]
    fn restart_recommendation_no_warning_when_zero_advance() {
        let cfg = RestartSchedulerConfig {
            mode: RestartMode::Advisory,
            advance_warning_minutes: 0,
            cooldown_hours: 0.0,
            ..RestartSchedulerConfig::default()
        };
        let scheduler = RestartScheduler::new(cfg);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let forecast = vec![HazardForecastPoint {
            offset_minutes: 30,
            hazard_rate: 1.0,
            predicted_activity: Some(0.2),
        }];
        let rec = scheduler.recommend(now, &forecast).expect("recommendation");
        assert_eq!(rec.warning_epoch_secs, None);
    }

    #[test]
    fn restart_recommendation_no_snapshot_when_disabled() {
        let cfg = RestartSchedulerConfig {
            mode: RestartMode::Advisory,
            pre_restart_snapshot: false,
            cooldown_hours: 0.0,
            ..RestartSchedulerConfig::default()
        };
        let scheduler = RestartScheduler::new(cfg);
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100_000);
        let forecast = vec![HazardForecastPoint {
            offset_minutes: 30,
            hazard_rate: 1.0,
            predicted_activity: Some(0.2),
        }];
        let rec = scheduler.recommend(now, &forecast).expect("recommendation");
        assert_eq!(rec.snapshot_epoch_secs, None);
    }

    #[test]
    fn model_report_risk_fractions_sum_to_one_with_nonzero_beta() {
        let model = SurvivalModel::with_params(
            SurvivalConfig {
                warmup_observations: 0,
                ..Default::default()
            },
            WeibullParams {
                shape: 1.5,
                scale: 168.0,
                beta: [0.5, 0.1, 0.2, 0.05, 0.3],
            },
        );
        let cov = Covariates {
            rss_gb: 4.0,
            pane_count: 20.0,
            output_rate_mbps: 2.0,
            uptime_hours: 50.0,
            conn_error_rate: 1.0,
        };
        let report = model.report(24.0, &cov);
        let total_fraction: f64 = report.risk_factors.iter().map(|rf| rf.risk_fraction).sum();
        assert!(
            (total_fraction - 1.0).abs() < 1e-10,
            "risk fractions should sum to 1.0, got {}",
            total_fraction
        );
    }

    #[test]
    fn model_report_risk_fractions_zero_with_zero_beta() {
        let model = SurvivalModel::new(SurvivalConfig {
            warmup_observations: 0,
            ..Default::default()
        });
        let cov = Covariates {
            rss_gb: 4.0,
            pane_count: 20.0,
            ..Default::default()
        };
        let report = model.report(24.0, &cov);
        // With zero beta, all risk fractions should be 0
        for rf in &report.risk_factors {
            assert!(
                rf.risk_fraction.abs() < 1e-15,
                "expected risk_fraction=0 with zero beta, got {} for {}",
                rf.risk_fraction,
                rf.name
            );
        }
    }

    #[test]
    fn baseline_hazard_decreasing_when_k_less_than_1() {
        // k < 1 → decreasing hazard with time (infant mortality)
        let params = WeibullParams {
            shape: 0.5,
            scale: 100.0,
            beta: [0.0; Covariates::COUNT],
        };
        let h1 = params.baseline_hazard(1.0);
        let h2 = params.baseline_hazard(10.0);
        let h3 = params.baseline_hazard(100.0);
        assert!(h1 > 0.0);
        assert!(h1 > h2, "h(1)={} should be > h(10)={} when k<1", h1, h2);
        assert!(h2 > h3, "h(10)={} should be > h(100)={} when k<1", h2, h3);
    }
}
