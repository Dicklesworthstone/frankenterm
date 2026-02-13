//! Property-based tests for the `backpressure_severity` module.
//!
//! Covers `SeverityConfig` serde roundtrips, `ThrottleActions::from_severity`
//! range invariants and monotonicity, `SeverityConfig::ema_alpha` bounds,
//! and `ContinuousBackpressure` observe/severity behavioral properties.

use frankenterm_core::backpressure_severity::{
    ContinuousBackpressure, SeverityConfig, ThrottleActions,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_severity_config() -> impl Strategy<Value = SeverityConfig> {
    (
        0.01_f64..0.99,   // center_threshold
        0.1_f64..50.0,    // steepness
        1_usize..100,     // smoothing_window
    )
        .prop_map(|(center_threshold, steepness, smoothing_window)| SeverityConfig {
            center_threshold,
            steepness,
            smoothing_window,
        })
}

// =========================================================================
// SeverityConfig — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// SeverityConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde(config in arb_severity_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: SeverityConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((back.center_threshold - config.center_threshold).abs() < 1e-10);
        prop_assert!((back.steepness - config.steepness).abs() < 1e-10);
        prop_assert_eq!(back.smoothing_window, config.smoothing_window);
    }

    /// Default config has documented values.
    #[test]
    fn prop_default_config(_dummy in 0..1_u8) {
        let config = SeverityConfig::default();
        prop_assert!((config.center_threshold - 0.60).abs() < f64::EPSILON);
        prop_assert!((config.steepness - 8.0).abs() < f64::EPSILON);
        prop_assert_eq!(config.smoothing_window, 10);
    }
}

// =========================================================================
// SeverityConfig::ema_alpha — bounds
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ema_alpha is always in (0, 1].
    #[test]
    fn prop_ema_alpha_bounded(config in arb_severity_config()) {
        let alpha = config.ema_alpha();
        prop_assert!(alpha > 0.0, "alpha {} should be > 0", alpha);
        prop_assert!(alpha <= 1.0, "alpha {} should be <= 1", alpha);
    }

    /// ema_alpha decreases as smoothing_window increases (more smoothing).
    #[test]
    fn prop_ema_alpha_inversely_proportional(
        small_window in 1_usize..10,
        large_window in 50_usize..100,
    ) {
        let small = SeverityConfig { smoothing_window: small_window, ..Default::default() };
        let large = SeverityConfig { smoothing_window: large_window, ..Default::default() };
        prop_assert!(
            small.ema_alpha() >= large.ema_alpha(),
            "smaller window should have >= alpha: {} < {}",
            small.ema_alpha(), large.ema_alpha()
        );
    }

    /// ema_alpha for window=1 gives maximum responsiveness (alpha = 1.0).
    #[test]
    fn prop_ema_alpha_window_one(_dummy in 0..1_u8) {
        let config = SeverityConfig { smoothing_window: 1, ..Default::default() };
        prop_assert!((config.ema_alpha() - 1.0).abs() < f64::EPSILON);
    }
}

// =========================================================================
// ThrottleActions::from_severity — range invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Severity field is clamped to [0, 1].
    #[test]
    fn prop_throttle_severity_clamped(s in -1.0_f64..2.0) {
        let actions = ThrottleActions::from_severity(s);
        prop_assert!(actions.severity >= 0.0, "severity {} < 0", actions.severity);
        prop_assert!(actions.severity <= 1.0, "severity {} > 1", actions.severity);
    }

    /// poll_backoff_multiplier is in [1.0, 4.0].
    #[test]
    fn prop_poll_backoff_range(s in 0.0_f64..1.0) {
        let actions = ThrottleActions::from_severity(s);
        prop_assert!(actions.poll_backoff_multiplier >= 1.0, "poll_backoff {} < 1", actions.poll_backoff_multiplier);
        prop_assert!(actions.poll_backoff_multiplier <= 4.0, "poll_backoff {} > 4", actions.poll_backoff_multiplier);
    }

    /// pane_skip_fraction is in [0.0, 0.5].
    #[test]
    fn prop_pane_skip_range(s in 0.0_f64..1.0) {
        let actions = ThrottleActions::from_severity(s);
        prop_assert!(actions.pane_skip_fraction >= 0.0, "pane_skip {} < 0", actions.pane_skip_fraction);
        prop_assert!(actions.pane_skip_fraction <= 0.5, "pane_skip {} > 0.5", actions.pane_skip_fraction);
    }

    /// detection_skip_fraction is in [0.0, 0.25].
    #[test]
    fn prop_detection_skip_range(s in 0.0_f64..1.0) {
        let actions = ThrottleActions::from_severity(s);
        prop_assert!(actions.detection_skip_fraction >= 0.0);
        prop_assert!(actions.detection_skip_fraction <= 0.25);
    }

    /// buffer_limit_factor is in [0.2, 1.0].
    #[test]
    fn prop_buffer_limit_range(s in 0.0_f64..1.0) {
        let actions = ThrottleActions::from_severity(s);
        prop_assert!(actions.buffer_limit_factor >= 0.2 - f64::EPSILON);
        prop_assert!(actions.buffer_limit_factor <= 1.0 + f64::EPSILON);
    }

    /// Zero severity gives no throttling.
    #[test]
    fn prop_zero_severity_no_throttle(_dummy in 0..1_u8) {
        let actions = ThrottleActions::from_severity(0.0);
        prop_assert!((actions.poll_backoff_multiplier - 1.0).abs() < f64::EPSILON);
        prop_assert!((actions.pane_skip_fraction - 0.0).abs() < f64::EPSILON);
        prop_assert!((actions.detection_skip_fraction - 0.0).abs() < f64::EPSILON);
        prop_assert!((actions.buffer_limit_factor - 1.0).abs() < f64::EPSILON);
    }

    /// Full severity gives maximum throttling.
    #[test]
    fn prop_full_severity_max_throttle(_dummy in 0..1_u8) {
        let actions = ThrottleActions::from_severity(1.0);
        prop_assert!((actions.poll_backoff_multiplier - 4.0).abs() < f64::EPSILON);
        prop_assert!((actions.pane_skip_fraction - 0.5).abs() < f64::EPSILON);
        prop_assert!((actions.detection_skip_fraction - 0.25).abs() < f64::EPSILON);
        prop_assert!((actions.buffer_limit_factor - 0.2).abs() < f64::EPSILON);
    }
}

// =========================================================================
// ThrottleActions::from_severity — monotonicity
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// poll_backoff increases with severity.
    #[test]
    fn prop_poll_backoff_monotonic(a in 0.0_f64..1.0, b in 0.0_f64..1.0) {
        if a <= b {
            let ta = ThrottleActions::from_severity(a);
            let tb = ThrottleActions::from_severity(b);
            prop_assert!(
                tb.poll_backoff_multiplier >= ta.poll_backoff_multiplier - f64::EPSILON,
                "poll_backoff not monotonic: s={} gives {} > s={} gives {}",
                a, ta.poll_backoff_multiplier, b, tb.poll_backoff_multiplier
            );
        }
    }

    /// pane_skip increases with severity.
    #[test]
    fn prop_pane_skip_monotonic(a in 0.0_f64..1.0, b in 0.0_f64..1.0) {
        if a <= b {
            let ta = ThrottleActions::from_severity(a);
            let tb = ThrottleActions::from_severity(b);
            prop_assert!(
                tb.pane_skip_fraction >= ta.pane_skip_fraction - f64::EPSILON,
                "pane_skip not monotonic"
            );
        }
    }

    /// buffer_limit decreases with severity.
    #[test]
    fn prop_buffer_limit_inversely_monotonic(a in 0.0_f64..1.0, b in 0.0_f64..1.0) {
        if a <= b {
            let ta = ThrottleActions::from_severity(a);
            let tb = ThrottleActions::from_severity(b);
            prop_assert!(
                tb.buffer_limit_factor <= ta.buffer_limit_factor + f64::EPSILON,
                "buffer_limit not inversely monotonic"
            );
        }
    }
}

// =========================================================================
// ThrottleActions — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ThrottleActions serde roundtrip preserves all fields.
    #[test]
    fn prop_throttle_actions_serde(s in 0.0_f64..1.0) {
        let actions = ThrottleActions::from_severity(s);
        let json = serde_json::to_string(&actions).unwrap();
        let back: ThrottleActions = serde_json::from_str(&json).unwrap();
        prop_assert!((back.severity - actions.severity).abs() < 1e-10);
        prop_assert!((back.poll_backoff_multiplier - actions.poll_backoff_multiplier).abs() < 1e-10);
        prop_assert!((back.pane_skip_fraction - actions.pane_skip_fraction).abs() < 1e-10);
        prop_assert!((back.detection_skip_fraction - actions.detection_skip_fraction).abs() < 1e-10);
        prop_assert!((back.buffer_limit_factor - actions.buffer_limit_factor).abs() < 1e-10);
    }
}

// =========================================================================
// ContinuousBackpressure — observe/severity
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// New ContinuousBackpressure severity is bounded in [0, 1].
    /// Initial severity is sigmoid(-steepness * center_threshold), which is
    /// near 0 for typical configs but not exactly 0.
    #[test]
    fn prop_new_severity_bounded(config in arb_severity_config()) {
        let bp = ContinuousBackpressure::new(config);
        let s = bp.severity();
        prop_assert!(s >= 0.0, "initial severity {} < 0", s);
        prop_assert!(s <= 1.0, "initial severity {} > 1", s);
    }

    /// observe_ratio clamps severity to [0, 1].
    #[test]
    fn prop_observe_ratio_severity_bounded(
        config in arb_severity_config(),
        ratio in -0.5_f64..1.5,
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        bp.observe_ratio(ratio);
        let s = bp.severity();
        prop_assert!(s >= 0.0, "severity {} < 0", s);
        prop_assert!(s <= 1.0, "severity {} > 1", s);
    }

    /// Repeated zero-ratio observations drive smoothed_ratio toward 0.
    /// With center > 0 and steepness > 1, severity follows suit.
    #[test]
    fn prop_zero_load_drives_ratio_down(config in arb_severity_config()) {
        let mut bp = ContinuousBackpressure::new(config);
        // First spike
        bp.observe_ratio(1.0);
        let initial_ratio = bp.smoothed_ratio();
        // Many zero observations
        for _ in 0..200 {
            bp.observe_ratio(0.0);
        }
        prop_assert!(
            bp.smoothed_ratio() < initial_ratio,
            "smoothed_ratio should decrease from {} to {}", initial_ratio, bp.smoothed_ratio()
        );
    }

    /// with_defaults creates a working instance with low initial severity.
    #[test]
    fn prop_with_defaults_works(_dummy in 0..1_u8) {
        let bp = ContinuousBackpressure::with_defaults();
        // Initial severity is sigmoid(-8.0 * 0.6) = sigmoid(-4.8) ≈ 0.008
        // Not exactly 0 but very low.
        prop_assert!(bp.severity() < 0.05, "initial severity {} should be < 0.05", bp.severity());
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn throttle_at_zero() {
    let actions = ThrottleActions::from_severity(0.0);
    assert!((actions.poll_backoff_multiplier - 1.0).abs() < f64::EPSILON);
}

#[test]
fn throttle_at_one() {
    let actions = ThrottleActions::from_severity(1.0);
    assert!((actions.poll_backoff_multiplier - 4.0).abs() < f64::EPSILON);
}

#[test]
fn config_default_serde_roundtrip() {
    let config = SeverityConfig::default();
    let json = serde_json::to_string(&config).unwrap();
    let back: SeverityConfig = serde_json::from_str(&json).unwrap();
    assert!((back.center_threshold - 0.60).abs() < f64::EPSILON);
}
