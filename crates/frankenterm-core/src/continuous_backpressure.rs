//! Continuous backpressure severity function.
//!
//! Replaces the discrete 4-tier FSM in `backpressure.rs` with a smooth
//! sigmoid severity function and exponential moving average (EMA)
//! smoothing for natural hysteresis.
//!
//! # Severity Function
//!
//! ```text
//!   s(t) = σ(k · (q(t) - θ))
//!
//!   where σ(x) = 1 / (1 + e⁻ˣ)
//!         q(t) = EMA-smoothed queue ratio
//!         θ    = center threshold (severity = 0.5)
//!         k    = steepness parameter
//! ```
//!
//! # Throttle Actions
//!
//! Each action scales continuously with severity:
//! - Polling backoff: `1.0 + 3.0 · s` (1× to 4×)
//! - Pane skip fraction: `0.5 · s²` (0% to 50%)
//! - Detection skip: `0.25 · s` (0% to 25%)
//! - Buffer limit: `max · (1 - 0.8 · s)` (100% to 20%)

use serde::{Deserialize, Serialize};

use crate::backpressure::BackpressureTier;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the continuous backpressure model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuousBackpressureConfig {
    /// Queue ratio where severity equals 0.5 (default: 0.60).
    pub center_threshold: f64,

    /// Sigmoid steepness — controls how fast severity rises (default: 8.0).
    /// Higher values create a sharper transition around the center.
    pub steepness: f64,

    /// EMA smoothing window in samples (default: 10).
    /// Alpha = 2 / (N + 1).
    pub smoothing_window: usize,

    /// Maximum polling backoff multiplier (default: 3.0).
    /// Actual multiplier = 1.0 + max_backoff * severity.
    pub max_backoff_multiplier: f64,

    /// Maximum pane skip fraction at full severity (default: 0.50).
    pub max_pane_skip: f64,

    /// Maximum detection skip fraction at full severity (default: 0.25).
    pub max_detection_skip: f64,

    /// Minimum buffer limit as fraction of normal (default: 0.20).
    /// At full severity, buffer = max_segments * min_buffer_fraction.
    pub min_buffer_fraction: f64,
}

impl Default for ContinuousBackpressureConfig {
    fn default() -> Self {
        Self {
            center_threshold: 0.60,
            steepness: 8.0,
            smoothing_window: 10,
            max_backoff_multiplier: 3.0,
            max_pane_skip: 0.50,
            max_detection_skip: 0.25,
            min_buffer_fraction: 0.20,
        }
    }
}

// =============================================================================
// EMA Smoother
// =============================================================================

/// Exponential Moving Average smoother.
#[derive(Debug, Clone)]
pub struct EmaSmoother {
    /// Smoothing factor alpha = 2 / (window + 1).
    alpha: f64,
    /// Current smoothed value.
    value: f64,
    /// Whether at least one sample has been processed.
    initialized: bool,
}

impl EmaSmoother {
    /// Create a new EMA smoother with the given window size.
    #[must_use]
    pub fn new(window: usize) -> Self {
        let window = window.max(1);
        Self {
            alpha: 2.0 / (window as f64 + 1.0),
            value: 0.0,
            initialized: false,
        }
    }

    /// Feed a raw observation and return the smoothed value.
    pub fn update(&mut self, raw: f64) -> f64 {
        if !self.initialized {
            self.value = raw;
            self.initialized = true;
        } else {
            self.value = self.alpha * raw + (1.0 - self.alpha) * self.value;
        }
        self.value
    }

    /// Current smoothed value.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Whether the smoother has been initialized.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Reset to uninitialized state.
    pub fn reset(&mut self) {
        self.value = 0.0;
        self.initialized = false;
    }
}

// =============================================================================
// Severity Function
// =============================================================================

/// Compute the sigmoid function: σ(x) = 1 / (1 + e⁻ˣ).
#[inline]
fn sigmoid(x: f64) -> f64 {
    // Clamp input to avoid overflow
    let x = x.clamp(-500.0, 500.0);
    1.0 / (1.0 + (-x).exp())
}

/// Compute severity from a queue ratio using the sigmoid model.
///
/// - `q`: Queue ratio in [0, 1] (smoothed or raw).
/// - `center`: Center threshold θ.
/// - `steepness`: Steepness parameter k.
///
/// Returns severity in [0, 1].
#[inline]
pub fn severity(q: f64, center: f64, steepness: f64) -> f64 {
    sigmoid(steepness * (q - center))
}

// =============================================================================
// Throttle Actions
// =============================================================================

/// Continuous throttle actions derived from the severity value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThrottleActions {
    /// Current severity in [0.0, 1.0].
    pub severity: f64,

    /// Polling interval multiplier (1.0 = no backoff, 4.0 = max backoff).
    pub poll_multiplier: f64,

    /// Fraction of panes to skip (0.0 = none, up to max_pane_skip).
    /// Uses s² curve to delay shedding until severity is high.
    pub pane_skip_fraction: f64,

    /// Fraction of pattern detections to skip (0.0 = none, up to max_detection_skip).
    pub detection_skip_fraction: f64,

    /// Buffer limit as fraction of normal maximum (1.0 = full, down to min_buffer_fraction).
    pub buffer_limit_fraction: f64,

    /// Equivalent discrete tier for backward compatibility.
    pub equivalent_tier: BackpressureTier,
}

// =============================================================================
// Continuous Backpressure Engine
// =============================================================================

/// Continuous backpressure engine using sigmoid severity with EMA smoothing.
#[derive(Debug, Clone)]
pub struct ContinuousBackpressure {
    config: ContinuousBackpressureConfig,
    capture_smoother: EmaSmoother,
    write_smoother: EmaSmoother,
    /// The current smoothed queue ratio (max of capture and write).
    current_q: f64,
    /// The current severity value.
    current_severity: f64,
    /// Total number of updates.
    update_count: u64,
}

impl ContinuousBackpressure {
    /// Create a new continuous backpressure engine.
    #[must_use]
    pub fn new(config: ContinuousBackpressureConfig) -> Self {
        let window = config.smoothing_window;
        Self {
            config,
            capture_smoother: EmaSmoother::new(window),
            write_smoother: EmaSmoother::new(window),
            current_q: 0.0,
            current_severity: 0.0,
            update_count: 0,
        }
    }

    /// Update with new queue depth observations.
    ///
    /// - `capture_ratio`: Current capture channel depth / capacity (0.0–1.0).
    /// - `write_ratio`: Current write queue depth / capacity (0.0–1.0).
    ///
    /// Returns the new throttle actions.
    pub fn update(
        &mut self,
        capture_ratio: f64,
        write_ratio: f64,
    ) -> ThrottleActions {
        let capture_ratio = capture_ratio.clamp(0.0, 1.0);
        let write_ratio = write_ratio.clamp(0.0, 1.0);

        let smoothed_capture = self.capture_smoother.update(capture_ratio);
        let smoothed_write = self.write_smoother.update(write_ratio);

        self.current_q = smoothed_capture.max(smoothed_write);
        self.current_severity = severity(
            self.current_q,
            self.config.center_threshold,
            self.config.steepness,
        );
        self.update_count += 1;

        self.compute_actions()
    }

    /// Get current throttle actions without updating.
    #[must_use]
    pub fn current_actions(&self) -> ThrottleActions {
        self.compute_actions()
    }

    /// Current severity value in [0.0, 1.0].
    #[must_use]
    pub fn severity(&self) -> f64 {
        self.current_severity
    }

    /// Current smoothed queue ratio.
    #[must_use]
    pub fn queue_ratio(&self) -> f64 {
        self.current_q
    }

    /// Equivalent discrete tier for backward compatibility.
    #[must_use]
    pub fn equivalent_tier(&self) -> BackpressureTier {
        severity_to_tier(self.current_severity)
    }

    /// Total number of update calls.
    #[must_use]
    pub fn update_count(&self) -> u64 {
        self.update_count
    }

    /// Get config reference.
    #[must_use]
    pub fn config(&self) -> &ContinuousBackpressureConfig {
        &self.config
    }

    /// Reset all state.
    pub fn reset(&mut self) {
        self.capture_smoother.reset();
        self.write_smoother.reset();
        self.current_q = 0.0;
        self.current_severity = 0.0;
        self.update_count = 0;
    }

    fn compute_actions(&self) -> ThrottleActions {
        let s = self.current_severity;

        ThrottleActions {
            severity: s,
            poll_multiplier: 1.0 + self.config.max_backoff_multiplier * s,
            pane_skip_fraction: self.config.max_pane_skip * s * s, // Quadratic
            detection_skip_fraction: self.config.max_detection_skip * s,
            buffer_limit_fraction: 1.0 - (1.0 - self.config.min_buffer_fraction) * s,
            equivalent_tier: severity_to_tier(s),
        }
    }
}

/// Map a continuous severity value to a discrete BackpressureTier.
#[must_use]
pub fn severity_to_tier(s: f64) -> BackpressureTier {
    if s < 0.25 {
        BackpressureTier::Green
    } else if s < 0.60 {
        BackpressureTier::Yellow
    } else if s < 0.85 {
        BackpressureTier::Red
    } else {
        BackpressureTier::Black
    }
}

// =============================================================================
// Snapshot (for telemetry / serialization)
// =============================================================================

/// Serializable snapshot of the continuous backpressure state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackpressureSnapshot {
    pub severity: f64,
    pub queue_ratio: f64,
    pub equivalent_tier: BackpressureTier,
    pub actions: ThrottleActions,
    pub update_count: u64,
}

impl ContinuousBackpressure {
    /// Take a snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self) -> BackpressureSnapshot {
        BackpressureSnapshot {
            severity: self.current_severity,
            queue_ratio: self.current_q,
            equivalent_tier: self.equivalent_tier(),
            actions: self.current_actions(),
            update_count: self.update_count,
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sigmoid / severity function ─────────────────────────────────────

    #[test]
    fn sigmoid_at_zero() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn sigmoid_extremes() {
        assert!(sigmoid(100.0) > 0.999);
        assert!(sigmoid(-100.0) < 0.001);
    }

    #[test]
    fn sigmoid_monotonic() {
        let mut prev = sigmoid(-10.0);
        for i in -99..100 {
            let x = i as f64 * 0.1;
            let s = sigmoid(x);
            assert!(s >= prev, "sigmoid not monotonic at x={x}");
            prev = s;
        }
    }

    #[test]
    fn severity_at_center() {
        let s = severity(0.6, 0.6, 8.0);
        assert!((s - 0.5).abs() < 1e-10, "severity at center should be 0.5, got {s}");
    }

    #[test]
    fn severity_low_queue() {
        let s = severity(0.1, 0.6, 8.0);
        assert!(s < 0.1, "low queue ratio should give low severity, got {s}");
    }

    #[test]
    fn severity_high_queue() {
        let s = severity(0.95, 0.6, 8.0);
        assert!(s > 0.9, "high queue ratio should give high severity, got {s}");
    }

    #[test]
    fn severity_range() {
        for i in 0..=100 {
            let q = i as f64 / 100.0;
            let s = severity(q, 0.6, 8.0);
            assert!(
                (0.0..=1.0).contains(&s),
                "severity {s} out of [0,1] for q={q}"
            );
        }
    }

    #[test]
    fn severity_monotonic() {
        let mut prev = severity(0.0, 0.6, 8.0);
        for i in 1..=100 {
            let q = i as f64 / 100.0;
            let s = severity(q, 0.6, 8.0);
            assert!(s >= prev, "severity not monotonic at q={q}");
            prev = s;
        }
    }

    #[test]
    fn severity_steepness_effect() {
        // Higher steepness = sharper transition
        let s_gentle = severity(0.7, 0.6, 2.0);
        let s_steep = severity(0.7, 0.6, 20.0);
        // Both above 0.5 since q > center, but steep should be closer to 1
        assert!(s_steep > s_gentle, "steeper should give higher severity above center");
    }

    // ── EMA Smoother ────────────────────────────────────────────────────

    #[test]
    fn ema_first_value() {
        let mut ema = EmaSmoother::new(10);
        let v = ema.update(5.0);
        assert!((v - 5.0).abs() < f64::EPSILON);
        assert!(ema.is_initialized());
    }

    #[test]
    fn ema_converges_to_constant() {
        let mut ema = EmaSmoother::new(10);
        for _ in 0..100 {
            ema.update(42.0);
        }
        assert!(
            (ema.value() - 42.0).abs() < 0.01,
            "EMA should converge to constant, got {}",
            ema.value()
        );
    }

    #[test]
    fn ema_smooths_noise() {
        let mut ema = EmaSmoother::new(20);
        // Feed alternating 0 and 10
        for i in 0..100 {
            let raw = if i % 2 == 0 { 0.0 } else { 10.0 };
            ema.update(raw);
        }
        // Should converge near 5.0
        assert!(
            (ema.value() - 5.0).abs() < 1.5,
            "EMA should smooth toward 5.0, got {}",
            ema.value()
        );
    }

    #[test]
    fn ema_reset() {
        let mut ema = EmaSmoother::new(10);
        ema.update(100.0);
        assert!(ema.is_initialized());

        ema.reset();
        assert!(!ema.is_initialized());
        assert!((ema.value() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ema_window_1_tracks_instantly() {
        let mut ema = EmaSmoother::new(1);
        ema.update(10.0);
        let v = ema.update(20.0);
        assert!(
            (v - 20.0).abs() < f64::EPSILON,
            "window=1 should track instantly, got {v}"
        );
    }

    // ── Tier mapping ────────────────────────────────────────────────────

    #[test]
    fn tier_mapping_boundaries() {
        assert_eq!(severity_to_tier(0.0), BackpressureTier::Green);
        assert_eq!(severity_to_tier(0.24), BackpressureTier::Green);
        assert_eq!(severity_to_tier(0.25), BackpressureTier::Yellow);
        assert_eq!(severity_to_tier(0.59), BackpressureTier::Yellow);
        assert_eq!(severity_to_tier(0.60), BackpressureTier::Red);
        assert_eq!(severity_to_tier(0.84), BackpressureTier::Red);
        assert_eq!(severity_to_tier(0.85), BackpressureTier::Black);
        assert_eq!(severity_to_tier(1.0), BackpressureTier::Black);
    }

    #[test]
    fn tier_mapping_monotonic() {
        let mut prev_tier = severity_to_tier(0.0);
        for i in 1..=100 {
            let s = i as f64 / 100.0;
            let tier = severity_to_tier(s);
            assert!(tier >= prev_tier, "tier not monotonic at s={s}");
            prev_tier = tier;
        }
    }

    // ── ThrottleActions ─────────────────────────────────────────────────

    #[test]
    fn actions_at_zero_severity() {
        let bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        let a = bp.current_actions();
        assert!((a.severity - 0.0).abs() < 0.01);
        assert!((a.poll_multiplier - 1.0).abs() < 0.1);
        assert!(a.pane_skip_fraction < 0.01);
        assert!(a.detection_skip_fraction < 0.01);
        assert!((a.buffer_limit_fraction - 1.0).abs() < 0.01);
    }

    #[test]
    fn actions_poll_multiplier_range() {
        let config = ContinuousBackpressureConfig::default();
        let mut bp = ContinuousBackpressure::new(config);

        // Low load
        let a = bp.update(0.1, 0.1);
        assert!(a.poll_multiplier >= 1.0);
        assert!(a.poll_multiplier < 2.0);

        // High load
        for _ in 0..50 {
            bp.update(0.99, 0.99);
        }
        let a = bp.current_actions();
        assert!(a.poll_multiplier > 3.0);
        assert!(a.poll_multiplier <= 4.01);
    }

    #[test]
    fn actions_pane_skip_quadratic() {
        // Pane skip should be low for moderate severity due to s² curve
        let config = ContinuousBackpressureConfig::default();
        let mut bp = ContinuousBackpressure::new(config);

        // Push to moderate severity (~0.5)
        for _ in 0..50 {
            bp.update(0.6, 0.6);
        }

        let a = bp.current_actions();
        // At severity ~0.5, pane_skip = 0.5 * 0.5² = 0.125
        assert!(
            a.pane_skip_fraction < 0.2,
            "quadratic curve should keep pane skip low at moderate severity: {}",
            a.pane_skip_fraction
        );
    }

    #[test]
    fn actions_buffer_limit_decreases() {
        let config = ContinuousBackpressureConfig::default();
        let mut bp = ContinuousBackpressure::new(config);

        let a_low = bp.update(0.1, 0.1);

        for _ in 0..50 {
            bp.update(0.99, 0.99);
        }
        let a_high = bp.current_actions();

        assert!(
            a_high.buffer_limit_fraction < a_low.buffer_limit_fraction,
            "buffer limit should decrease with severity"
        );
    }

    // ── Continuous Backpressure engine ───────────────────────────────────

    #[test]
    fn engine_initial_state() {
        let bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        assert!((bp.severity() - 0.0).abs() < 0.01);
        assert!((bp.queue_ratio() - 0.0).abs() < f64::EPSILON);
        assert_eq!(bp.equivalent_tier(), BackpressureTier::Green);
        assert_eq!(bp.update_count(), 0);
    }

    #[test]
    fn engine_low_load_stays_green() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        for _ in 0..50 {
            bp.update(0.2, 0.1);
        }
        assert_eq!(bp.equivalent_tier(), BackpressureTier::Green);
        assert!(bp.severity() < 0.25);
    }

    #[test]
    fn engine_high_load_goes_critical() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        for _ in 0..50 {
            bp.update(0.95, 0.90);
        }
        assert!(bp.severity() > 0.85);
        assert_eq!(bp.equivalent_tier(), BackpressureTier::Black);
    }

    #[test]
    fn engine_smooth_transition() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());

        // Start low
        for _ in 0..20 {
            bp.update(0.1, 0.1);
        }
        let s1 = bp.severity();

        // Ramp up gradually
        let mut severity_values = Vec::new();
        for i in 0..20 {
            let ratio = 0.1 + (i as f64) * 0.04;
            bp.update(ratio, ratio);
            severity_values.push(bp.severity());
        }

        // Severity should increase smoothly (no big jumps)
        for window in severity_values.windows(2) {
            let jump = window[1] - window[0];
            assert!(
                jump < 0.3,
                "severity jump {} too large (not smooth)",
                jump
            );
        }

        // Overall should have increased
        let s2 = bp.severity();
        assert!(s2 > s1, "severity should increase from {s1} to {s2}");
    }

    #[test]
    fn engine_ema_provides_hysteresis() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());

        // Push to high severity
        for _ in 0..50 {
            bp.update(0.95, 0.95);
        }
        let high_severity = bp.severity();

        // Sudden drop to low load — severity should decrease gradually
        bp.update(0.1, 0.1);
        let after_one_low = bp.severity();
        assert!(
            after_one_low > high_severity * 0.5,
            "EMA should prevent instant severity drop: {} vs {}",
            after_one_low,
            high_severity
        );
    }

    #[test]
    fn engine_uses_max_of_capture_and_write() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());

        // High capture, low write
        for _ in 0..50 {
            bp.update(0.9, 0.1);
        }
        let s1 = bp.severity();

        // Low capture, high write
        let mut bp2 = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        for _ in 0..50 {
            bp2.update(0.1, 0.9);
        }
        let s2 = bp2.severity();

        // Both should have similar high severity
        assert!(s1 > 0.5, "high capture should raise severity: {s1}");
        assert!(s2 > 0.5, "high write should raise severity: {s2}");
    }

    #[test]
    fn engine_clamps_input() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        // Should not panic with out-of-range inputs
        let a = bp.update(1.5, -0.5);
        assert!((0.0..=1.0).contains(&a.severity));
    }

    #[test]
    fn engine_reset() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        for _ in 0..50 {
            bp.update(0.9, 0.9);
        }
        assert!(bp.severity() > 0.5);

        bp.reset();
        assert!((bp.severity() - 0.0).abs() < f64::EPSILON);
        assert_eq!(bp.update_count(), 0);
    }

    // ── Snapshot ────────────────────────────────────────────────────────

    #[test]
    fn snapshot_serializes() {
        let mut bp = ContinuousBackpressure::new(ContinuousBackpressureConfig::default());
        for _ in 0..10 {
            bp.update(0.5, 0.5);
        }
        let snap = bp.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: BackpressureSnapshot = serde_json::from_str(&json).unwrap();
        assert!((parsed.severity - snap.severity).abs() < f64::EPSILON);
        assert_eq!(parsed.update_count, 10);
    }

    // ── Config ──────────────────────────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = ContinuousBackpressureConfig::default();
        assert!((c.center_threshold - 0.60).abs() < f64::EPSILON);
        assert!((c.steepness - 8.0).abs() < f64::EPSILON);
        assert_eq!(c.smoothing_window, 10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = ContinuousBackpressureConfig {
            center_threshold: 0.50,
            steepness: 12.0,
            smoothing_window: 20,
            max_backoff_multiplier: 5.0,
            max_pane_skip: 0.70,
            max_detection_skip: 0.30,
            min_buffer_fraction: 0.10,
        };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: ContinuousBackpressureConfig = serde_json::from_str(&json).unwrap();
        assert!((parsed.center_threshold - 0.50).abs() < f64::EPSILON);
        assert!((parsed.steepness - 12.0).abs() < f64::EPSILON);
    }

    // ── Lipschitz continuity ────────────────────────────────────────────

    #[test]
    fn severity_lipschitz_continuity() {
        let k = 8.0;
        let center = 0.6;

        for i in 0..100 {
            let q = i as f64 / 100.0;
            let delta = 0.005;
            let s1 = severity(q, center, k);
            let s2 = severity(q + delta, center, k);

            // For sigmoid, max derivative = k/4 (at the center)
            // So |s2 - s1| should be <= delta * k/4 + epsilon
            let lipschitz_bound = delta * k / 4.0 + 0.001;
            assert!(
                (s2 - s1).abs() <= lipschitz_bound,
                "discontinuity at q={q}: |{s2} - {s1}| = {} > {lipschitz_bound}",
                (s2 - s1).abs()
            );
        }
    }
}
