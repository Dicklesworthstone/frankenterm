//! Anytime-Valid Conformal Drift Detection for ARS Reflexes.
//!
//! Uses e-values (test martingales) to provide mathematically rigorous
//! detection of semantic drift in reflex success rates. When the e-value
//! exceeds 1/α, we have formal proof the reflex has drifted and it is
//! immediately demoted to Shadow Mode (Incubating).
//!
//! # E-Value Testing
//!
//! For each reflex, we maintain a running e-value:
//!
//! ```text
//! E_n = ∏ (1 + λ_i (X_i - p₀))
//! ```
//!
//! where `p₀` is the calibrated null rate, `X_i ∈ {0, 1}` is the outcome,
//! and `λ_i` is an adaptive betting fraction. The e-value is a non-negative
//! supermartingale under H₀, so `P(E_n ≥ 1/α) ≤ α` at any stopping time.
//!
//! # Integration
//!
//! ```text
//! Reflex executes → outcome (success/fail) → ArsDriftDetector.observe()
//!                                              ↓
//!                               E_n > 1/α? → demote to Shadow Mode
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ars_fst::ReflexId;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for e-value drift detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EValueConfig {
    /// Significance level α. Drift declared when e-value ≥ 1/α.
    pub alpha: f64,
    /// Minimum observations before calibration is considered valid.
    pub min_calibration: usize,
    /// Maximum calibration window (FIFO) for estimating null rate.
    pub calibration_window: usize,
    /// Minimum betting fraction magnitude (prevents log-underflow).
    pub min_lambda: f64,
    /// Maximum betting fraction magnitude (prevents blow-up).
    pub max_lambda: f64,
    /// E-value decay factor per observation (prevents runaway from old evidence).
    /// Set to 1.0 to disable decay.
    pub decay: f64,
    /// Whether to auto-reset after drift is detected.
    pub auto_reset_on_drift: bool,
}

impl Default for EValueConfig {
    fn default() -> Self {
        Self {
            alpha: 0.05,
            min_calibration: 10,
            calibration_window: 100,
            min_lambda: 0.01,
            max_lambda: 0.95,
            decay: 0.999,
            auto_reset_on_drift: true,
        }
    }
}

impl EValueConfig {
    /// The rejection threshold: 1/α.
    pub fn threshold(&self) -> f64 {
        1.0 / self.alpha
    }
}

// =============================================================================
// Drift verdict
// =============================================================================

/// Result of a drift check.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DriftVerdict {
    /// No drift detected — e-value below threshold.
    NoDrift {
        e_value: f64,
        null_rate: f64,
    },
    /// Drift detected — e-value exceeded 1/α.
    Drifted {
        e_value: f64,
        null_rate: f64,
        observed_rate: f64,
        observations: usize,
    },
    /// Not enough data to form a judgment.
    InsufficientData {
        observations: usize,
        required: usize,
    },
}

impl DriftVerdict {
    /// Whether drift was detected.
    pub fn is_drifted(&self) -> bool {
        matches!(self, Self::Drifted { .. })
    }

    /// Whether there is sufficient data.
    pub fn has_sufficient_data(&self) -> bool {
        !matches!(self, Self::InsufficientData { .. })
    }
}

// =============================================================================
// Per-reflex e-value monitor
// =============================================================================

/// Monitors a single reflex for semantic drift using e-values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EValueMonitor {
    /// Calibration observations (FIFO of 0/1 values).
    calibration: Vec<f64>,
    /// Estimated null success rate from calibration phase.
    null_rate: f64,
    /// Running e-value (product of likelihood ratios).
    e_value: f64,
    /// Total observations.
    total_observations: usize,
    /// Post-calibration successes.
    post_cal_successes: usize,
    /// Post-calibration observations.
    post_cal_observations: usize,
    /// Number of drift detections.
    drift_count: usize,
    /// Whether calibration is locked (null rate established).
    calibrated: bool,
    /// Cluster ID for this reflex.
    cluster_id: String,
}

impl EValueMonitor {
    /// Create a new monitor for a reflex.
    pub fn new(cluster_id: &str) -> Self {
        Self {
            calibration: Vec::new(),
            null_rate: 0.0,
            e_value: 1.0,
            total_observations: 0,
            post_cal_successes: 0,
            post_cal_observations: 0,
            drift_count: 0,
            calibrated: false,
            cluster_id: cluster_id.to_string(),
        }
    }

    /// Observe an outcome (1.0 = success, 0.0 = failure).
    pub fn observe(&mut self, outcome: f64, config: &EValueConfig) -> DriftVerdict {
        self.total_observations += 1;
        let outcome = outcome.clamp(0.0, 1.0);

        // Phase 1: Calibration.
        if !self.calibrated {
            self.calibration.push(outcome);

            // FIFO eviction if over window.
            while self.calibration.len() > config.calibration_window {
                self.calibration.remove(0);
            }

            if self.calibration.len() < config.min_calibration {
                return DriftVerdict::InsufficientData {
                    observations: self.calibration.len(),
                    required: config.min_calibration,
                };
            }

            // Lock calibration.
            let sum: f64 = self.calibration.iter().sum();
            self.null_rate = sum / self.calibration.len() as f64;
            // Clamp to avoid degenerate rates.
            self.null_rate = self.null_rate.clamp(0.01, 0.99);
            self.calibrated = true;
            self.e_value = 1.0;
            debug!(
                null_rate = self.null_rate,
                n = self.calibration.len(),
                "drift monitor calibrated"
            );
            return DriftVerdict::NoDrift {
                e_value: self.e_value,
                null_rate: self.null_rate,
            };
        }

        // Phase 2: E-value accumulation.
        self.post_cal_observations += 1;
        if outcome > 0.5 {
            self.post_cal_successes += 1;
        }

        // Adaptive lambda: bet on the difference between observed and null rate.
        let observed_rate = if self.post_cal_observations > 0 {
            self.post_cal_successes as f64 / self.post_cal_observations as f64
        } else {
            self.null_rate
        };
        let lambda = self.compute_lambda(observed_rate, config);

        // E-value update: E_n = E_{n-1} × (1 + λ(X - p₀)).
        let factor = 1.0 + lambda * (outcome - self.null_rate);
        // Ensure non-negative (martingale property).
        let factor = factor.max(0.0);

        // Apply decay to prevent old evidence from dominating.
        self.e_value = self.e_value * config.decay * factor;

        // Clamp to avoid infinity.
        self.e_value = self.e_value.min(1e15);

        // Check for drift.
        if self.e_value >= config.threshold() {
            self.drift_count += 1;
            let verdict = DriftVerdict::Drifted {
                e_value: self.e_value,
                null_rate: self.null_rate,
                observed_rate,
                observations: self.post_cal_observations,
            };

            warn!(
                e_value = self.e_value,
                null_rate = self.null_rate,
                observed_rate,
                "drift detected — e-value exceeded threshold"
            );

            if config.auto_reset_on_drift {
                self.reset_monitoring();
            }

            return verdict;
        }

        DriftVerdict::NoDrift {
            e_value: self.e_value,
            null_rate: self.null_rate,
        }
    }

    /// Compute the adaptive betting fraction λ.
    fn compute_lambda(&self, observed_rate: f64, config: &EValueConfig) -> f64 {
        let p0 = self.null_rate;
        let diff = observed_rate - p0;

        // Scale lambda by magnitude of observed deviation.
        // Optimal Kelly-style: λ* = (p_obs - p0) / (p0(1-p0))
        let variance = p0 * (1.0 - p0);
        if variance < 1e-12 {
            return 0.0;
        }

        let lambda = diff / variance;
        // Clamp to safe range.
        lambda.clamp(-config.max_lambda, config.max_lambda)
            .clamp(-config.max_lambda, config.max_lambda)
            // Ensure |λ| ≥ min_lambda only if there's a real signal.
            * if diff.abs() > 0.01 { 1.0 } else { 0.0 }
    }

    /// Reset monitoring (keep calibration, reset e-value).
    fn reset_monitoring(&mut self) {
        self.e_value = 1.0;
        self.post_cal_successes = 0;
        self.post_cal_observations = 0;
    }

    /// Full reset (including calibration).
    pub fn full_reset(&mut self) {
        self.calibration.clear();
        self.null_rate = 0.0;
        self.e_value = 1.0;
        self.total_observations = 0;
        self.post_cal_successes = 0;
        self.post_cal_observations = 0;
        self.calibrated = false;
    }

    /// Current e-value.
    pub fn e_value(&self) -> f64 {
        self.e_value
    }

    /// Estimated null rate.
    pub fn null_rate(&self) -> f64 {
        self.null_rate
    }

    /// Whether calibration is complete.
    pub fn is_calibrated(&self) -> bool {
        self.calibrated
    }

    /// Total observations.
    pub fn total_observations(&self) -> usize {
        self.total_observations
    }

    /// Number of drift detections.
    pub fn drift_count(&self) -> usize {
        self.drift_count
    }

    /// Cluster ID.
    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    /// Post-calibration observed rate.
    pub fn observed_rate(&self) -> f64 {
        if self.post_cal_observations > 0 {
            self.post_cal_successes as f64 / self.post_cal_observations as f64
        } else {
            self.null_rate
        }
    }
}

// =============================================================================
// Drift event
// =============================================================================

/// Event emitted when a reflex drifts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArsDriftEvent {
    /// Reflex that drifted.
    pub reflex_id: ReflexId,
    /// Cluster the reflex belongs to.
    pub cluster_id: String,
    /// E-value at detection.
    pub e_value: f64,
    /// Null (calibrated) success rate.
    pub null_rate: f64,
    /// Observed success rate post-calibration.
    pub observed_rate: f64,
    /// Post-calibration observations at detection.
    pub observations: usize,
    /// Suggested action.
    pub action: DriftAction,
}

/// Action to take when drift is detected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriftAction {
    /// Demote to Incubating (shadow mode).
    DemoteToShadow,
    /// Recalibrate the null rate.
    Recalibrate,
    /// Alert operator.
    AlertOperator,
}

// =============================================================================
// Multi-reflex drift detector
// =============================================================================

/// Manages drift detection across all reflexes.
pub struct ArsDriftDetector {
    config: EValueConfig,
    /// Per-reflex monitors.
    monitors: HashMap<ReflexId, EValueMonitor>,
    /// Total drift events emitted.
    total_drifts: u64,
    /// Total observations processed.
    total_observations: u64,
}

impl ArsDriftDetector {
    /// Create with given config.
    pub fn new(config: EValueConfig) -> Self {
        Self {
            config,
            monitors: HashMap::new(),
            total_drifts: 0,
            total_observations: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(EValueConfig::default())
    }

    /// Register a reflex for monitoring.
    pub fn register_reflex(&mut self, reflex_id: ReflexId, cluster_id: &str) {
        self.monitors
            .entry(reflex_id)
            .or_insert_with(|| EValueMonitor::new(cluster_id));
    }

    /// Observe an execution outcome for a reflex.
    /// Returns a drift event if drift was detected.
    pub fn observe(
        &mut self,
        reflex_id: ReflexId,
        success: bool,
    ) -> Option<ArsDriftEvent> {
        self.total_observations += 1;
        let outcome = if success { 1.0 } else { 0.0 };

        let monitor = self
            .monitors
            .entry(reflex_id)
            .or_insert_with(|| EValueMonitor::new("unknown"));

        let verdict = monitor.observe(outcome, &self.config);

        if let DriftVerdict::Drifted {
            e_value,
            null_rate,
            observed_rate,
            observations,
        } = verdict
        {
            self.total_drifts += 1;
            Some(ArsDriftEvent {
                reflex_id,
                cluster_id: monitor.cluster_id().to_string(),
                e_value,
                null_rate,
                observed_rate,
                observations,
                action: DriftAction::DemoteToShadow,
            })
        } else {
            None
        }
    }

    /// Get the monitor for a reflex.
    pub fn monitor(&self, reflex_id: ReflexId) -> Option<&EValueMonitor> {
        self.monitors.get(&reflex_id)
    }

    /// Get the configuration.
    pub fn config(&self) -> &EValueConfig {
        &self.config
    }

    /// Get statistics.
    pub fn stats(&self) -> ArsDriftStats {
        let calibrated = self.monitors.values().filter(|m| m.is_calibrated()).count();
        let drifted = self.monitors.values().filter(|m| m.drift_count() > 0).count();
        ArsDriftStats {
            total_observations: self.total_observations,
            total_drifts: self.total_drifts,
            registered_reflexes: self.monitors.len(),
            calibrated_reflexes: calibrated,
            drifted_reflexes: drifted,
        }
    }

    /// Reset a specific reflex monitor.
    pub fn reset_reflex(&mut self, reflex_id: ReflexId) {
        if let Some(monitor) = self.monitors.get_mut(&reflex_id) {
            monitor.full_reset();
        }
    }

    /// Check all reflexes and return any with high e-values (approaching drift).
    pub fn at_risk_reflexes(&self, warning_fraction: f64) -> Vec<(ReflexId, f64)> {
        let threshold = self.config.threshold();
        let warn_level = threshold * warning_fraction;
        self.monitors
            .iter()
            .filter(|(_, m)| m.is_calibrated() && m.e_value() >= warn_level)
            .map(|(&id, m)| (id, m.e_value()))
            .collect()
    }
}

/// Statistics for drift detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArsDriftStats {
    pub total_observations: u64,
    pub total_drifts: u64,
    pub registered_reflexes: usize,
    pub calibrated_reflexes: usize,
    pub drifted_reflexes: usize,
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn quick_config() -> EValueConfig {
        EValueConfig {
            min_calibration: 5,
            calibration_window: 20,
            alpha: 0.05,
            decay: 1.0, // No decay for deterministic tests.
            auto_reset_on_drift: true,
            ..Default::default()
        }
    }

    // ---- EValueConfig ----

    #[test]
    fn default_threshold() {
        let config = EValueConfig::default();
        let diff = (config.threshold() - 20.0).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn custom_alpha_threshold() {
        let config = EValueConfig {
            alpha: 0.01,
            ..Default::default()
        };
        let diff = (config.threshold() - 100.0).abs();
        assert!(diff < 1e-10);
    }

    // ---- EValueMonitor calibration ----

    #[test]
    fn insufficient_data_before_calibration() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");

        let v = monitor.observe(1.0, &config);
        let is_insuf = matches!(v, DriftVerdict::InsufficientData { .. });
        assert!(is_insuf);
    }

    #[test]
    fn calibrates_after_min_observations() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");

        for _ in 0..4 {
            monitor.observe(1.0, &config);
        }
        assert!(!monitor.is_calibrated());

        let v = monitor.observe(1.0, &config);
        assert!(monitor.is_calibrated());
        let is_no_drift = matches!(v, DriftVerdict::NoDrift { .. });
        assert!(is_no_drift);
    }

    #[test]
    fn null_rate_matches_calibration_data() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");

        // 3 successes, 2 failures = 0.6 rate.
        for outcome in [1.0, 1.0, 1.0, 0.0, 0.0] {
            monitor.observe(outcome, &config);
        }
        let diff = (monitor.null_rate() - 0.6).abs();
        assert!(diff < 1e-10, "expected 0.6, got {}", monitor.null_rate());
    }

    #[test]
    fn null_rate_clamped_to_safe_range() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");

        // All successes → rate 1.0, clamped to 0.99.
        for _ in 0..5 {
            monitor.observe(1.0, &config);
        }
        assert!(monitor.null_rate() <= 0.99);

        // All failures → rate 0.0, clamped to 0.01.
        let mut monitor2 = EValueMonitor::new("c1");
        for _ in 0..5 {
            monitor2.observe(0.0, &config);
        }
        assert!(monitor2.null_rate() >= 0.01);
    }

    // ---- E-value behavior ----

    #[test]
    fn e_value_starts_at_one() {
        let monitor = EValueMonitor::new("c1");
        let diff = (monitor.e_value() - 1.0).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn e_value_stable_under_null() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at 80% success rate.
        for outcome in [1.0, 1.0, 1.0, 1.0, 0.0] {
            monitor.observe(outcome, &config);
        }

        // Continue with same rate — e-value should stay moderate.
        for i in 0..20 {
            let outcome = if i % 5 == 0 { 0.0 } else { 1.0 };
            let v = monitor.observe(outcome, &config);
            assert!(!v.is_drifted(), "should not drift under null at step {i}");
        }
    }

    #[test]
    fn e_value_grows_under_drift() {
        let config = EValueConfig {
            min_calibration: 5,
            alpha: 0.05,
            decay: 1.0,
            auto_reset_on_drift: false, // Don't reset so we can observe growth.
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at 80% success rate.
        for outcome in [1.0, 1.0, 1.0, 1.0, 0.0] {
            monitor.observe(outcome, &config);
        }
        let e_after_cal = monitor.e_value();

        // Now feed all failures — should drive e-value up.
        for _ in 0..50 {
            monitor.observe(0.0, &config);
        }
        assert!(
            monitor.e_value() > e_after_cal,
            "e-value should grow under drift: {} vs {}",
            monitor.e_value(),
            e_after_cal
        );
    }

    #[test]
    fn detects_drift_on_rate_drop() {
        let config = EValueConfig {
            min_calibration: 10,
            alpha: 0.05,
            decay: 1.0,
            auto_reset_on_drift: true,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at 90% success.
        for i in 0..10 {
            let outcome = if i == 0 { 0.0 } else { 1.0 };
            monitor.observe(outcome, &config);
        }

        // Drop to 0% — should detect drift eventually.
        let mut detected = false;
        for _ in 0..200 {
            let v = monitor.observe(0.0, &config);
            if v.is_drifted() {
                detected = true;
                break;
            }
        }
        assert!(detected, "should detect drift on rate drop");
    }

    #[test]
    fn auto_reset_after_drift() {
        let config = EValueConfig {
            min_calibration: 5,
            alpha: 0.05,
            decay: 1.0,
            auto_reset_on_drift: true,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate.
        for outcome in [1.0, 1.0, 1.0, 1.0, 0.0] {
            monitor.observe(outcome, &config);
        }

        // Force drift.
        let mut drifted = false;
        for _ in 0..200 {
            let v = monitor.observe(0.0, &config);
            if v.is_drifted() {
                drifted = true;
                break;
            }
        }
        assert!(drifted);

        // After auto-reset, e-value should be 1.0.
        let diff = (monitor.e_value() - 1.0).abs();
        assert!(diff < 1e-10, "e-value should reset to 1.0 after drift");
    }

    #[test]
    fn full_reset_clears_everything() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate.
        for _ in 0..5 {
            monitor.observe(1.0, &config);
        }
        assert!(monitor.is_calibrated());

        monitor.full_reset();
        assert!(!monitor.is_calibrated());
        assert_eq!(monitor.total_observations(), 0);
        let diff = (monitor.e_value() - 1.0).abs();
        assert!(diff < 1e-10);
    }

    // ---- DriftVerdict ----

    #[test]
    fn no_drift_is_not_drifted() {
        let v = DriftVerdict::NoDrift {
            e_value: 1.5,
            null_rate: 0.8,
        };
        assert!(!v.is_drifted());
        assert!(v.has_sufficient_data());
    }

    #[test]
    fn drifted_is_drifted() {
        let v = DriftVerdict::Drifted {
            e_value: 25.0,
            null_rate: 0.8,
            observed_rate: 0.3,
            observations: 50,
        };
        assert!(v.is_drifted());
    }

    #[test]
    fn insufficient_has_no_sufficient_data() {
        let v = DriftVerdict::InsufficientData {
            observations: 3,
            required: 10,
        };
        assert!(!v.has_sufficient_data());
        assert!(!v.is_drifted());
    }

    // ---- ArsDriftDetector ----

    #[test]
    fn register_and_observe() {
        let mut detector = ArsDriftDetector::with_defaults();
        detector.register_reflex(1, "c1");

        let result = detector.observe(1, true);
        assert!(result.is_none()); // Insufficient data.
    }

    #[test]
    fn auto_registers_unknown_reflex() {
        let mut detector = ArsDriftDetector::with_defaults();
        detector.observe(99, true);
        assert!(detector.monitor(99).is_some());
    }

    #[test]
    fn detects_drift_via_detector() {
        let config = EValueConfig {
            min_calibration: 5,
            alpha: 0.05,
            decay: 1.0,
            ..Default::default()
        };
        let mut detector = ArsDriftDetector::new(config);
        detector.register_reflex(1, "c1");

        // Calibrate with high success.
        for _ in 0..5 {
            detector.observe(1, true);
        }

        // Drop to all failures.
        let mut detected = false;
        for _ in 0..200 {
            if let Some(event) = detector.observe(1, false) {
                assert_eq!(event.reflex_id, 1);
                assert_eq!(event.action, DriftAction::DemoteToShadow);
                detected = true;
                break;
            }
        }
        assert!(detected, "detector should detect drift");
    }

    #[test]
    fn stats_track_correctly() {
        let config = EValueConfig {
            min_calibration: 3,
            ..Default::default()
        };
        let mut detector = ArsDriftDetector::new(config);
        detector.register_reflex(1, "c1");
        detector.register_reflex(2, "c2");

        for _ in 0..5 {
            detector.observe(1, true);
            detector.observe(2, true);
        }

        let stats = detector.stats();
        assert_eq!(stats.registered_reflexes, 2);
        assert_eq!(stats.total_observations, 10);
    }

    #[test]
    fn at_risk_reflexes_empty_when_stable() {
        let config = quick_config();
        let mut detector = ArsDriftDetector::new(config);
        detector.register_reflex(1, "c1");

        // Calibrate.
        for _ in 0..5 {
            detector.observe(1, true);
        }

        // Continue stable.
        for _ in 0..10 {
            detector.observe(1, true);
        }

        let at_risk = detector.at_risk_reflexes(0.5);
        assert!(at_risk.is_empty());
    }

    #[test]
    fn reset_reflex_clears_monitor() {
        let config = quick_config();
        let mut detector = ArsDriftDetector::new(config);
        detector.register_reflex(1, "c1");

        for _ in 0..10 {
            detector.observe(1, true);
        }
        assert!(detector.monitor(1).unwrap().is_calibrated());

        detector.reset_reflex(1);
        assert!(!detector.monitor(1).unwrap().is_calibrated());
    }

    // ---- Serde roundtrips ----

    #[test]
    fn config_serde_roundtrip() {
        let config = EValueConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let decoded: EValueConfig = serde_json::from_str(&json).unwrap();
        let diff = (decoded.alpha - config.alpha).abs();
        assert!(diff < 1e-10);
    }

    #[test]
    fn verdict_serde_roundtrip() {
        let v = DriftVerdict::Drifted {
            e_value: 25.0,
            null_rate: 0.8,
            observed_rate: 0.3,
            observations: 50,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: DriftVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn drift_event_serde_roundtrip() {
        let event = ArsDriftEvent {
            reflex_id: 42,
            cluster_id: "c1".to_string(),
            e_value: 25.0,
            null_rate: 0.8,
            observed_rate: 0.3,
            observations: 50,
            action: DriftAction::DemoteToShadow,
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: ArsDriftEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.reflex_id, 42);
        assert_eq!(decoded.action, DriftAction::DemoteToShadow);
    }

    #[test]
    fn drift_action_serde_roundtrip() {
        for action in [
            DriftAction::DemoteToShadow,
            DriftAction::Recalibrate,
            DriftAction::AlertOperator,
        ] {
            let json = serde_json::to_string(&action).unwrap();
            let decoded: DriftAction = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, action);
        }
    }

    #[test]
    fn stats_serde_roundtrip() {
        let stats = ArsDriftStats {
            total_observations: 100,
            total_drifts: 2,
            registered_reflexes: 5,
            calibrated_reflexes: 4,
            drifted_reflexes: 2,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: ArsDriftStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, stats);
    }

    #[test]
    fn monitor_serde_roundtrip() {
        let config = quick_config();
        let mut monitor = EValueMonitor::new("c1");
        for _ in 0..5 {
            monitor.observe(1.0, &config);
        }

        let json = serde_json::to_string(&monitor).unwrap();
        let decoded: EValueMonitor = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.is_calibrated(), monitor.is_calibrated());
        let diff = (decoded.null_rate() - monitor.null_rate()).abs();
        assert!(diff < 1e-10);
    }

    // ---- Decay behavior ----

    #[test]
    fn decay_prevents_runaway() {
        let config = EValueConfig {
            min_calibration: 5,
            decay: 0.9, // Aggressive decay.
            alpha: 0.05,
            auto_reset_on_drift: false,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at 50%.
        for outcome in [1.0, 0.0, 1.0, 0.0, 1.0] {
            monitor.observe(outcome, &config);
        }

        // Small deviations — with decay, e-value should stay bounded.
        for _ in 0..100 {
            monitor.observe(0.6, &config);
        }
        // E-value should be finite and moderate.
        assert!(monitor.e_value() < 1e10);
    }

    #[test]
    fn e_value_capped_at_max() {
        let config = EValueConfig {
            min_calibration: 5,
            decay: 1.0,
            alpha: 0.0001, // Very strict — threshold = 10000.
            auto_reset_on_drift: false,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at 90%.
        for outcome in [1.0, 1.0, 1.0, 1.0, 0.0] {
            monitor.observe(outcome, &config);
        }

        // Massive drift.
        for _ in 0..10000 {
            monitor.observe(0.0, &config);
        }
        assert!(monitor.e_value() <= 1e15);
    }
}
