//! Property-based tests for drift module (ADWIN drift detection).
//!
//! Verifies ADWIN windowing invariants:
//! - Window len >= 1 after push (never empties from drift shrink)
//! - Mean bounded by min/max of current window contents
//! - Variance non-negative
//! - Constant signal produces no drift
//! - Delta clamped to [1e-10, 1.0]
//! - DriftInfo: mean_diff == |old_mean - new_mean|
//! - DriftInfo: is_drop/is_spike consistency
//! - DriftInfo: relative_change handles zero old_mean
//! - RuleMonitor: observation counter accuracy
//! - RuleMonitor: window bounded by max_window_size
//! - DriftMonitor: disabled → never fires
//! - DriftMonitor: auto-registers rules
//! - Summary: total_drifts == sum of per-rule drifts
//! - Serde roundtrips for all serializable types
//!
//! Bead: wa-o8bt

use proptest::prelude::*;

use frankenterm_core::drift::{
    AdwinWindow, DriftConfig, DriftEvent, DriftInfo, DriftMonitor, DriftSummary, DriftType,
    RuleMonitor, RuleSummary,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_delta() -> impl Strategy<Value = f64> {
    (0.001..=0.5_f64) // valid confidence range
}

fn arb_observation() -> impl Strategy<Value = f64> {
    (-100.0..=100.0_f64)
}

fn arb_observations(max_len: usize) -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(arb_observation(), 1..max_len)
}

fn arb_config() -> impl Strategy<Value = DriftConfig> {
    (
        any::<bool>(),   // enabled
        0.001..=0.5_f64, // confidence
        4usize..=50,     // min_window_size
        100usize..=500,  // max_window_size
        0.0..=5.0_f64,   // min_mean_diff
    )
        .prop_map(
            |(enabled, confidence, min_ws, max_ws, min_diff)| DriftConfig {
                enabled,
                confidence,
                min_window_size: min_ws,
                max_window_size: max_ws.max(min_ws + 10),
                min_mean_diff: min_diff,
            },
        )
}

fn arb_drift_info() -> impl Strategy<Value = DriftInfo> {
    (
        -100.0..=100.0_f64, // old_mean
        -100.0..=100.0_f64, // new_mean
        1usize..=1000,      // dropped_count
        1usize..=1000,      // remaining_count
    )
        .prop_map(|(old_mean, new_mean, dropped, remaining)| {
            let mean_diff = (old_mean - new_mean).abs();
            DriftInfo {
                old_mean,
                new_mean,
                dropped_count: dropped,
                remaining_count: remaining,
                mean_diff,
                threshold: mean_diff * 0.9, // threshold < mean_diff (drift was detected)
            }
        })
}

// ────────────────────────────────────────────────────────────────────
// AdwinWindow: len >= 1 after push
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Window length is always >= 1 after a push (drift shrinks but never empties).
    #[test]
    fn prop_window_len_positive_after_push(
        delta in arb_delta(),
        observations in arb_observations(200),
    ) {
        let mut w = AdwinWindow::new(delta);
        for &v in &observations {
            w.push(v);
            prop_assert!(
                w.len() >= 1,
                "window must not be empty after push, len={}", w.len()
            );
        }
    }

    /// Empty window has len == 0 and mean == 0.
    #[test]
    fn prop_empty_window(delta in arb_delta()) {
        let w = AdwinWindow::new(delta);
        prop_assert!(w.is_empty());
        prop_assert_eq!(w.len(), 0);
        prop_assert!(w.mean().abs() < f64::EPSILON, "empty mean should be 0");
    }
}

// ────────────────────────────────────────────────────────────────────
// AdwinWindow: mean bounded by observation range
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Mean is always between the min and max of ALL observations ever pushed
    /// (since the window only shrinks from the left).
    #[test]
    fn prop_mean_bounded_by_range(
        delta in arb_delta(),
        observations in prop::collection::vec(0.0..=100.0_f64, 1..200),
    ) {
        let mut w = AdwinWindow::new(delta);
        let global_min = observations.iter().cloned().fold(f64::INFINITY, f64::min);
        let global_max = observations.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        for &v in &observations {
            w.push(v);
            let mean = w.mean();
            prop_assert!(
                mean >= global_min - 1e-9 && mean <= global_max + 1e-9,
                "mean {} outside [{}, {}]", mean, global_min, global_max
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// AdwinWindow: variance non-negative
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Variance is always >= 0.
    #[test]
    fn prop_variance_non_negative(
        delta in arb_delta(),
        observations in arb_observations(100),
    ) {
        let mut w = AdwinWindow::new(delta);
        for &v in &observations {
            w.push(v);
            prop_assert!(
                w.variance() >= -1e-12,
                "variance {} must be non-negative", w.variance()
            );
        }
    }

    /// Constant signal has zero variance (if no drift-induced shrink).
    #[test]
    fn prop_constant_signal_zero_variance(
        delta in 0.001..=0.01_f64,
        value in -50.0..=50.0_f64,
        n in 2usize..=100,
    ) {
        let mut w = AdwinWindow::new(delta);
        for _ in 0..n {
            w.push(value);
        }
        // Constant signal should not drift, so all values are in window
        if w.len() == n {
            prop_assert!(
                w.variance().abs() < 1e-9,
                "constant signal should have ~0 variance, got {}", w.variance()
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// AdwinWindow: constant signal → no drift
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// A perfectly constant signal should never trigger drift.
    #[test]
    fn prop_constant_no_drift(
        delta in arb_delta(),
        value in -100.0..=100.0_f64,
        n in 1usize..=500,
    ) {
        let mut w = AdwinWindow::new(delta);
        let mut drifts = 0;
        for _ in 0..n {
            if w.push(value).is_some() {
                drifts += 1;
            }
        }
        prop_assert_eq!(drifts, 0, "constant signal must produce 0 drifts");
    }
}

// ────────────────────────────────────────────────────────────────────
// AdwinWindow: delta clamping
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Delta is always clamped to [1e-10, 1.0].
    #[test]
    fn prop_delta_clamped(raw_delta in -10.0..=10.0_f64) {
        let w = AdwinWindow::new(raw_delta);
        prop_assert!(w.delta() >= 1e-10, "delta {} < 1e-10", w.delta());
        prop_assert!(w.delta() <= 1.0, "delta {} > 1.0", w.delta());
    }
}

// ────────────────────────────────────────────────────────────────────
// AdwinWindow: reset clears state
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Reset returns window to empty state.
    #[test]
    fn prop_reset_clears(
        delta in arb_delta(),
        observations in arb_observations(50),
    ) {
        let mut w = AdwinWindow::new(delta);
        for &v in &observations {
            w.push(v);
        }
        w.reset();
        prop_assert!(w.is_empty());
        prop_assert_eq!(w.len(), 0);
        prop_assert!(w.mean().abs() < f64::EPSILON);
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftInfo: consistency properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// mean_diff == |old_mean - new_mean|.
    #[test]
    fn prop_drift_info_mean_diff(info in arb_drift_info()) {
        let expected = (info.old_mean - info.new_mean).abs();
        prop_assert!(
            (info.mean_diff - expected).abs() < 1e-9,
            "mean_diff {} != |{} - {}| = {}", info.mean_diff, info.old_mean, info.new_mean, expected
        );
    }

    /// is_drop and is_spike are mutually exclusive when means differ.
    #[test]
    fn prop_drift_info_direction_exclusive(info in arb_drift_info()) {
        if (info.old_mean - info.new_mean).abs() > f64::EPSILON {
            prop_assert!(
                info.is_drop() != info.is_spike(),
                "is_drop and is_spike must be exclusive: old={}, new={}",
                info.old_mean, info.new_mean
            );
        }
    }

    /// is_drop iff new_mean < old_mean.
    #[test]
    fn prop_drift_info_is_drop_correct(info in arb_drift_info()) {
        prop_assert_eq!(
            info.is_drop(),
            info.new_mean < info.old_mean,
            "is_drop should match new < old"
        );
    }

    /// relative_change returns None when old_mean is near zero.
    #[test]
    fn prop_relative_change_zero_old_mean(
        new_mean in -100.0..=100.0_f64,
    ) {
        let info = DriftInfo {
            old_mean: 0.0,
            new_mean,
            dropped_count: 10,
            remaining_count: 10,
            mean_diff: new_mean.abs(),
            threshold: 1.0,
        };
        prop_assert!(info.relative_change().is_none(), "zero old_mean → None");
    }

    /// relative_change == (new - old) / old when old != 0.
    #[test]
    fn prop_relative_change_correct(info in arb_drift_info()) {
        if info.old_mean.abs() >= f64::EPSILON {
            let rc = info.relative_change().unwrap();
            let expected = (info.new_mean - info.old_mean) / info.old_mean;
            prop_assert!(
                (rc - expected).abs() < 1e-9,
                "relative_change {} != expected {}", rc, expected
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftInfo: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DriftInfo survives JSON roundtrip.
    #[test]
    fn prop_drift_info_serde(info in arb_drift_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: DriftInfo = serde_json::from_str(&json).unwrap();
        prop_assert!((info.old_mean - back.old_mean).abs() < 1e-9);
        prop_assert!((info.new_mean - back.new_mean).abs() < 1e-9);
        prop_assert_eq!(info.dropped_count, back.dropped_count);
        prop_assert_eq!(info.remaining_count, back.remaining_count);
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftConfig: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DriftConfig survives JSON roundtrip.
    #[test]
    fn prop_drift_config_serde(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: DriftConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.enabled, back.enabled);
        prop_assert!((config.confidence - back.confidence).abs() < 1e-9);
        prop_assert_eq!(config.min_window_size, back.min_window_size);
        prop_assert_eq!(config.max_window_size, back.max_window_size);
    }
}

// ────────────────────────────────────────────────────────────────────
// RuleMonitor: observation counter
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// total_observations == number of observe() calls.
    #[test]
    fn prop_rule_monitor_obs_counter(
        observations in arb_observations(100),
    ) {
        let config = DriftConfig::default();
        let mut mon = RuleMonitor::new("test".to_string(), 0.01);

        for (i, &v) in observations.iter().enumerate() {
            mon.observe(v, &config);
            prop_assert_eq!(
                mon.total_observations(), i + 1,
                "observation count mismatch at step {}", i
            );
        }
    }

    /// Window size <= max_window_size after any number of observations.
    #[test]
    fn prop_rule_monitor_window_bounded(
        max_ws in 20usize..=100,
        observations in prop::collection::vec(0.0..=10.0_f64, 1..500),
    ) {
        let config = DriftConfig {
            max_window_size: max_ws,
            ..Default::default()
        };
        let mut mon = RuleMonitor::new("bounded".to_string(), 0.01);

        for &v in &observations {
            mon.observe(v, &config);
            prop_assert!(
                mon.window_size() <= max_ws,
                "window {} > max {}", mon.window_size(), max_ws
            );
        }
    }

    /// Reset clears all counters.
    #[test]
    fn prop_rule_monitor_reset(
        observations in arb_observations(50),
    ) {
        let config = DriftConfig::default();
        let mut mon = RuleMonitor::new("test".to_string(), 0.01);

        for &v in &observations {
            mon.observe(v, &config);
        }

        mon.reset();
        prop_assert_eq!(mon.total_observations(), 0);
        prop_assert_eq!(mon.total_drifts(), 0);
        prop_assert_eq!(mon.window_size(), 0);
        prop_assert!(mon.last_drift().is_none());
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftMonitor: disabled → never fires
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Disabled monitor never produces drift events.
    #[test]
    fn prop_disabled_no_drift(
        observations in prop::collection::vec(
            (0usize..5, -100.0..=100.0_f64),
            1..200,
        ),
    ) {
        let config = DriftConfig {
            enabled: false,
            ..Default::default()
        };
        let mut dm = DriftMonitor::new(config);
        let rules = ["r0", "r1", "r2", "r3", "r4"];

        for &(rule_idx, value) in &observations {
            let event = dm.observe(rules[rule_idx], value);
            prop_assert!(event.is_none(), "disabled monitor must never fire");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftMonitor: auto-register and rule count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// observe() auto-registers rules; rule_count matches distinct rule IDs.
    #[test]
    fn prop_auto_register_rule_count(
        n_rules in 1usize..=10,
        obs_per_rule in 1usize..=20,
    ) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for rule_idx in 0..n_rules {
            let rule_id = format!("rule_{}", rule_idx);
            for i in 0..obs_per_rule {
                dm.observe(&rule_id, i as f64);
            }
        }

        prop_assert_eq!(dm.rule_count(), n_rules, "rule_count mismatch");
    }

    /// register_rule is idempotent: re-registering doesn't change count.
    #[test]
    fn prop_register_idempotent(n in 1usize..=5) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for _ in 0..n {
            dm.register_rule("same_rule");
        }
        prop_assert_eq!(dm.rule_count(), 1, "re-registering should not add duplicates");
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftMonitor: summary consistency
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Summary total_rules == rule_count.
    #[test]
    fn prop_summary_total_rules(
        n_rules in 1usize..=8,
        obs in 1usize..=10,
    ) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for r in 0..n_rules {
            for i in 0..obs {
                dm.observe(&format!("r{}", r), i as f64);
            }
        }

        let summary = dm.summary();
        prop_assert_eq!(summary.total_rules, dm.rule_count());
    }

    /// Summary total_drifts == sum of per-rule total_drifts.
    #[test]
    fn prop_summary_drift_sum(
        n_rules in 1usize..=5,
    ) {
        let config = DriftConfig {
            min_window_size: 5,
            min_mean_diff: 0.1,
            ..Default::default()
        };
        let mut dm = DriftMonitor::new(config);

        // Feed constant data (no drift expected)
        for r in 0..n_rules {
            for _ in 0..20 {
                dm.observe(&format!("rule_{}", r), 5.0);
            }
        }

        let summary = dm.summary();
        let sum_drifts: usize = summary.rules.iter().map(|r| r.total_drifts).sum();
        prop_assert_eq!(
            summary.total_drifts, sum_drifts,
            "total_drifts should equal sum of per-rule drifts"
        );
    }

    /// Summary rules are sorted alphabetically.
    #[test]
    fn prop_summary_rules_sorted(
        n_rules in 2usize..=8,
    ) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for r in 0..n_rules {
            dm.observe(&format!("z{}", r), 1.0);
            dm.observe(&format!("a{}", r), 1.0);
        }

        let summary = dm.summary();
        for pair in summary.rules.windows(2) {
            prop_assert!(
                pair[0].rule_id <= pair[1].rule_id,
                "rules not sorted: {} > {}", pair[0].rule_id, pair[1].rule_id
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftMonitor: unregister
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Unregistering a rule reduces rule_count by 1.
    #[test]
    fn prop_unregister_decrements(n_rules in 2usize..=10) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for r in 0..n_rules {
            dm.register_rule(&format!("rule_{}", r));
        }
        prop_assert_eq!(dm.rule_count(), n_rules);

        let removed = dm.unregister_rule("rule_0");
        prop_assert!(removed, "unregister existing rule should return true");
        prop_assert_eq!(dm.rule_count(), n_rules - 1);
    }

    /// Unregistering a nonexistent rule returns false, count unchanged.
    #[test]
    fn prop_unregister_nonexistent(n_rules in 1usize..=5) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for r in 0..n_rules {
            dm.register_rule(&format!("rule_{}", r));
        }

        let removed = dm.unregister_rule("does_not_exist");
        prop_assert!(!removed);
        prop_assert_eq!(dm.rule_count(), n_rules);
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftMonitor: reset
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Reset clears all monitors but preserves registered rules.
    #[test]
    fn prop_monitor_reset_preserves_rules(
        n_rules in 1usize..=5,
        obs in 5usize..=20,
    ) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for r in 0..n_rules {
            for i in 0..obs {
                dm.observe(&format!("rule_{}", r), i as f64);
            }
        }

        let count_before = dm.rule_count();
        dm.reset();

        // Rules still registered but counters cleared
        prop_assert_eq!(dm.rule_count(), count_before, "rules should persist after reset");
        for r in 0..n_rules {
            let mon = dm.rule_monitor(&format!("rule_{}", r)).unwrap();
            prop_assert_eq!(mon.total_observations(), 0, "observations should be 0 after reset");
            prop_assert_eq!(mon.window_size(), 0, "window should be empty after reset");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// DriftSummary/RuleSummary: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DriftSummary survives JSON roundtrip.
    #[test]
    fn prop_summary_serde_roundtrip(
        n_rules in 1usize..=5,
        obs in 1usize..=10,
    ) {
        let config = DriftConfig::default();
        let mut dm = DriftMonitor::new(config);

        for r in 0..n_rules {
            for i in 0..obs {
                dm.observe(&format!("rule_{}", r), i as f64);
            }
        }

        let summary = dm.summary();
        let json = serde_json::to_string(&summary).unwrap();
        let back: DriftSummary = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(summary.total_rules, back.total_rules);
        prop_assert_eq!(summary.total_drifts, back.total_drifts);
        prop_assert_eq!(summary.rules.len(), back.rules.len());
    }

    /// DriftType survives serde roundtrip.
    #[test]
    fn prop_drift_type_serde(is_drop in any::<bool>()) {
        let dt = if is_drop { DriftType::RateDrop } else { DriftType::RateSpike };
        let json = serde_json::to_string(&dt).unwrap();
        let back: DriftType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dt, back);
    }
}
