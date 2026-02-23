//! Property-based tests for ARS e-value drift detection.
//!
//! Verifies e-value martingale properties, calibration invariants,
//! tier promotion/demotion integration, and serde roundtrips.

use proptest::prelude::*;

use frankenterm_core::ars_drift::{
    ArsDriftDetector, ArsDriftStats, DriftAction,
    DriftVerdict, EValueConfig, EValueMonitor,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_config() -> impl Strategy<Value = EValueConfig> {
    (
        0.001..0.2f64,    // alpha
        3..20usize,       // min_calibration
        20..200usize,     // calibration_window
        0.001..0.5f64,    // min_lambda
        0.5..0.99f64,     // max_lambda
        0.95..1.0f64,     // decay
    )
        .prop_map(|(alpha, min_cal, cal_win, min_l, max_l, decay)| EValueConfig {
            alpha,
            min_calibration: min_cal,
            calibration_window: cal_win,
            min_lambda: min_l,
            max_lambda: max_l,
            decay,
            auto_reset_on_drift: true,
        })
}

fn arb_outcome_sequence(min: usize, max: usize) -> impl Strategy<Value = Vec<bool>> {
    prop::collection::vec(prop::bool::ANY, min..=max)
}


// =============================================================================
// Calibration invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn insufficient_until_min_calibration(
        min_cal in 3..15usize,
        outcomes in arb_outcome_sequence(1, 14),
    ) {
        let config = EValueConfig {
            min_calibration: min_cal,
            calibration_window: 100,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        for (i, &outcome) in outcomes.iter().enumerate() {
            if i >= min_cal {
                break;
            }
            let v = monitor.observe(if outcome { 1.0 } else { 0.0 }, &config);
            if i + 1 < min_cal {
                let is_insuf = matches!(v, DriftVerdict::InsufficientData { .. });
                prop_assert!(is_insuf, "should be insufficient at step {}", i);
            }
        }
    }

    #[test]
    fn calibration_locks_after_min_observations(
        min_cal in 3..10usize,
    ) {
        let config = EValueConfig {
            min_calibration: min_cal,
            calibration_window: 100,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        for _ in 0..min_cal {
            monitor.observe(1.0, &config);
        }
        prop_assert!(monitor.is_calibrated());
    }

    #[test]
    fn null_rate_in_valid_range(
        outcomes in arb_outcome_sequence(5, 20),
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            calibration_window: 100,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        for &outcome in &outcomes {
            monitor.observe(if outcome { 1.0 } else { 0.0 }, &config);
        }

        if monitor.is_calibrated() {
            // Null rate clamped to [0.01, 0.99].
            prop_assert!(monitor.null_rate() >= 0.01);
            prop_assert!(monitor.null_rate() <= 0.99);
        }
    }
}

// =============================================================================
// E-value invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn e_value_always_non_negative(
        outcomes in arb_outcome_sequence(5, 50),
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            decay: 1.0,
            auto_reset_on_drift: false,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        for &outcome in &outcomes {
            monitor.observe(if outcome { 1.0 } else { 0.0 }, &config);
            prop_assert!(
                monitor.e_value() >= 0.0,
                "e-value must be non-negative, got {}",
                monitor.e_value()
            );
        }
    }

    #[test]
    fn e_value_bounded_above(
        outcomes in arb_outcome_sequence(10, 100),
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            decay: 1.0,
            auto_reset_on_drift: false,
            alpha: 0.0001,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        for &outcome in &outcomes {
            monitor.observe(if outcome { 1.0 } else { 0.0 }, &config);
        }

        prop_assert!(
            monitor.e_value() <= 1e15,
            "e-value should be capped, got {}",
            monitor.e_value()
        );
    }

    #[test]
    fn e_value_starts_at_one_after_calibration(
        min_cal in 3..10usize,
    ) {
        let config = EValueConfig {
            min_calibration: min_cal,
            calibration_window: 100,
            decay: 1.0,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Exactly min_cal observations to trigger calibration.
        for _ in 0..min_cal {
            monitor.observe(1.0, &config);
        }

        // E-value should be 1.0 right after calibration.
        let diff = (monitor.e_value() - 1.0).abs();
        prop_assert!(diff < 1e-10, "e-value should be 1.0 after calibration, got {}", monitor.e_value());
    }
}

// =============================================================================
// Drift detection invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn drift_requires_sufficient_evidence(
        alpha in 0.01..0.1f64,
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            alpha,
            decay: 1.0,
            auto_reset_on_drift: false,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at 90%.
        for outcome in [1.0, 1.0, 1.0, 1.0, 0.0] {
            monitor.observe(outcome, &config);
        }

        // Single failure should not cause drift.
        let v = monitor.observe(0.0, &config);
        prop_assert!(!v.is_drifted(), "single failure should not trigger drift");
    }

    #[test]
    fn sustained_failures_cause_drift(
        alpha in 0.01..0.1f64,
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            alpha,
            decay: 1.0,
            auto_reset_on_drift: true,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate at high success.
        for _ in 0..5 {
            monitor.observe(1.0, &config);
        }

        // Sustained failures should eventually trigger drift.
        let mut detected = false;
        for _ in 0..500 {
            let v = monitor.observe(0.0, &config);
            if v.is_drifted() {
                detected = true;
                break;
            }
        }
        prop_assert!(detected, "sustained failures should cause drift with alpha={}", alpha);
    }

    #[test]
    fn no_drift_under_null_hypothesis(
        successes_per_10 in 5..9u32,
    ) {
        // Use a repeating pattern: successes_per_10 out of 10 are successes.
        let config = EValueConfig {
            min_calibration: 20,
            calibration_window: 50,
            alpha: 0.05,
            decay: 1.0,
            auto_reset_on_drift: false,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Generate repeating pattern for both calibration and test.
        let pattern: Vec<f64> = (0..10)
            .map(|i| if i < successes_per_10 { 1.0 } else { 0.0 })
            .collect();

        // Calibrate.
        for i in 0..20 {
            monitor.observe(pattern[i % 10], &config);
        }

        // Continue with identical pattern — should not drift.
        let mut drift_count = 0;
        for i in 0..100 {
            let v = monitor.observe(pattern[i % 10], &config);
            if v.is_drifted() {
                drift_count += 1;
            }
        }

        prop_assert!(drift_count == 0, "should not drift under null, got {} drifts", drift_count);
    }

    #[test]
    fn auto_reset_restores_e_value(
        alpha in 0.01..0.1f64,
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            alpha,
            decay: 1.0,
            auto_reset_on_drift: true,
            ..Default::default()
        };
        let mut monitor = EValueMonitor::new("c1");

        // Calibrate.
        for _ in 0..5 {
            monitor.observe(1.0, &config);
        }

        // Drift.
        let mut drifted = false;
        for _ in 0..500 {
            let v = monitor.observe(0.0, &config);
            if v.is_drifted() {
                drifted = true;
                break;
            }
        }

        if drifted {
            let diff = (monitor.e_value() - 1.0).abs();
            prop_assert!(diff < 1e-10, "e-value should be 1.0 after auto-reset");
        }
    }
}

// =============================================================================
// Multi-reflex detector invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn detector_stats_count_observations(
        n_reflexes in 1..5usize,
        n_obs in 1..20usize,
    ) {
        let config = EValueConfig {
            min_calibration: 100, // Keep in calibration mode to avoid drift.
            ..Default::default()
        };
        let mut detector = ArsDriftDetector::new(config);

        for r in 0..n_reflexes {
            detector.register_reflex(r as u64 + 1, &format!("c{r}"));
        }

        for r in 0..n_reflexes {
            for _ in 0..n_obs {
                detector.observe(r as u64 + 1, true);
            }
        }

        let stats = detector.stats();
        prop_assert_eq!(stats.total_observations, (n_reflexes * n_obs) as u64);
        prop_assert_eq!(stats.registered_reflexes, n_reflexes);
    }

    #[test]
    fn detector_isolates_reflexes(
        n_reflexes in 2..5usize,
    ) {
        let config = EValueConfig {
            min_calibration: 5,
            decay: 1.0,
            auto_reset_on_drift: false,
            ..Default::default()
        };
        let mut detector = ArsDriftDetector::new(config);

        for r in 0..n_reflexes {
            detector.register_reflex(r as u64 + 1, "c1");
        }

        // Calibrate all.
        for r in 0..n_reflexes {
            for _ in 0..5 {
                detector.observe(r as u64 + 1, true);
            }
        }

        // Only fail reflex 1.
        for _ in 0..50 {
            detector.observe(1, false);
        }

        // Reflex 1 should have higher e-value than others.
        let e1 = detector.monitor(1).unwrap().e_value();
        for r in 1..n_reflexes {
            let e_other = detector.monitor(r as u64 + 1).unwrap().e_value();
            prop_assert!(
                e1 >= e_other,
                "failing reflex e-value {} should be >= stable reflex {} e-value {}",
                e1, r + 1, e_other
            );
        }
    }
}

// =============================================================================
// Serde roundtrip invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: EValueConfig = serde_json::from_str(&json).unwrap();
        let diff = (decoded.alpha - config.alpha).abs();
        prop_assert!(diff < 1e-10);
        let diff2 = (decoded.decay - config.decay).abs();
        prop_assert!(diff2 < 1e-10);
    }

    #[test]
    fn verdict_no_drift_roundtrip(
        e_val in 0.1..100.0f64,
        null_rate in 0.01..0.99f64,
    ) {
        let v = DriftVerdict::NoDrift {
            e_value: e_val,
            null_rate,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: DriftVerdict = serde_json::from_str(&json).unwrap();
        // f64 loses precision through JSON — use tolerance.
        if let DriftVerdict::NoDrift { e_value: de, null_rate: dn } = decoded {
            prop_assert!((de - e_val).abs() < 1e-10);
            prop_assert!((dn - null_rate).abs() < 1e-10);
        } else {
            prop_assert!(false, "wrong variant");
        }
    }

    #[test]
    fn verdict_drifted_roundtrip(
        e_val in 20.0..1000.0f64,
        null_rate in 0.5..0.99f64,
        obs_rate in 0.01..0.5f64,
        obs in 10..200usize,
    ) {
        let v = DriftVerdict::Drifted {
            e_value: e_val,
            null_rate,
            observed_rate: obs_rate,
            observations: obs,
        };
        let json = serde_json::to_string(&v).unwrap();
        let decoded: DriftVerdict = serde_json::from_str(&json).unwrap();
        if let DriftVerdict::Drifted { e_value: de, null_rate: dn, observed_rate: dor, observations: dobs } = decoded {
            prop_assert!((de - e_val).abs() < 1e-10);
            prop_assert!((dn - null_rate).abs() < 1e-10);
            prop_assert!((dor - obs_rate).abs() < 1e-10);
            prop_assert_eq!(dobs, obs);
        } else {
            prop_assert!(false, "wrong variant");
        }
    }

    #[test]
    fn drift_stats_roundtrip(
        total_obs in 0..10000u64,
        total_drifts in 0..100u64,
        reg in 0..50usize,
    ) {
        let stats = ArsDriftStats {
            total_observations: total_obs,
            total_drifts,
            registered_reflexes: reg,
            calibrated_reflexes: reg.min(reg),
            drifted_reflexes: 0,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: ArsDriftStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded, stats);
    }

    #[test]
    fn drift_action_roundtrip(idx in 0..3usize) {
        let actions = [
            DriftAction::DemoteToShadow,
            DriftAction::Recalibrate,
            DriftAction::AlertOperator,
        ];
        let action = &actions[idx];
        let json = serde_json::to_string(action).unwrap();
        let decoded: DriftAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&decoded, action);
    }
}
