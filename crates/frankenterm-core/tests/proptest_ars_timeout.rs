//! Property-based tests for ARS dynamic timeout calculator.
//!
//! Verifies invariants of log-normal parameter estimation, CDF/quantile,
//! expected loss minimization, and timeout bounds.

use proptest::prelude::*;

use frankenterm_core::ars_timeout::{
    DurationStats, TimeoutCalculator, TimeoutConfig, TimeoutDecision,
    TimeoutMethod, TimeoutTracker,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_positive_durations(min: usize, max: usize) -> impl Strategy<Value = Vec<f64>> {
    prop::collection::vec(1.0..10000.0f64, min..=max)
}

fn arb_timeout_config() -> impl Strategy<Value = TimeoutConfig> {
    (
        1.0..100.0f64,    // cost_hang
        0.1..50.0f64,     // cost_premature_kill
        100..2000u64,     // min_timeout_ms
        10_000..120_000u64, // max_timeout_ms
        1000..10_000u64,  // default_timeout_ms
        1..10usize,       // min_observations
        1.0..3.0f64,      // safety_multiplier
        0.8..0.999f64,    // fallback_percentile
    )
        .prop_map(
            |(ch, cp, min_t, max_t, def_t, min_obs, safety, perc)| TimeoutConfig {
                cost_hang: ch,
                cost_premature_kill: cp,
                min_timeout_ms: min_t,
                max_timeout_ms: max_t.max(min_t + 1000),
                default_timeout_ms: def_t.max(min_t).min(max_t),
                min_observations: min_obs,
                safety_multiplier: safety,
                fallback_percentile: perc,
            },
        )
}

// =============================================================================
// DurationStats invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stats_count_matches_positive_inputs(
        durations in prop::collection::vec(-100.0..1000.0f64, 1..50),
    ) {
        if let Some(stats) = DurationStats::from_durations(&durations) {
            let positive_count = durations.iter().filter(|d| **d > 0.0).count();
            prop_assert_eq!(stats.count, positive_count);
        }
    }

    #[test]
    fn stats_mean_in_range(durations in arb_positive_durations(1, 50)) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let min = durations.iter().copied().fold(f64::INFINITY, f64::min);
        let max = durations.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        prop_assert!(
            stats.mean_ms >= min - 1e-10 && stats.mean_ms <= max + 1e-10,
            "mean {} should be in [{}, {}]",
            stats.mean_ms,
            min,
            max
        );
    }

    #[test]
    fn stats_median_in_range(durations in arb_positive_durations(1, 50)) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        prop_assert!(stats.median_ms >= stats.min_ms - 1e-10);
        prop_assert!(stats.median_ms <= stats.max_ms + 1e-10);
    }

    #[test]
    fn stats_std_dev_non_negative(durations in arb_positive_durations(1, 50)) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        prop_assert!(stats.std_dev_ms >= 0.0);
    }

    #[test]
    fn stats_ln_sigma_non_negative(durations in arb_positive_durations(2, 50)) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        prop_assert!(
            stats.ln_sigma >= -1e-10,
            "ln_sigma should be non-negative, got {}",
            stats.ln_sigma
        );
    }
}

// =============================================================================
// CDF invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn cdf_in_unit_interval(
        durations in arb_positive_durations(3, 20),
        t in 0.0..20000.0f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let cdf = stats.cdf(t);
        prop_assert!(cdf >= 0.0, "CDF should be >= 0, got {}", cdf);
        prop_assert!(cdf <= 1.0, "CDF should be <= 1, got {}", cdf);
    }

    #[test]
    fn survival_in_unit_interval(
        durations in arb_positive_durations(3, 20),
        t in 0.0..20000.0f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let s = stats.survival(t);
        prop_assert!(s >= 0.0 && s <= 1.0);
    }

    #[test]
    fn cdf_plus_survival_is_one(
        durations in arb_positive_durations(3, 20),
        t in 0.1..10000.0f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let sum = stats.cdf(t) + stats.survival(t);
        prop_assert!(
            (sum - 1.0).abs() < 1e-10,
            "CDF + survival should be 1, got {}",
            sum
        );
    }

    #[test]
    fn cdf_is_monotone_nondecreasing(
        durations in arb_positive_durations(3, 20),
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let mut prev = 0.0;
        for t in (0..100).map(|i| i as f64 * 100.0) {
            let cdf = stats.cdf(t);
            prop_assert!(
                cdf >= prev - 1e-10,
                "CDF should be monotone: prev={} cur={} at t={}",
                prev,
                cdf,
                t
            );
            prev = cdf;
        }
    }

    #[test]
    fn cdf_at_zero_is_zero(durations in arb_positive_durations(3, 20)) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let cdf_zero = stats.cdf(0.0);
        prop_assert!(
            cdf_zero.abs() < 1e-10,
            "CDF(0) should be 0, got {}",
            cdf_zero
        );
    }
}

// =============================================================================
// Quantile invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn quantile_is_positive(
        durations in arb_positive_durations(3, 20),
        p in 0.01..0.99f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let q = stats.quantile(p);
        prop_assert!(q > 0.0, "quantile should be positive for p={}", p);
    }

    #[test]
    fn quantile_is_monotone(
        durations in arb_positive_durations(3, 20),
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let mut prev = 0.0;
        for i in 1..=99 {
            let p = i as f64 / 100.0;
            let q = stats.quantile(p);
            prop_assert!(
                q >= prev - 1e-6,
                "quantile should be monotone: prev={} cur={} at p={}",
                prev,
                q,
                p
            );
            prev = q;
        }
    }

    #[test]
    fn quantile_cdf_approximate_inverse(
        durations in arb_positive_durations(5, 30),
        p in 0.05..0.95f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        if stats.ln_sigma > 0.01 {
            let t = stats.quantile(p);
            let recovered = stats.cdf(t);
            prop_assert!(
                (recovered - p).abs() < 0.05,
                "quantile-CDF roundtrip: p={} t={} recovered={}",
                p,
                t,
                recovered
            );
        }
    }
}

// =============================================================================
// Expected loss invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn expected_loss_non_negative(
        durations in arb_positive_durations(3, 20),
        t in 0.1..50000.0f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let calc = TimeoutCalculator::with_defaults();
        let loss = calc.expected_loss(&stats, t);
        prop_assert!(loss >= 0.0, "expected loss should be non-negative, got {}", loss);
    }

    #[test]
    fn expected_loss_bounded_by_max_cost(
        durations in arb_positive_durations(3, 20),
        t in 0.1..50000.0f64,
        cost_hang in 1.0..100.0f64,
        cost_kill in 0.1..50.0f64,
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let calc = TimeoutCalculator::new(TimeoutConfig {
            cost_hang,
            cost_premature_kill: cost_kill,
            min_observations: 1,
            ..Default::default()
        });
        let loss = calc.expected_loss(&stats, t);
        let max_cost = cost_hang.max(cost_kill);
        prop_assert!(
            loss <= max_cost + 0.01,
            "loss {} should be <= max cost {}",
            loss,
            max_cost
        );
    }
}

// =============================================================================
// Timeout calculation invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn timeout_respects_bounds(
        durations in arb_positive_durations(5, 30),
        config in arb_timeout_config(),
    ) {
        let calc = TimeoutCalculator::new(config.clone());
        let decision = calc.calculate(&durations);
        prop_assert!(
            decision.timeout_ms >= config.min_timeout_ms,
            "timeout {} should be >= min {}",
            decision.timeout_ms,
            config.min_timeout_ms
        );
        prop_assert!(
            decision.timeout_ms <= config.max_timeout_ms,
            "timeout {} should be <= max {}",
            decision.timeout_ms,
            config.max_timeout_ms
        );
    }

    #[test]
    fn timeout_insufficient_data_returns_default(
        n_obs in 0..3usize,
    ) {
        let durations: Vec<f64> = (0..n_obs).map(|i| (i + 1) as f64 * 100.0).collect();
        let calc = TimeoutCalculator::new(TimeoutConfig {
            min_observations: 3,
            ..Default::default()
        });
        let decision = calc.calculate(&durations);
        let is_default = decision.method == TimeoutMethod::Default;
        prop_assert!(is_default, "insufficient data should use Default method");
        prop_assert!(!decision.is_data_driven);
    }

    #[test]
    fn timeout_sufficient_data_is_data_driven(
        durations in arb_positive_durations(5, 30),
    ) {
        let calc = TimeoutCalculator::new(TimeoutConfig {
            min_observations: 3,
            ..Default::default()
        });
        let decision = calc.calculate(&durations);
        prop_assert!(decision.is_data_driven);
        prop_assert!(decision.stats.is_some());
    }

    #[test]
    fn timeout_does_not_panic(
        config in arb_timeout_config(),
        durations in prop::collection::vec(-100.0..50000.0f64, 0..50),
    ) {
        let calc = TimeoutCalculator::new(config);
        let _decision = calc.calculate(&durations);
        // No panic = success.
    }
}

// =============================================================================
// Config serde invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(config in arb_timeout_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: TimeoutConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.min_timeout_ms, config.min_timeout_ms);
        prop_assert_eq!(decoded.max_timeout_ms, config.max_timeout_ms);
        prop_assert_eq!(decoded.min_observations, config.min_observations);
        let ch_diff = (decoded.cost_hang - config.cost_hang).abs();
        prop_assert!(ch_diff < 1e-10);
        let cp_diff = (decoded.cost_premature_kill - config.cost_premature_kill).abs();
        prop_assert!(cp_diff < 1e-10);
    }

    #[test]
    fn decision_serde_roundtrip(
        durations in arb_positive_durations(5, 20),
    ) {
        let calc = TimeoutCalculator::with_defaults();
        let decision = calc.calculate(&durations);
        let json = serde_json::to_string(&decision).unwrap();
        let decoded: TimeoutDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.timeout_ms, decision.timeout_ms);
        prop_assert_eq!(decoded.method, decision.method);
        prop_assert_eq!(decoded.is_data_driven, decision.is_data_driven);
    }

    #[test]
    fn stats_serde_roundtrip(
        durations in arb_positive_durations(3, 20),
    ) {
        let stats = DurationStats::from_durations(&durations).unwrap();
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: DurationStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decoded.count, stats.count);
        let mean_diff = (decoded.mean_ms - stats.mean_ms).abs();
        prop_assert!(mean_diff < 1e-10);
    }
}

// =============================================================================
// Tracker invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tracker_observation_count_matches(
        durations in arb_positive_durations(0, 50),
    ) {
        let mut tracker = TimeoutTracker::with_defaults();
        for d in &durations {
            tracker.record(*d);
        }
        prop_assert_eq!(tracker.observation_count(), durations.len());
        prop_assert_eq!(tracker.total_observations(), durations.len() as u64);
    }

    #[test]
    fn tracker_timeout_rate_in_unit_interval(
        n_recorded in 1..50u64,
        timeout_frac in 0.0..1.0f64,
    ) {
        let mut tracker = TimeoutTracker::with_defaults();
        for i in 0..n_recorded {
            tracker.record((i + 1) as f64 * 10.0);
        }
        let n_timeouts = (n_recorded as f64 * timeout_frac) as u64;
        for _ in 0..n_timeouts {
            tracker.record_timeout();
        }
        let rate = tracker.timeout_rate();
        prop_assert!(rate >= 0.0);
        prop_assert!(rate <= 1.0 + 1e-10);
    }
}
