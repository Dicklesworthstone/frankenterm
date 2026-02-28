//! Adaptive watchdog thresholds via scalar Kalman filter.
//!
//! Replaces fixed heartbeat staleness thresholds with learned adaptive
//! thresholds based on observed inter-heartbeat intervals.  Each monitored
//! component gets its own Kalman filter that tracks the true heartbeat
//! interval and its variance.  Thresholds are set at μ + k·σ, where k
//! controls sensitivity.
//!
//! # How It Works
//!
//! ```text
//! heartbeat₁ ──┐                    ┌── threshold = μ + k·σ
//! heartbeat₂ ──┤  interval(ms)      │
//! heartbeat₃ ──┼─────────────► KF ──┼── z-score = (interval - μ) / σ
//! heartbeat₄ ──┤                    │
//! heartbeat₅ ──┘                    └── health = f(z-score)
//! ```
//!
//! During warmup (< `min_observations`), falls back to fixed thresholds.
//! After warmup, z-score-based classification provides smooth anomaly detection.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::watchdog::{Component, HealthStatus};

// =============================================================================
// Telemetry
// =============================================================================

/// Operational telemetry counters for the adaptive watchdog.
///
/// All counters are plain `u64` because `AdaptiveWatchdog` uses `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct AdaptiveWatchdogTelemetry {
    /// Total observe() calls (heartbeat observations).
    observations: u64,
    /// Observations that hit a registered component.
    observations_matched: u64,
    /// Total check_health() calls.
    health_checks: u64,
    /// Total classify_component() calls.
    classifications: u64,
    /// Total reset() calls.
    resets: u64,
    /// Count of Healthy classifications produced.
    status_healthy: u64,
    /// Count of Degraded classifications produced.
    status_degraded: u64,
    /// Count of Critical classifications produced.
    status_critical: u64,
    /// Count of Hung classifications produced.
    status_hung: u64,
}

impl AdaptiveWatchdogTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> AdaptiveWatchdogTelemetrySnapshot {
        AdaptiveWatchdogTelemetrySnapshot {
            observations: self.observations,
            observations_matched: self.observations_matched,
            health_checks: self.health_checks,
            classifications: self.classifications,
            resets: self.resets,
            status_healthy: self.status_healthy,
            status_degraded: self.status_degraded,
            status_critical: self.status_critical,
            status_hung: self.status_hung,
        }
    }
}

/// Serializable snapshot of adaptive watchdog telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdaptiveWatchdogTelemetrySnapshot {
    /// Total observe() calls.
    pub observations: u64,
    /// Observations that hit a registered component.
    pub observations_matched: u64,
    /// Total check_health() calls.
    pub health_checks: u64,
    /// Total classify_component() calls.
    pub classifications: u64,
    /// Total reset() calls.
    pub resets: u64,
    /// Count of Healthy classifications from check_health/classify_component.
    pub status_healthy: u64,
    /// Count of Degraded classifications.
    pub status_degraded: u64,
    /// Count of Critical classifications.
    pub status_critical: u64,
    /// Count of Hung classifications.
    pub status_hung: u64,
}

// =============================================================================
// Scalar Kalman Filter
// =============================================================================

/// Scalar (1D) Kalman filter for tracking a stationary signal with noise.
///
/// State model: x(t) = x(t-1) + w,  w ~ N(0, Q)
/// Observation:  z(t) = x(t) + v,    v ~ N(0, R)
///
/// The filter estimates the true value `x` and its uncertainty `P`.
#[derive(Debug, Clone)]
pub struct KalmanFilter {
    /// State estimate (e.g., estimated inter-heartbeat interval in ms).
    x: f64,
    /// Estimate variance (uncertainty squared).
    p: f64,
    /// Process noise variance — how much the true value can drift per step.
    q: f64,
    /// Measurement noise variance — observation jitter.
    r: f64,
    /// Whether the filter has been initialized with a first observation.
    initialized: bool,
}

impl KalmanFilter {
    /// Create a new Kalman filter with specified noise parameters.
    ///
    /// - `q`: Process noise variance (higher = more adaptive, tracks changes faster)
    /// - `r`: Measurement noise variance (higher = smoother, less reactive to outliers)
    #[must_use]
    pub fn new(q: f64, r: f64) -> Self {
        Self {
            x: 0.0,
            p: 1.0,
            q: q.max(1e-12), // Prevent zero process noise
            r: r.max(1e-12), // Prevent zero measurement noise
            initialized: false,
        }
    }

    /// Feed a new observation into the filter.
    ///
    /// On the first call, initializes the state to the observation value.
    /// Subsequent calls run the predict-update cycle.
    pub fn update(&mut self, z: f64) {
        if !self.initialized {
            self.x = z;
            self.p = self.r; // Initial uncertainty = measurement noise
            self.initialized = true;
            return;
        }

        // Predict step: state unchanged (constant model), variance grows
        let p_pred = self.p + self.q;

        // Update step
        let innovation = z - self.x;
        let s = p_pred + self.r; // Innovation variance
        let k = p_pred / s; // Kalman gain

        self.x += k * innovation;
        self.p = (1.0 - k) * p_pred;

        // Ensure P stays positive (numerical safety)
        if self.p < 1e-15 {
            self.p = 1e-15;
        }
    }

    /// Current state estimate.
    #[must_use]
    pub fn estimate(&self) -> f64 {
        self.x
    }

    /// Current estimate variance.
    #[must_use]
    pub fn variance(&self) -> f64 {
        self.p
    }

    /// Standard deviation of the estimate (√P).
    #[must_use]
    pub fn std_dev(&self) -> f64 {
        self.p.sqrt()
    }

    /// Whether the filter has received at least one observation.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Compute the z-score for a given observation relative to the current estimate.
    ///
    /// z = (observation - estimate) / std_dev
    ///
    /// Returns `None` if the filter is uninitialized or variance is zero.
    #[must_use]
    pub fn z_score(&self, observation: f64) -> Option<f64> {
        if !self.initialized || self.p <= 0.0 {
            return None;
        }
        Some((observation - self.x) / self.p.sqrt())
    }

    /// Reset the filter to its uninitialized state.
    pub fn reset(&mut self) {
        self.x = 0.0;
        self.p = 1.0;
        self.initialized = false;
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for adaptive Kalman watchdog thresholds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveWatchdogConfig {
    /// Number of standard deviations for the adaptive threshold (default: 3.0).
    /// Higher = more tolerant of slow heartbeats.
    pub sensitivity_k: f64,

    /// Process noise variance for the Kalman filter (default: 100.0).
    /// Higher values make the filter more adaptive to changing conditions.
    /// Units: ms² (since we're tracking intervals in ms).
    pub process_noise: f64,

    /// Measurement noise variance for the Kalman filter (default: 2500.0).
    /// Higher values make the filter smoother and less reactive to outliers.
    /// Units: ms².
    pub measurement_noise: f64,

    /// Minimum observations before switching from fixed to adaptive thresholds.
    pub min_observations: usize,

    /// z-score threshold for Degraded status (default: 2.0).
    pub degraded_z: f64,

    /// z-score threshold for Critical status (default: 3.0).
    pub critical_z: f64,

    /// z-score threshold for Hung status (default: 5.0).
    pub hung_z: f64,
}

impl Default for AdaptiveWatchdogConfig {
    fn default() -> Self {
        Self {
            sensitivity_k: 3.0,
            process_noise: 100.0,      // ~10ms std dev of drift per step
            measurement_noise: 2500.0, // ~50ms std dev of jitter
            min_observations: 5,
            degraded_z: 2.0,
            critical_z: 3.0,
            hung_z: 5.0,
        }
    }
}

// =============================================================================
// Health Classification
// =============================================================================

/// Extended health status that includes z-score context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthClassification {
    /// The health status.
    pub status: HealthStatus,
    /// z-score of the current interval relative to learned distribution.
    /// `None` during warmup (using fixed thresholds).
    pub z_score: Option<f64>,
    /// Current adaptive threshold in ms (`None` during warmup).
    pub adaptive_threshold_ms: Option<f64>,
    /// Kalman estimate of normal interval in ms (`None` if uninitialized).
    pub estimated_interval_ms: Option<f64>,
    /// Kalman estimate std dev in ms (`None` if uninitialized).
    pub estimated_std_dev_ms: Option<f64>,
    /// Number of observations so far.
    pub observations: usize,
    /// Whether adaptive mode is active (vs fallback to fixed).
    pub adaptive_mode: bool,
}

// =============================================================================
// Per-Component Tracker
// =============================================================================

/// Tracks heartbeat intervals for a single component using a Kalman filter.
#[derive(Debug, Clone)]
pub struct ComponentTracker {
    filter: KalmanFilter,
    observations: usize,
    last_heartbeat_ms: Option<u64>,
    /// Fixed threshold to use during warmup period.
    fallback_threshold_ms: u64,
}

impl ComponentTracker {
    /// Create a new tracker with specified fallback threshold.
    #[must_use]
    pub fn new(config: &AdaptiveWatchdogConfig, fallback_threshold_ms: u64) -> Self {
        Self {
            filter: KalmanFilter::new(config.process_noise, config.measurement_noise),
            observations: 0,
            last_heartbeat_ms: None,
            fallback_threshold_ms,
        }
    }

    /// Record a heartbeat and update the Kalman filter with the observed interval.
    pub fn observe(&mut self, heartbeat_ms: u64) {
        if let Some(prev) = self.last_heartbeat_ms {
            let interval = heartbeat_ms.saturating_sub(prev) as f64;
            if interval > 0.0 {
                self.filter.update(interval);
                self.observations += 1;
            }
        }
        self.last_heartbeat_ms = Some(heartbeat_ms);
    }

    /// Get the adaptive threshold at k standard deviations above the mean.
    #[must_use]
    pub fn adaptive_threshold(&self, k: f64) -> Option<f64> {
        if !self.filter.is_initialized() {
            return None;
        }
        Some(k.mul_add(self.filter.std_dev(), self.filter.estimate()))
    }

    /// Classify the health of this component based on the time since last heartbeat.
    #[must_use]
    pub fn classify(
        &self,
        current_ms: u64,
        config: &AdaptiveWatchdogConfig,
    ) -> HealthClassification {
        let interval_ms = self
            .last_heartbeat_ms
            .map(|last| current_ms.saturating_sub(last) as f64);

        // During warmup: use fixed thresholds
        if self.observations < config.min_observations {
            let status = match interval_ms {
                None => HealthStatus::Healthy, // Never seen — assume startup
                Some(interval) => {
                    let threshold = self.fallback_threshold_ms as f64;
                    if interval <= threshold {
                        HealthStatus::Healthy
                    } else if interval <= threshold * 2.0 {
                        HealthStatus::Degraded
                    } else {
                        HealthStatus::Critical
                    }
                }
            };
            return HealthClassification {
                status,
                z_score: None,
                adaptive_threshold_ms: None,
                estimated_interval_ms: if self.filter.is_initialized() {
                    Some(self.filter.estimate())
                } else {
                    None
                },
                estimated_std_dev_ms: if self.filter.is_initialized() {
                    Some(self.filter.std_dev())
                } else {
                    None
                },
                observations: self.observations,
                adaptive_mode: false,
            };
        }

        // Adaptive mode
        let interval = match interval_ms {
            Some(i) => i,
            None => {
                return HealthClassification {
                    status: HealthStatus::Healthy,
                    z_score: None,
                    adaptive_threshold_ms: self.adaptive_threshold(config.sensitivity_k),
                    estimated_interval_ms: Some(self.filter.estimate()),
                    estimated_std_dev_ms: Some(self.filter.std_dev()),
                    observations: self.observations,
                    adaptive_mode: true,
                };
            }
        };

        let z = self.filter.z_score(interval).unwrap_or(0.0);

        let status = if z < config.degraded_z {
            HealthStatus::Healthy
        } else if z < config.critical_z {
            HealthStatus::Degraded
        } else if z < config.hung_z {
            HealthStatus::Critical
        } else {
            HealthStatus::Hung
        };

        HealthClassification {
            status,
            z_score: Some(z),
            adaptive_threshold_ms: self.adaptive_threshold(config.sensitivity_k),
            estimated_interval_ms: Some(self.filter.estimate()),
            estimated_std_dev_ms: Some(self.filter.std_dev()),
            observations: self.observations,
            adaptive_mode: true,
        }
    }

    /// Number of inter-heartbeat intervals observed.
    #[must_use]
    pub fn observation_count(&self) -> usize {
        self.observations
    }

    /// Kalman filter state estimate (estimated normal interval in ms).
    #[must_use]
    pub fn estimated_interval(&self) -> Option<f64> {
        if self.filter.is_initialized() {
            Some(self.filter.estimate())
        } else {
            None
        }
    }

    /// Reset tracker state.
    pub fn reset(&mut self) {
        self.filter.reset();
        self.observations = 0;
        self.last_heartbeat_ms = None;
    }
}

// =============================================================================
// Adaptive Watchdog
// =============================================================================

/// Adaptive watchdog that wraps per-component Kalman trackers.
///
/// Feeds heartbeat observations to component trackers and produces
/// health classifications with z-score context.
#[derive(Debug, Clone)]
pub struct AdaptiveWatchdog {
    config: AdaptiveWatchdogConfig,
    trackers: HashMap<Component, ComponentTracker>,
    /// Operational telemetry counters.
    telemetry: AdaptiveWatchdogTelemetry,
}

/// Full adaptive health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveHealthReport {
    pub timestamp_ms: u64,
    pub overall: HealthStatus,
    pub components: Vec<ComponentClassification>,
}

/// Per-component classification in the adaptive report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentClassification {
    pub component: Component,
    pub classification: HealthClassification,
}

impl AdaptiveWatchdog {
    /// Create a new adaptive watchdog with default fallback thresholds from `WatchdogConfig`.
    #[must_use]
    pub fn new(config: AdaptiveWatchdogConfig) -> Self {
        let fallback_thresholds = [
            (Component::Discovery, 15_000u64),
            (Component::Capture, 5_000),
            (Component::Persistence, 30_000),
            (Component::Maintenance, 120_000),
        ];

        let trackers = fallback_thresholds
            .into_iter()
            .map(|(comp, threshold)| (comp, ComponentTracker::new(&config, threshold)))
            .collect();

        Self {
            config,
            trackers,
            telemetry: AdaptiveWatchdogTelemetry::new(),
        }
    }

    /// Create with custom fallback thresholds per component.
    #[must_use]
    pub fn with_fallbacks(config: AdaptiveWatchdogConfig, fallbacks: &[(Component, u64)]) -> Self {
        let trackers = fallbacks
            .iter()
            .map(|(comp, threshold)| (*comp, ComponentTracker::new(&config, *threshold)))
            .collect();

        Self {
            config,
            trackers,
            telemetry: AdaptiveWatchdogTelemetry::new(),
        }
    }

    /// Record a heartbeat for a component.
    pub fn observe(&mut self, component: Component, heartbeat_ms: u64) {
        self.telemetry.observations += 1;
        if let Some(tracker) = self.trackers.get_mut(&component) {
            self.telemetry.observations_matched += 1;
            tracker.observe(heartbeat_ms);
        }
    }

    /// Classify health of a single component.
    #[must_use]
    pub fn classify_component(
        &mut self,
        component: Component,
        current_ms: u64,
    ) -> Option<HealthClassification> {
        self.telemetry.classifications += 1;
        let result = self
            .trackers
            .get(&component)
            .map(|t: &ComponentTracker| t.classify(current_ms, &self.config));
        if let Some(ref c) = result {
            self.count_status(c.status);
        }
        result
    }

    /// Produce a full health report across all components.
    #[must_use]
    pub fn check_health(&mut self, current_ms: u64) -> AdaptiveHealthReport {
        self.telemetry.health_checks += 1;
        let mut worst = HealthStatus::Healthy;
        let mut components = Vec::with_capacity(self.trackers.len());

        for (&component, tracker) in &self.trackers {
            let classification = tracker.classify(current_ms, &self.config);
            if classification.status > worst {
                worst = classification.status;
            }
            // Inline count_status to avoid &mut self borrow conflict with &self.trackers
            match classification.status {
                HealthStatus::Healthy => self.telemetry.status_healthy += 1,
                HealthStatus::Degraded => self.telemetry.status_degraded += 1,
                HealthStatus::Critical => self.telemetry.status_critical += 1,
                HealthStatus::Hung => self.telemetry.status_hung += 1,
            }
            components.push(ComponentClassification {
                component,
                classification,
            });
        }

        // Sort by component for deterministic output
        components.sort_by_key(|c| match c.component {
            Component::Discovery => 0,
            Component::Capture => 1,
            Component::Persistence => 2,
            Component::Maintenance => 3,
        });

        AdaptiveHealthReport {
            timestamp_ms: current_ms,
            overall: worst,
            components,
        }
    }

    /// Get the tracker for a specific component.
    #[must_use]
    pub fn tracker(&self, component: Component) -> Option<&ComponentTracker> {
        self.trackers.get(&component)
    }

    /// Get the config.
    #[must_use]
    pub fn config(&self) -> &AdaptiveWatchdogConfig {
        &self.config
    }

    /// Reset all component trackers.
    pub fn reset(&mut self) {
        self.telemetry.resets += 1;
        for tracker in self.trackers.values_mut() {
            tracker.reset();
        }
    }

    /// Access the operational telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &AdaptiveWatchdogTelemetry {
        &self.telemetry
    }

    /// Increment the status-specific counter.
    fn count_status(&mut self, status: HealthStatus) {
        match status {
            HealthStatus::Healthy => self.telemetry.status_healthy += 1,
            HealthStatus::Degraded => self.telemetry.status_degraded += 1,
            HealthStatus::Critical => self.telemetry.status_critical += 1,
            HealthStatus::Hung => self.telemetry.status_hung += 1,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Kalman Filter basics ────────────────────────────────────────────

    #[test]
    fn kalman_uninitialized() {
        let kf = KalmanFilter::new(1.0, 1.0);
        assert!(!kf.is_initialized());
        assert!(kf.estimate().abs() < f64::EPSILON);
        assert!(kf.z_score(10.0).is_none());
    }

    #[test]
    fn kalman_first_observation_initializes() {
        let mut kf = KalmanFilter::new(1.0, 1.0);
        kf.update(100.0);
        assert!(kf.is_initialized());
        assert!((kf.estimate() - 100.0).abs() < 1e-10);
        // Initial P = R
        assert!((kf.variance() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn kalman_converges_to_constant_signal() {
        let mut kf = KalmanFilter::new(0.1, 1.0);
        let true_value = 50.0;

        for _ in 0..100 {
            kf.update(true_value);
        }

        assert!(
            (kf.estimate() - true_value).abs() < 0.5,
            "estimate {} should be near {}",
            kf.estimate(),
            true_value
        );
    }

    #[test]
    fn kalman_variance_stays_positive() {
        let mut kf = KalmanFilter::new(0.01, 0.01);
        for i in 0..1000 {
            kf.update((i as f64).mul_add(0.001, 10.0));
            assert!(kf.variance() > 0.0, "P must stay positive at step {i}");
        }
    }

    #[test]
    fn kalman_z_score_symmetric() {
        let mut kf = KalmanFilter::new(1.0, 1.0);
        // Feed enough observations to stabilize
        for _ in 0..20 {
            kf.update(100.0);
        }

        let z_above = kf.z_score(110.0).unwrap();
        let z_below = kf.z_score(90.0).unwrap();
        assert!(
            (z_above + z_below).abs() < 0.5,
            "z-scores should be roughly symmetric: above={z_above}, below={z_below}"
        );
    }

    #[test]
    fn kalman_adapts_to_shift() {
        let mut kf = KalmanFilter::new(10.0, 5.0);

        // First regime: 100ms intervals
        for _ in 0..50 {
            kf.update(100.0);
        }
        assert!(
            (kf.estimate() - 100.0).abs() < 5.0,
            "should track ~100: {}",
            kf.estimate()
        );

        // Second regime: 200ms intervals (shift)
        for _ in 0..50 {
            kf.update(200.0);
        }
        assert!(
            (kf.estimate() - 200.0).abs() < 10.0,
            "should adapt toward ~200: {}",
            kf.estimate()
        );
    }

    #[test]
    fn kalman_reset() {
        let mut kf = KalmanFilter::new(1.0, 1.0);
        kf.update(100.0);
        kf.update(200.0);
        assert!(kf.is_initialized());

        kf.reset();
        assert!(!kf.is_initialized());
        assert!(kf.estimate().abs() < f64::EPSILON);
    }

    #[test]
    fn kalman_noisy_convergence() {
        let mut kf = KalmanFilter::new(1.0, 25.0); // R=25 → noisy measurements
        let true_value = 75.0;

        // Simulate noisy observations: 75 ± some variation
        let observations = [
            73.0, 78.0, 71.0, 76.0, 74.0, 77.0, 72.0, 79.0, 75.0, 74.0, 76.0, 73.0, 77.0, 74.0,
            76.0, 75.0, 73.0, 78.0, 74.0, 76.0,
        ];

        for &z in &observations {
            kf.update(z);
        }

        assert!(
            (kf.estimate() - true_value).abs() < 5.0,
            "estimate {} should be near {} after 20 noisy obs",
            kf.estimate(),
            true_value
        );
    }

    // ── Config ──────────────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let config = AdaptiveWatchdogConfig::default();
        assert!((config.sensitivity_k - 3.0).abs() < f64::EPSILON);
        assert_eq!(config.min_observations, 5);
        assert!((config.degraded_z - 2.0).abs() < f64::EPSILON);
        assert!((config.critical_z - 3.0).abs() < f64::EPSILON);
        assert!((config.hung_z - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = AdaptiveWatchdogConfig {
            sensitivity_k: 2.5,
            process_noise: 50.0,
            measurement_noise: 1000.0,
            min_observations: 10,
            degraded_z: 1.5,
            critical_z: 2.5,
            hung_z: 4.0,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AdaptiveWatchdogConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.sensitivity_k - 2.5).abs() < f64::EPSILON);
        assert_eq!(parsed.min_observations, 10);
        assert!((parsed.hung_z - 4.0).abs() < f64::EPSILON);
    }

    // ── Component Tracker ───────────────────────────────────────────────

    #[test]
    fn tracker_warmup_uses_fixed_threshold() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 5,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        // Record a few heartbeats (< min_observations)
        tracker.observe(1000);
        tracker.observe(2000);
        tracker.observe(3000);

        // Check at 5500ms — within 5000ms fallback → Healthy
        let c = tracker.classify(5500, &config);
        assert!(!c.adaptive_mode);
        assert_eq!(c.status, HealthStatus::Healthy);
    }

    #[test]
    fn tracker_warmup_fixed_degraded() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 10,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        tracker.observe(1000);
        tracker.observe(2000);

        // 9000ms since last heartbeat (2000), within 2x threshold
        let c = tracker.classify(9000, &config);
        assert!(!c.adaptive_mode);
        assert_eq!(c.status, HealthStatus::Degraded);
    }

    #[test]
    fn tracker_warmup_fixed_critical() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 10,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        tracker.observe(1000);
        tracker.observe(2000);

        // 15000ms since last heartbeat → > 2x threshold → Critical
        let c = tracker.classify(17000, &config);
        assert!(!c.adaptive_mode);
        assert_eq!(c.status, HealthStatus::Critical);
    }

    #[test]
    fn tracker_switches_to_adaptive_after_warmup() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        // Feed 4 heartbeats → 3 intervals → meets min_observations
        for i in 0..4 {
            tracker.observe(i * 1000);
        }

        assert_eq!(tracker.observation_count(), 3);

        // Now classify should use adaptive mode
        let c = tracker.classify(4000, &config);
        assert!(c.adaptive_mode);
        assert!(c.z_score.is_some());
        assert!(c.adaptive_threshold_ms.is_some());
    }

    #[test]
    fn tracker_adaptive_healthy_normal_interval() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            process_noise: 10.0,
            measurement_noise: 100.0,
            degraded_z: 2.0,
            critical_z: 3.0,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        // Feed regular 1000ms intervals
        for i in 0..10 {
            tracker.observe(i * 1000);
        }

        // Check at time 10000 (1000ms since last = normal)
        let c = tracker.classify(10_000, &config);
        assert!(c.adaptive_mode);
        assert_eq!(c.status, HealthStatus::Healthy);
        assert!(c.z_score.unwrap() < 2.0, "z={}", c.z_score.unwrap());
    }

    #[test]
    fn tracker_adaptive_detects_anomaly() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            process_noise: 1.0,
            measurement_noise: 10.0,
            degraded_z: 2.0,
            critical_z: 3.0,
            hung_z: 5.0,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        // Feed regular 1000ms intervals
        for i in 0..20 {
            tracker.observe(i * 1000);
        }

        // 20s since last heartbeat (was expecting ~1s) -> z >> 5 -> Hung
        let c = tracker.classify(39_000, &config);
        assert!(c.adaptive_mode);
        assert!(
            c.z_score.unwrap() >= 5.0,
            "z-score {} should indicate hung (>= 5.0)",
            c.z_score.unwrap()
        );
        assert_eq!(c.status, HealthStatus::Hung);
    }

    #[test]
    fn tracker_estimated_interval() {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        assert!(tracker.estimated_interval().is_none());

        for i in 0..10 {
            tracker.observe(i * 500); // 500ms intervals
        }

        let est = tracker.estimated_interval().unwrap();
        assert!(
            (est - 500.0).abs() < 50.0,
            "estimated interval {} should be near 500",
            est
        );
    }

    #[test]
    fn tracker_reset_clears_state() {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        for i in 0..10 {
            tracker.observe(i * 1000);
        }
        assert!(tracker.observation_count() > 0);

        tracker.reset();
        assert_eq!(tracker.observation_count(), 0);
        assert!(tracker.estimated_interval().is_none());
    }

    // ── Adaptive Watchdog ───────────────────────────────────────────────

    #[test]
    fn watchdog_creates_all_components() {
        let wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        assert!(wd.tracker(Component::Discovery).is_some());
        assert!(wd.tracker(Component::Capture).is_some());
        assert!(wd.tracker(Component::Persistence).is_some());
        assert!(wd.tracker(Component::Maintenance).is_some());
    }

    #[test]
    fn watchdog_healthy_after_regular_heartbeats() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            process_noise: 10.0,
            measurement_noise: 100.0,
            ..Default::default()
        };
        let mut wd = AdaptiveWatchdog::new(config);

        // Simulate regular heartbeats for all components
        for i in 0..10u64 {
            let t = i * 1000;
            wd.observe(Component::Discovery, t);
            wd.observe(Component::Capture, t);
            wd.observe(Component::Persistence, t);
            wd.observe(Component::Maintenance, t);
        }

        let report = wd.check_health(10_000);
        assert_eq!(report.overall, HealthStatus::Healthy);
        assert_eq!(report.components.len(), 4);
    }

    #[test]
    fn watchdog_detects_stale_component() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            process_noise: 1.0,
            measurement_noise: 10.0,
            degraded_z: 2.0,
            critical_z: 3.0,
            ..Default::default()
        };
        let mut wd = AdaptiveWatchdog::new(config);

        // Regular heartbeats for all
        for i in 0..20u64 {
            let t = i * 1000;
            wd.observe(Component::Discovery, t);
            wd.observe(Component::Capture, t);
            wd.observe(Component::Persistence, t);
            wd.observe(Component::Maintenance, t);
        }

        // Discovery stops, others continue
        for i in 20..30u64 {
            let t = i * 1000;
            wd.observe(Component::Capture, t);
            wd.observe(Component::Persistence, t);
            wd.observe(Component::Maintenance, t);
        }

        // Check at time 30s — discovery hasn't heartbeated in 11s
        let report = wd.check_health(30_000);
        assert!(report.overall > HealthStatus::Healthy);

        let discovery = report
            .components
            .iter()
            .find(|c| c.component == Component::Discovery)
            .unwrap();
        assert!(
            discovery.classification.status > HealthStatus::Healthy,
            "discovery should be degraded or critical"
        );
    }

    #[test]
    fn watchdog_report_serializes() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());

        for i in 0..5u64 {
            wd.observe(Component::Discovery, i * 1000);
            wd.observe(Component::Capture, i * 200);
        }

        let report = wd.check_health(5000);
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: AdaptiveHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.overall, report.overall);
        assert_eq!(parsed.components.len(), 4);
    }

    #[test]
    fn watchdog_custom_fallbacks() {
        let config = AdaptiveWatchdogConfig::default();
        let wd = AdaptiveWatchdog::with_fallbacks(
            config,
            &[(Component::Discovery, 10_000), (Component::Capture, 2_000)],
        );

        // Should only have the specified components
        assert!(wd.tracker(Component::Discovery).is_some());
        assert!(wd.tracker(Component::Capture).is_some());
        assert!(wd.tracker(Component::Persistence).is_none());
    }

    #[test]
    fn watchdog_reset() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());

        for i in 0..10u64 {
            wd.observe(Component::Discovery, i * 1000);
        }

        assert!(
            wd.tracker(Component::Discovery)
                .unwrap()
                .observation_count()
                > 0
        );

        wd.reset();

        assert_eq!(
            wd.tracker(Component::Discovery)
                .unwrap()
                .observation_count(),
            0
        );
    }

    // ── Threshold properties ────────────────────────────────────────────

    #[test]
    fn adaptive_threshold_above_estimate() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        for i in 0..20 {
            tracker.observe(i * 1000);
        }

        let est = tracker.estimated_interval().unwrap();
        let threshold = tracker.adaptive_threshold(3.0).unwrap();
        assert!(
            threshold >= est,
            "threshold {} must be >= estimate {}",
            threshold,
            est
        );
    }

    #[test]
    fn z_score_increases_with_interval() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            process_noise: 1.0,
            measurement_noise: 10.0,
            ..Default::default()
        };
        let mut tracker = ComponentTracker::new(&config, 5_000);

        // Stable 1000ms intervals
        for i in 0..20 {
            tracker.observe(i * 1000);
        }

        // z-score at various intervals
        let c1 = tracker.classify(20_000, &config); // 1s since last
        let c2 = tracker.classify(25_000, &config); // 6s since last
        let c3 = tracker.classify(30_000, &config); // 11s since last

        let z1 = c1.z_score.unwrap();
        let z2 = c2.z_score.unwrap();
        let z3 = c3.z_score.unwrap();

        assert!(z1 < z2, "z1={z1} should be < z2={z2}");
        assert!(z2 < z3, "z2={z2} should be < z3={z3}");
    }

    #[test]
    fn health_status_ordering_preserved() {
        assert!(HealthStatus::Healthy < HealthStatus::Degraded);
        assert!(HealthStatus::Degraded < HealthStatus::Critical);
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn tracker_no_heartbeat_during_warmup_is_healthy() {
        let config = AdaptiveWatchdogConfig::default();
        let tracker = ComponentTracker::new(&config, 5_000);

        // Never observed any heartbeat
        let c = tracker.classify(10_000, &config);
        assert_eq!(c.status, HealthStatus::Healthy);
        assert!(!c.adaptive_mode);
    }

    #[test]
    fn tracker_single_heartbeat_no_interval() {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        tracker.observe(1000);
        assert_eq!(tracker.observation_count(), 0); // No interval yet

        let c = tracker.classify(3000, &config);
        assert!(!c.adaptive_mode);
    }

    #[test]
    fn kalman_extreme_values() {
        let mut kf = KalmanFilter::new(0.001, 0.001);
        kf.update(1e10);
        assert!(kf.is_initialized());
        assert!(kf.variance() > 0.0);

        kf.update(1e-10);
        assert!(kf.variance() > 0.0);
    }

    #[test]
    fn tracker_zero_interval_ignored() {
        let config = AdaptiveWatchdogConfig::default();
        let mut tracker = ComponentTracker::new(&config, 5_000);

        tracker.observe(1000);
        tracker.observe(1000); // Same timestamp → zero interval, should be ignored
        assert_eq!(tracker.observation_count(), 0);
    }

    #[test]
    fn classification_serde_roundtrip() {
        let c = HealthClassification {
            status: HealthStatus::Degraded,
            z_score: Some(2.5),
            adaptive_threshold_ms: Some(1500.0),
            estimated_interval_ms: Some(1000.0),
            estimated_std_dev_ms: Some(50.0),
            observations: 20,
            adaptive_mode: true,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: HealthClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.status, HealthStatus::Degraded);
        assert!((parsed.z_score.unwrap() - 2.5).abs() < f64::EPSILON);
        assert_eq!(parsed.observations, 20);
        assert!(parsed.adaptive_mode);
    }

    #[test]
    fn watchdog_classify_single_component() {
        let config = AdaptiveWatchdogConfig {
            min_observations: 3,
            ..Default::default()
        };
        let mut wd = AdaptiveWatchdog::new(config);

        for i in 0..5u64 {
            wd.observe(Component::Capture, i * 200);
        }

        let c = wd.classify_component(Component::Capture, 1200);
        assert!(c.is_some());
        let c = c.unwrap();
        assert!(c.adaptive_mode);
    }

    #[test]
    fn watchdog_classify_nonexistent_component_returns_none() {
        let config = AdaptiveWatchdogConfig::default();
        let mut wd = AdaptiveWatchdog::with_fallbacks(config, &[(Component::Discovery, 5_000)]);

        assert!(wd.classify_component(Component::Capture, 1000).is_none());
    }

    // ── Batch: DarkBadger wa-1u90p.7.1 ──────────────────────

    // ── KalmanFilter additional coverage ────────────────────

    #[test]
    fn kalman_debug_clone() {
        let kf = KalmanFilter::new(100.0, 2500.0);
        let dbg = format!("{:?}", kf);
        assert!(dbg.contains("KalmanFilter"));
        let kf2 = kf.clone();
        assert!(!kf2.is_initialized());
    }

    #[test]
    fn kalman_std_dev_positive() {
        let mut kf = KalmanFilter::new(100.0, 2500.0);
        kf.update(500.0);
        kf.update(510.0);
        assert!(kf.std_dev() > 0.0);
        assert!((kf.std_dev() - kf.variance().sqrt()).abs() < f64::EPSILON);
    }

    #[test]
    fn kalman_is_initialized_tracking() {
        let mut kf = KalmanFilter::new(100.0, 2500.0);
        assert!(!kf.is_initialized());
        kf.update(100.0);
        assert!(kf.is_initialized());
        kf.reset();
        assert!(!kf.is_initialized());
    }

    #[test]
    fn kalman_z_score_none_when_uninitialized() {
        let kf = KalmanFilter::new(100.0, 2500.0);
        assert!(kf.z_score(100.0).is_none());
    }

    #[test]
    fn kalman_z_score_near_zero_for_estimate() {
        let mut kf = KalmanFilter::new(100.0, 2500.0);
        for _ in 0..20 {
            kf.update(500.0);
        }
        let z = kf.z_score(500.0).unwrap();
        assert!(
            z.abs() < 0.5,
            "z-score at estimate should be near zero, got {}",
            z
        );
    }

    #[test]
    fn kalman_process_noise_clamped() {
        let kf = KalmanFilter::new(0.0, 0.0);
        // Should not panic — noise clamped to minimum
        assert!(kf.variance() > 0.0);
    }

    // ── AdaptiveWatchdogConfig additional coverage ──────────

    #[test]
    fn config_debug_clone() {
        let c = AdaptiveWatchdogConfig::default();
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("AdaptiveWatchdogConfig"));
        let c2 = c.clone();
        assert!((c2.sensitivity_k - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn config_default_z_thresholds() {
        let c = AdaptiveWatchdogConfig::default();
        assert!(c.degraded_z < c.critical_z);
        assert!(c.critical_z < c.hung_z);
        assert!((c.degraded_z - 2.0).abs() < f64::EPSILON);
        assert!((c.critical_z - 3.0).abs() < f64::EPSILON);
        assert!((c.hung_z - 5.0).abs() < f64::EPSILON);
    }

    // ── HealthClassification additional coverage ────────────

    #[test]
    fn health_classification_debug_clone() {
        let c = HealthClassification {
            status: HealthStatus::Healthy,
            z_score: None,
            adaptive_threshold_ms: None,
            estimated_interval_ms: None,
            estimated_std_dev_ms: None,
            observations: 0,
            adaptive_mode: false,
        };
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("HealthClassification"));
        let c2 = c.clone();
        assert_eq!(c2.observations, 0);
        assert!(!c2.adaptive_mode);
    }

    #[test]
    fn health_classification_serde_all_none() {
        let c = HealthClassification {
            status: HealthStatus::Healthy,
            z_score: None,
            adaptive_threshold_ms: None,
            estimated_interval_ms: None,
            estimated_std_dev_ms: None,
            observations: 0,
            adaptive_mode: false,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: HealthClassification = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status, HealthStatus::Healthy);
        assert!(back.z_score.is_none());
    }

    // ── ComponentTracker additional coverage ────────────────

    #[test]
    fn tracker_debug_clone() {
        let config = AdaptiveWatchdogConfig::default();
        let t = ComponentTracker::new(&config, 5000);
        let dbg = format!("{:?}", t);
        assert!(dbg.contains("ComponentTracker"));
        let t2 = t.clone();
        assert_eq!(t2.observation_count(), 0);
    }

    #[test]
    fn tracker_observation_count_increments() {
        let config = AdaptiveWatchdogConfig::default();
        let mut t = ComponentTracker::new(&config, 5000);
        assert_eq!(t.observation_count(), 0);
        t.observe(1000);
        assert_eq!(t.observation_count(), 0); // First observe sets baseline, no interval
        t.observe(2000);
        assert_eq!(t.observation_count(), 1);
        t.observe(3000);
        assert_eq!(t.observation_count(), 2);
    }

    #[test]
    fn tracker_estimated_interval_none_initially() {
        let config = AdaptiveWatchdogConfig::default();
        let t = ComponentTracker::new(&config, 5000);
        assert!(t.estimated_interval().is_none());
    }

    #[test]
    fn tracker_adaptive_threshold_none_before_init() {
        let config = AdaptiveWatchdogConfig::default();
        let t = ComponentTracker::new(&config, 5000);
        assert!(t.adaptive_threshold(3.0).is_none());
    }

    #[test]
    fn tracker_adaptive_threshold_positive_after_observations() {
        let config = AdaptiveWatchdogConfig::default();
        let mut t = ComponentTracker::new(&config, 5000);
        for i in 0..10u64 {
            t.observe(i * 1000);
        }
        let threshold = t.adaptive_threshold(3.0);
        assert!(threshold.is_some());
        assert!(threshold.unwrap() > 0.0);
    }

    // ── AdaptiveWatchdog additional coverage ────────────────

    #[test]
    fn watchdog_debug_clone() {
        let config = AdaptiveWatchdogConfig::default();
        let wd = AdaptiveWatchdog::new(config);
        let dbg = format!("{:?}", wd);
        assert!(dbg.contains("AdaptiveWatchdog"));
        let mut wd2 = wd.clone();
        let _ = wd2.check_health(1000); // Should work on clone
    }

    #[test]
    fn watchdog_config_accessor() {
        let config = AdaptiveWatchdogConfig {
            sensitivity_k: 4.0,
            ..Default::default()
        };
        let wd = AdaptiveWatchdog::new(config);
        assert!((wd.config().sensitivity_k - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn watchdog_tracker_accessor() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.observe(Component::Capture, 1000);
        assert!(wd.tracker(Component::Capture).is_some());
        // All component trackers are pre-created in new()
        assert!(wd.tracker(Component::Discovery).is_some());
    }

    // ── AdaptiveHealthReport additional coverage ────────────

    #[test]
    fn health_report_debug_clone_serde() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.observe(Component::Capture, 1000);
        wd.observe(Component::Capture, 2000);
        let report = wd.check_health(2500);
        let dbg = format!("{:?}", report);
        assert!(dbg.contains("AdaptiveHealthReport"));
        let r2 = report.clone();
        assert_eq!(r2.overall, report.overall);
        let json = serde_json::to_string(&report).unwrap();
        let back: AdaptiveHealthReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.overall, report.overall);
    }

    // ── ComponentClassification additional coverage ─────────

    #[test]
    fn component_classification_debug_clone() {
        let cc = ComponentClassification {
            component: Component::Capture,
            classification: HealthClassification {
                status: HealthStatus::Healthy,
                z_score: Some(0.5),
                adaptive_threshold_ms: Some(1500.0),
                estimated_interval_ms: Some(1000.0),
                estimated_std_dev_ms: Some(50.0),
                observations: 10,
                adaptive_mode: true,
            },
        };
        let dbg = format!("{:?}", cc);
        assert!(dbg.contains("ComponentClassification"));
        let cc2 = cc.clone();
        assert_eq!(cc2.component, Component::Capture);
    }

    // ── Telemetry counter tests ──────────────────────────────────────

    #[test]
    fn telemetry_initial_zero() {
        let wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        let snap = wd.telemetry().snapshot();
        assert_eq!(snap.observations, 0);
        assert_eq!(snap.observations_matched, 0);
        assert_eq!(snap.health_checks, 0);
        assert_eq!(snap.classifications, 0);
        assert_eq!(snap.resets, 0);
        assert_eq!(snap.status_healthy, 0);
        assert_eq!(snap.status_degraded, 0);
        assert_eq!(snap.status_critical, 0);
        assert_eq!(snap.status_hung, 0);
    }

    #[test]
    fn telemetry_observe_counted() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.observe(Component::Capture, 1000);
        wd.observe(Component::Capture, 2000);
        wd.observe(Component::Persistence, 1500);

        let snap = wd.telemetry().snapshot();
        assert_eq!(snap.observations, 3);
        assert!(snap.observations_matched >= 1); // at least some matched
    }

    #[test]
    fn telemetry_health_check_counted() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.observe(Component::Capture, 1000);
        let _ = wd.check_health(2000);

        let snap = wd.telemetry().snapshot();
        assert_eq!(snap.health_checks, 1);
        // At least one status bucket should be incremented
        let total_statuses = snap.status_healthy + snap.status_degraded
            + snap.status_critical + snap.status_hung;
        assert!(total_statuses >= 1);
    }

    #[test]
    fn telemetry_classify_counted() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.observe(Component::Capture, 1000);
        let _ = wd.classify_component(Component::Capture, 2000);

        let snap = wd.telemetry().snapshot();
        assert_eq!(snap.classifications, 1);
    }

    #[test]
    fn telemetry_reset_counted() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.reset();

        let snap = wd.telemetry().snapshot();
        assert_eq!(snap.resets, 1);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let mut wd = AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default());
        wd.observe(Component::Capture, 1000);
        let _ = wd.check_health(2000);

        let snap = wd.telemetry().snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: AdaptiveWatchdogTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.observations, snap.observations);
        assert_eq!(parsed.health_checks, snap.health_checks);
    }
}
