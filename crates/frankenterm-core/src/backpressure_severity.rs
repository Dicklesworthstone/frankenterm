//! Continuous backpressure severity function — smooth load-proportional throttling.
//!
//! Replaces the discrete 4-tier FSM with a continuous severity value in [0, 1],
//! enabling proportional throttling actions instead of step-function transitions.
//!
//! # Severity function
//!
//! ```text
//! s(t) = sigmoid(k * (q(t) - θ))
//! ```
//!
//! where `q(t)` is the EMA-smoothed queue ratio, `θ` is the center threshold
//! (severity = 0.5 when load = θ), and `k` controls steepness.
//!
//! # Throttling actions
//!
//! | Action               | Formula                         | Range       |
//! |----------------------|---------------------------------|-------------|
//! | Poll backoff mult    | 1.0 + 3.0 × s                  | 1× to 4×   |
//! | Pane skip fraction   | 0.5 × s²                       | 0% to 50%  |
//! | Detection skip       | 0.25 × s                        | 0% to 25%  |
//! | Buffer limit factor  | 1.0 − 0.8 × s                  | 100% to 20% |
//!
//! # Backward compatibility
//!
//! [`ContinuousBackpressure::equivalent_tier`] maps the continuous severity
//! back to [`BackpressureTier`] for existing consumers.

use serde::{Deserialize, Serialize};

use crate::backpressure::{BackpressureTier, QueueDepths};

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the continuous backpressure severity model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeverityConfig {
    /// Queue ratio where severity = 0.5. Default: 0.60.
    pub center_threshold: f64,
    /// Sigmoid steepness parameter. Higher = sharper transition. Default: 8.0.
    pub steepness: f64,
    /// EMA smoothing window in samples. Default: 10.
    pub smoothing_window: usize,
}

impl Default for SeverityConfig {
    fn default() -> Self {
        Self {
            center_threshold: 0.60,
            steepness: 8.0,
            smoothing_window: 10,
        }
    }
}

impl SeverityConfig {
    /// EMA alpha derived from smoothing window: alpha = 2 / (N + 1).
    pub fn ema_alpha(&self) -> f64 {
        let n = self.smoothing_window.max(1) as f64;
        2.0 / (n + 1.0)
    }
}

// =============================================================================
// Throttle actions
// =============================================================================

/// Proportional throttling actions derived from the current severity.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ThrottleActions {
    /// Raw severity value in [0, 1].
    pub severity: f64,
    /// Poll interval multiplier (1.0 = normal, up to 4.0).
    pub poll_backoff_multiplier: f64,
    /// Fraction of lowest-priority panes to skip (0.0 to 0.5).
    pub pane_skip_fraction: f64,
    /// Fraction of detection work to skip (0.0 to 0.25).
    pub detection_skip_fraction: f64,
    /// Buffer limit as fraction of normal capacity (1.0 to 0.2).
    pub buffer_limit_factor: f64,
}

impl ThrottleActions {
    /// Compute throttle actions from a severity value.
    pub fn from_severity(severity: f64) -> Self {
        let s = severity.clamp(0.0, 1.0);
        Self {
            severity: s,
            poll_backoff_multiplier: 3.0f64.mul_add(s, 1.0),
            pane_skip_fraction: 0.5 * s * s,
            detection_skip_fraction: 0.25 * s,
            buffer_limit_factor: 0.8f64.mul_add(-s, 1.0),
        }
    }
}

// =============================================================================
// Continuous backpressure model
// =============================================================================

/// Continuous backpressure severity model with EMA smoothing.
///
/// Call [`observe`] with each queue depth sample to update the smoothed
/// ratio and severity. Read the current state with [`severity`],
/// [`throttle_actions`], or [`equivalent_tier`].
#[derive(Debug, Clone)]
pub struct ContinuousBackpressure {
    config: SeverityConfig,
    /// EMA-smoothed queue ratio.
    smoothed_ratio: f64,
    /// Number of observations seen so far (for warm-up).
    observation_count: u64,
}

impl ContinuousBackpressure {
    /// Create a new continuous backpressure model.
    pub fn new(config: SeverityConfig) -> Self {
        Self {
            config,
            smoothed_ratio: 0.0,
            observation_count: 0,
        }
    }

    /// Create with default config.
    pub fn with_defaults() -> Self {
        Self::new(SeverityConfig::default())
    }

    /// Observe a new queue depth sample. Updates the EMA-smoothed ratio
    /// and returns the new severity.
    pub fn observe(&mut self, depths: &QueueDepths) -> f64 {
        let raw_ratio = depths.capture_ratio().max(depths.write_ratio());
        self.observe_ratio(raw_ratio)
    }

    /// Observe a raw queue ratio directly (useful for testing).
    pub fn observe_ratio(&mut self, raw_ratio: f64) -> f64 {
        let ratio = raw_ratio.clamp(0.0, 1.0);
        let alpha = self.config.ema_alpha();

        if self.observation_count == 0 {
            // Initialize EMA with first observation.
            self.smoothed_ratio = ratio;
        } else {
            self.smoothed_ratio = alpha.mul_add(ratio, (1.0 - alpha) * self.smoothed_ratio);
        }
        self.observation_count += 1;

        self.severity()
    }

    /// Current severity in [0, 1].
    pub fn severity(&self) -> f64 {
        sigmoid(self.config.steepness * (self.smoothed_ratio - self.config.center_threshold))
    }

    /// Current EMA-smoothed queue ratio.
    pub fn smoothed_ratio(&self) -> f64 {
        self.smoothed_ratio
    }

    /// Number of observations processed.
    pub fn observation_count(&self) -> u64 {
        self.observation_count
    }

    /// Compute proportional throttle actions from current severity.
    pub fn throttle_actions(&self) -> ThrottleActions {
        ThrottleActions::from_severity(self.severity())
    }

    /// Map current severity back to discrete [`BackpressureTier`].
    pub fn equivalent_tier(&self) -> BackpressureTier {
        let s = self.severity();
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

    /// Reset the model to its initial state.
    pub fn reset(&mut self) {
        self.smoothed_ratio = 0.0;
        self.observation_count = 0;
    }

    /// Read-only access to the config.
    pub fn config(&self) -> &SeverityConfig {
        &self.config
    }
}

// =============================================================================
// Sigmoid function
// =============================================================================

/// Standard logistic sigmoid: 1 / (1 + exp(-x)).
///
/// Clamped to avoid numerical overflow for extreme inputs.
fn sigmoid(x: f64) -> f64 {
    if x.is_nan() {
        return 0.5;
    }
    if x > 30.0 {
        return 1.0;
    }
    if x < -30.0 {
        return 0.0;
    }
    1.0 / (1.0 + (-x).exp())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- Sigmoid tests --

    #[test]
    fn sigmoid_at_zero_is_half() {
        let v = sigmoid(0.0);
        assert!((v - 0.5).abs() < 1e-10);
    }

    #[test]
    fn sigmoid_monotonic() {
        let mut prev = sigmoid(-20.0);
        for i in -199..200 {
            let x = i as f64 / 10.0;
            let v = sigmoid(x);
            assert!(v >= prev - f64::EPSILON, "sigmoid not monotonic at x={x}");
            prev = v;
        }
    }

    #[test]
    fn sigmoid_range() {
        for i in -500..500 {
            let x = i as f64 / 10.0;
            let v = sigmoid(x);
            assert!((0.0..=1.0).contains(&v), "sigmoid({x})={v} out of range");
        }
    }

    #[test]
    fn sigmoid_nan_returns_half() {
        assert_eq!(sigmoid(f64::NAN), 0.5);
    }

    #[test]
    fn sigmoid_extreme_values() {
        assert_eq!(sigmoid(100.0), 1.0);
        assert_eq!(sigmoid(-100.0), 0.0);
    }

    // -- SeverityConfig tests --

    #[test]
    fn config_default_values() {
        let c = SeverityConfig::default();
        assert!((c.center_threshold - 0.60).abs() < 1e-10);
        assert!((c.steepness - 8.0).abs() < 1e-10);
        assert_eq!(c.smoothing_window, 10);
    }

    #[test]
    fn config_ema_alpha() {
        let c = SeverityConfig {
            smoothing_window: 10,
            ..Default::default()
        };
        let alpha = c.ema_alpha();
        // alpha = 2/(10+1) ≈ 0.1818
        assert!((alpha - 2.0 / 11.0).abs() < 1e-10);
    }

    #[test]
    fn config_ema_alpha_window_one() {
        let c = SeverityConfig {
            smoothing_window: 1,
            ..Default::default()
        };
        assert!((c.ema_alpha() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn config_ema_alpha_window_zero() {
        let c = SeverityConfig {
            smoothing_window: 0,
            ..Default::default()
        };
        // Max(1, 0) = 1, alpha = 2/2 = 1.0
        assert!((c.ema_alpha() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let c = SeverityConfig {
            center_threshold: 0.75,
            steepness: 12.0,
            smoothing_window: 5,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: SeverityConfig = serde_json::from_str(&json).unwrap();
        assert!((back.center_threshold - 0.75).abs() < 1e-10);
        assert!((back.steepness - 12.0).abs() < 1e-10);
        assert_eq!(back.smoothing_window, 5);
    }

    // -- ThrottleActions tests --

    #[test]
    fn throttle_at_zero_severity() {
        let a = ThrottleActions::from_severity(0.0);
        assert!((a.poll_backoff_multiplier - 1.0).abs() < 1e-10);
        assert!((a.pane_skip_fraction).abs() < 1e-10);
        assert!((a.detection_skip_fraction).abs() < 1e-10);
        assert!((a.buffer_limit_factor - 1.0).abs() < 1e-10);
    }

    #[test]
    fn throttle_at_full_severity() {
        let a = ThrottleActions::from_severity(1.0);
        assert!((a.poll_backoff_multiplier - 4.0).abs() < 1e-10);
        assert!((a.pane_skip_fraction - 0.5).abs() < 1e-10);
        assert!((a.detection_skip_fraction - 0.25).abs() < 1e-10);
        assert!((a.buffer_limit_factor - 0.2).abs() < 1e-10);
    }

    #[test]
    fn throttle_at_half_severity() {
        let a = ThrottleActions::from_severity(0.5);
        assert!((a.poll_backoff_multiplier - 2.5).abs() < 1e-10);
        assert!((a.pane_skip_fraction - 0.125).abs() < 1e-10); // 0.5 * 0.25
        assert!((a.detection_skip_fraction - 0.125).abs() < 1e-10);
        assert!((a.buffer_limit_factor - 0.6).abs() < 1e-10);
    }

    #[test]
    fn throttle_clamps_input() {
        let below = ThrottleActions::from_severity(-0.5);
        assert!((below.severity).abs() < 1e-10);
        let above = ThrottleActions::from_severity(1.5);
        assert!((above.severity - 1.0).abs() < 1e-10);
    }

    // -- ContinuousBackpressure tests --

    #[test]
    fn model_starts_at_zero() {
        let m = ContinuousBackpressure::with_defaults();
        assert_eq!(m.observation_count(), 0);
        // Smoothed ratio = 0, severity = sigmoid(8 * (0 - 0.6)) = sigmoid(-4.8) ≈ 0.008
        let s = m.severity();
        assert!(s < 0.02, "initial severity should be near 0, got {s}");
    }

    #[test]
    fn model_first_observation_initializes() {
        let mut m = ContinuousBackpressure::with_defaults();
        m.observe_ratio(0.5);
        assert!((m.smoothed_ratio() - 0.5).abs() < 1e-10);
        assert_eq!(m.observation_count(), 1);
    }

    #[test]
    fn model_severity_at_center() {
        let config = SeverityConfig {
            center_threshold: 0.50,
            steepness: 8.0,
            smoothing_window: 1, // alpha=1 → no smoothing
        };
        let mut m = ContinuousBackpressure::new(config);
        m.observe_ratio(0.5);
        // sigmoid(8 * (0.5 - 0.5)) = sigmoid(0) = 0.5
        assert!((m.severity() - 0.5).abs() < 1e-10);
    }

    #[test]
    fn model_severity_increases_with_load() {
        let config = SeverityConfig {
            smoothing_window: 1, // No smoothing for clarity.
            ..Default::default()
        };
        let mut m = ContinuousBackpressure::new(config);
        m.observe_ratio(0.3);
        let s_low = m.severity();
        m.observe_ratio(0.9);
        let s_high = m.severity();
        assert!(s_high > s_low, "severity should increase with load");
    }

    #[test]
    fn model_ema_smoothing() {
        let config = SeverityConfig {
            smoothing_window: 10, // alpha = 2/11 ≈ 0.182
            ..Default::default()
        };
        let mut m = ContinuousBackpressure::new(config.clone());

        // First obs = 0.0
        m.observe_ratio(0.0);
        assert!((m.smoothed_ratio()).abs() < 1e-10);

        // Spike to 1.0
        m.observe_ratio(1.0);
        let alpha = config.ema_alpha();
        let expected = alpha * 1.0 + (1.0 - alpha) * 0.0;
        assert!(
            (m.smoothed_ratio() - expected).abs() < 1e-10,
            "smoothed={} expected={}",
            m.smoothed_ratio(),
            expected
        );
    }

    #[test]
    fn model_ema_convergence() {
        let config = SeverityConfig {
            smoothing_window: 10,
            ..Default::default()
        };
        let mut m = ContinuousBackpressure::new(config);

        // Feed constant 0.7 for many samples.
        for _ in 0..100 {
            m.observe_ratio(0.7);
        }
        // Should converge to 0.7.
        assert!(
            (m.smoothed_ratio() - 0.7).abs() < 0.01,
            "smoothed={} should converge to 0.7",
            m.smoothed_ratio()
        );
    }

    #[test]
    fn model_with_queue_depths() {
        let mut m = ContinuousBackpressure::with_defaults();
        let depths = QueueDepths {
            capture_depth: 70,
            capture_capacity: 100,
            write_depth: 50,
            write_capacity: 100,
        };
        m.observe(&depths);
        // Max ratio = 0.7 (capture), first obs → smoothed = 0.7
        assert!((m.smoothed_ratio() - 0.7).abs() < 1e-10);
    }

    #[test]
    fn model_reset() {
        let mut m = ContinuousBackpressure::with_defaults();
        m.observe_ratio(0.8);
        m.observe_ratio(0.9);
        m.reset();
        assert_eq!(m.observation_count(), 0);
        assert!((m.smoothed_ratio()).abs() < 1e-10);
    }

    // -- Tier mapping tests --

    #[test]
    fn tier_mapping_green() {
        let config = SeverityConfig {
            smoothing_window: 1,
            ..Default::default()
        };
        let mut m = ContinuousBackpressure::new(config);
        m.observe_ratio(0.0);
        assert_eq!(m.equivalent_tier(), BackpressureTier::Green);
    }

    #[test]
    fn tier_mapping_yellow() {
        let config = SeverityConfig {
            center_threshold: 0.50,
            steepness: 8.0,
            smoothing_window: 1,
        };
        let mut m = ContinuousBackpressure::new(config);
        m.observe_ratio(0.50);
        // severity = 0.5 → Yellow (0.25..0.60)
        assert_eq!(m.equivalent_tier(), BackpressureTier::Yellow);
    }

    #[test]
    fn tier_mapping_red() {
        let config = SeverityConfig {
            center_threshold: 0.50,
            steepness: 8.0,
            smoothing_window: 1,
        };
        let mut m = ContinuousBackpressure::new(config);
        // sigmoid(8*(0.60-0.50)) = sigmoid(0.8) ≈ 0.69 → Red (0.60..0.85)
        m.observe_ratio(0.60);
        let s = m.severity();
        assert!(s >= 0.60 && s < 0.85, "severity={s} should be in Red range");
        assert_eq!(m.equivalent_tier(), BackpressureTier::Red);
    }

    #[test]
    fn tier_mapping_black() {
        let config = SeverityConfig {
            center_threshold: 0.50,
            steepness: 20.0,
            smoothing_window: 1,
        };
        let mut m = ContinuousBackpressure::new(config);
        m.observe_ratio(0.80);
        // sigmoid(20*(0.8-0.5)) = sigmoid(6.0) ≈ 0.9975 → Black
        assert_eq!(m.equivalent_tier(), BackpressureTier::Black);
    }

    #[test]
    fn tier_ordering_monotonic() {
        let config = SeverityConfig {
            smoothing_window: 1,
            ..Default::default()
        };
        let mut prev_tier = BackpressureTier::Green;
        for i in 0..=100 {
            let ratio = i as f64 / 100.0;
            let mut m = ContinuousBackpressure::new(config.clone());
            m.observe_ratio(ratio);
            let tier = m.equivalent_tier();
            assert!(
                tier >= prev_tier,
                "tier decreased from {prev_tier:?} to {tier:?} at ratio={ratio}"
            );
            prev_tier = tier;
        }
    }

    // -- Smoke test: load spike scenario --

    #[test]
    fn load_spike_smooth_response() {
        let mut m = ContinuousBackpressure::with_defaults();

        // Warm up at low load.
        for _ in 0..20 {
            m.observe_ratio(0.2);
        }
        let s_low = m.severity();
        assert!(s_low < 0.1, "low load severity={s_low}");

        // Spike to high load.
        let mut severities = vec![];
        for _ in 0..20 {
            let s = m.observe_ratio(0.9);
            severities.push(s);
        }

        // Severity should increase smoothly (no jumps > 0.3 between consecutive samples).
        for window in severities.windows(2) {
            let diff = (window[1] - window[0]).abs();
            assert!(
                diff < 0.3,
                "severity jump too large: {} -> {} (diff={})",
                window[0],
                window[1],
                diff,
            );
        }

        // Final severity should be high.
        let s_final = *severities.last().unwrap();
        assert!(s_final > 0.5, "final severity={s_final} should be elevated");
    }

    // -- Proptest --

    mod proptest_severity {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Severity is monotonically increasing with queue ratio.
            #[test]
            fn severity_monotonicity(
                q1 in 0.0f64..1.0,
                q2 in 0.0f64..1.0,
                center in 0.1f64..0.9,
                steepness in 1.0f64..20.0,
            ) {
                let config = SeverityConfig {
                    center_threshold: center,
                    steepness,
                    smoothing_window: 1,
                };
                let (lo, hi) = if q1 <= q2 { (q1, q2) } else { (q2, q1) };

                let mut m_lo = ContinuousBackpressure::new(config.clone());
                m_lo.observe_ratio(lo);
                let mut m_hi = ContinuousBackpressure::new(config);
                m_hi.observe_ratio(hi);

                prop_assert!(
                    m_lo.severity() <= m_hi.severity() + f64::EPSILON,
                    "severity({lo}) = {} > severity({hi}) = {}",
                    m_lo.severity(),
                    m_hi.severity(),
                );
            }

            /// Severity is always in [0, 1].
            #[test]
            fn severity_range(
                q in 0.0f64..1.0,
                center in 0.1f64..0.9,
                steepness in 1.0f64..20.0,
            ) {
                let config = SeverityConfig {
                    center_threshold: center,
                    steepness,
                    smoothing_window: 1,
                };
                let mut m = ContinuousBackpressure::new(config);
                m.observe_ratio(q);
                let s = m.severity();
                prop_assert!(s >= 0.0 && s <= 1.0, "severity={s} out of range");
            }

            /// Severity continuity (Lipschitz bound).
            #[test]
            fn severity_continuity(
                q in 0.0f64..0.99,
                delta in 0.0001f64..0.01,
                steepness in 1.0f64..20.0,
            ) {
                let center = 0.6;
                let config = SeverityConfig {
                    center_threshold: center,
                    steepness,
                    smoothing_window: 1,
                };
                let mut m1 = ContinuousBackpressure::new(config.clone());
                m1.observe_ratio(q);
                let mut m2 = ContinuousBackpressure::new(config);
                m2.observe_ratio((q + delta).min(1.0));

                let diff = (m1.severity() - m2.severity()).abs();
                // Sigmoid derivative max is k/4, so Lipschitz constant = k/4
                let bound = delta * steepness / 4.0 + 1e-10;
                prop_assert!(
                    diff <= bound,
                    "discontinuity: |s({q}) - s({})| = {diff} > {bound}",
                    q + delta,
                );
            }

            /// EMA converges after enough identical observations.
            #[test]
            fn ema_convergence(
                target in 0.0f64..1.0,
                n in 20u32..100,
            ) {
                let config = SeverityConfig {
                    smoothing_window: 10,
                    ..Default::default()
                };
                let mut m = ContinuousBackpressure::new(config);
                for _ in 0..n {
                    m.observe_ratio(target);
                }
                prop_assert!(
                    (m.smoothed_ratio() - target).abs() < 0.01,
                    "smoothed={} should converge to target={target}",
                    m.smoothed_ratio(),
                );
            }

            /// Tier mapping is consistent (increasing severity → non-decreasing tier).
            #[test]
            fn tier_mapping_consistency(
                q1 in 0.0f64..1.0,
                q2 in 0.0f64..1.0,
            ) {
                let config = SeverityConfig {
                    smoothing_window: 1,
                    ..Default::default()
                };
                let (lo, hi) = if q1 <= q2 { (q1, q2) } else { (q2, q1) };
                let mut m_lo = ContinuousBackpressure::new(config.clone());
                m_lo.observe_ratio(lo);
                let mut m_hi = ContinuousBackpressure::new(config);
                m_hi.observe_ratio(hi);
                prop_assert!(
                    m_lo.equivalent_tier() <= m_hi.equivalent_tier(),
                    "tier({lo})={:?} > tier({hi})={:?}",
                    m_lo.equivalent_tier(),
                    m_hi.equivalent_tier(),
                );
            }

            /// Throttle actions are monotonic with severity.
            #[test]
            fn throttle_actions_monotonic(
                s1 in 0.0f64..1.0,
                s2 in 0.0f64..1.0,
            ) {
                let (lo, hi) = if s1 <= s2 { (s1, s2) } else { (s2, s1) };
                let a_lo = ThrottleActions::from_severity(lo);
                let a_hi = ThrottleActions::from_severity(hi);

                // All throttle actions should increase with severity.
                prop_assert!(a_lo.poll_backoff_multiplier <= a_hi.poll_backoff_multiplier + f64::EPSILON);
                prop_assert!(a_lo.pane_skip_fraction <= a_hi.pane_skip_fraction + f64::EPSILON);
                prop_assert!(a_lo.detection_skip_fraction <= a_hi.detection_skip_fraction + f64::EPSILON);
                // Buffer limit factor DECREASES with severity.
                prop_assert!(a_lo.buffer_limit_factor >= a_hi.buffer_limit_factor - f64::EPSILON);
            }
        }
    }
}
