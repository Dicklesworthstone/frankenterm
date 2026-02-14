//! Property-based tests for EWMA invariants.
//!
//! Bead: wa-5rdx
//!
//! Validates:
//! 1. First observation sets value: EWMA equals first observed value
//! 2. Value bounded: EWMA stays within [min, max] of observed values
//! 3. Half-life decay: after one half-life, weight is ~0.5
//! 4. Long time convergence: after many half-lives, converges to latest value
//! 5. Count tracks: n observations → count = n
//! 6. Reset zeroes: reset clears all state
//! 7. Variance non-negative: EwmaWithVariance.variance() >= 0
//! 8. Z-score zero with no variance: z_score returns 0 when stddev is 0
//! 9. Z-score sign: above mean → positive, below mean → negative
//! 10. Rate estimator non-negative: rate_per_sec() >= 0
//! 11. Rate estimator monotonicity: faster events → higher rate
//! 12. Stats serializable: stats() roundtrips through serde

use proptest::prelude::*;

use frankenterm_core::ewma::{Ewma, EwmaWithVariance, RateEstimator};

// =============================================================================
// Strategies
// =============================================================================

fn arb_half_life_ms() -> impl Strategy<Value = f64> {
    1.0_f64..100_000.0
}

fn arb_value() -> impl Strategy<Value = f64> {
    -1000.0_f64..1000.0
}

// =============================================================================
// Property: First observation sets value exactly
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn first_observation_sets_value(
        hl in arb_half_life_ms(),
        val in arb_value(),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        ewma.observe(val, 0);

        prop_assert!((ewma.value() - val).abs() < 1e-10,
            "first observation should set value to {}, got {}", val, ewma.value());
        prop_assert!(ewma.is_initialized());
        prop_assert_eq!(ewma.count(), 1);
    }
}

// =============================================================================
// Property: EWMA value bounded by observed range
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn value_bounded_by_observations(
        hl in arb_half_life_ms(),
        values in proptest::collection::vec(arb_value(), 2..30),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        let mut min_val = f64::INFINITY;
        let mut max_val = f64::NEG_INFINITY;

        for (i, &v) in values.iter().enumerate() {
            ewma.observe(v, i as u64 * 100);
            min_val = min_val.min(v);
            max_val = max_val.max(v);
        }

        prop_assert!(ewma.value() >= min_val - 1e-10,
            "EWMA {} should be >= min observed {}", ewma.value(), min_val);
        prop_assert!(ewma.value() <= max_val + 1e-10,
            "EWMA {} should be <= max observed {}", ewma.value(), max_val);
    }
}

// =============================================================================
// Property: Half-life decay factor
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn half_life_decay(
        hl in 10.0_f64..10_000.0,
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        ewma.observe(0.0, 0);
        ewma.observe(100.0, hl.round() as u64); // exactly one half-life

        // After one half-life, alpha = 0.5, so EWMA = 0.5*100 + 0.5*0 = 50
        // Tolerance accounts for residual f64 rounding in alpha computation.
        prop_assert!((ewma.value() - 50.0).abs() < 1.0,
            "after one half-life, value should be ~50, got {}", ewma.value());
    }
}

// =============================================================================
// Property: Long time convergence
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn long_time_converges(
        hl in 10.0_f64..1000.0,
        target in arb_value(),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        ewma.observe(0.0, 0);
        // Observe target after 100 half-lives.
        let long_time = (hl * 100.0) as u64;
        ewma.observe(target, long_time);

        prop_assert!((ewma.value() - target).abs() < 0.01,
            "after 100 half-lives, should converge to {}, got {}", target, ewma.value());
    }
}

// =============================================================================
// Property: Count tracks observations
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn count_tracks(
        n in 1_usize..50,
    ) {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        for i in 0..n {
            ewma.observe(i as f64, i as u64 * 100);
        }
        prop_assert_eq!(ewma.count(), n as u64);
    }
}

// =============================================================================
// Property: Reset clears all state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn reset_clears_state(
        values in proptest::collection::vec(arb_value(), 1..10),
    ) {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        for (i, &v) in values.iter().enumerate() {
            ewma.observe(v, i as u64 * 100);
        }

        ewma.reset();
        prop_assert!(!ewma.is_initialized());
        prop_assert_eq!(ewma.count(), 0);
        prop_assert!((ewma.value() - 0.0).abs() < 1e-10);
    }
}

// =============================================================================
// Property: Variance non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn variance_nonnegative(
        values in proptest::collection::vec(arb_value(), 2..20),
    ) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(1000.0);
        for (i, &v) in values.iter().enumerate() {
            tracker.observe(v, i as u64 * 100);
        }

        prop_assert!(tracker.variance() >= 0.0,
            "variance should be >= 0, got {}", tracker.variance());
        prop_assert!(tracker.stddev() >= 0.0,
            "stddev should be >= 0, got {}", tracker.stddev());
    }
}

// =============================================================================
// Property: Z-score zero with no variance
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn z_score_zero_no_variance(
        val in arb_value(),
        test_val in arb_value(),
    ) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(1000.0);
        // Single observation → no variance initialized.
        tracker.observe(val, 0);

        prop_assert!((tracker.z_score(test_val) - 0.0).abs() < 1e-10,
            "z_score should be 0 with no variance, got {}", tracker.z_score(test_val));
    }
}

// =============================================================================
// Property: Z-score sign correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn z_score_sign_correct(
        base in 50.0_f64..200.0,
        spread in 1.0_f64..10.0,
    ) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(5000.0);
        // Feed values around `base` with some spread.
        for i in 0..20_u64 {
            let v = base + if i % 2 == 0 { spread } else { -spread };
            tracker.observe(v, i * 100);
        }

        let mean = tracker.mean();
        let above = 10.0f64.mul_add(spread, mean);
        let below = 10.0f64.mul_add(-spread, mean);

        let z_above = tracker.z_score(above);
        let z_below = tracker.z_score(below);

        if tracker.stddev() > 1e-10 {
            prop_assert!(z_above > 0.0,
                "value above mean should have positive z-score, got {}", z_above);
            prop_assert!(z_below < 0.0,
                "value below mean should have negative z-score, got {}", z_below);
        }
    }
}

// =============================================================================
// Property: Rate estimator non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn rate_nonnegative(
        intervals in proptest::collection::vec(1_u64..1000, 2..20),
    ) {
        let mut rate = RateEstimator::with_half_life_ms(5000.0);
        let mut t = 0_u64;
        for &interval in &intervals {
            t += interval;
            rate.tick(t);
        }

        prop_assert!(rate.rate_per_sec() >= 0.0,
            "rate should be >= 0, got {}", rate.rate_per_sec());
    }
}

// =============================================================================
// Property: Faster events → higher rate
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn faster_events_higher_rate(
        fast_interval in 10_u64..100,
        slow_interval in 500_u64..2000,
    ) {
        let mut fast_rate = RateEstimator::with_half_life_ms(5000.0);
        let mut slow_rate = RateEstimator::with_half_life_ms(5000.0);

        let mut t = 0_u64;
        for _ in 0..20 {
            t += fast_interval;
            fast_rate.tick(t);
        }

        t = 0;
        for _ in 0..20 {
            t += slow_interval;
            slow_rate.tick(t);
        }

        prop_assert!(fast_rate.rate_per_sec() > slow_rate.rate_per_sec(),
            "fast rate {} should exceed slow rate {}",
            fast_rate.rate_per_sec(), slow_rate.rate_per_sec());
    }
}

// =============================================================================
// Property: Rate total_events tracks ticks
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn rate_total_events_tracks(
        n in 1_usize..50,
    ) {
        let mut rate = RateEstimator::with_half_life_ms(5000.0);
        for i in 0..n {
            rate.tick(i as u64 * 100);
        }
        prop_assert_eq!(rate.total_events(), n as u64);
    }
}

// =============================================================================
// Property: Rate reset clears
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn rate_reset_clears(
        n in 2_usize..20,
    ) {
        let mut rate = RateEstimator::with_half_life_ms(5000.0);
        for i in 0..n {
            rate.tick(i as u64 * 100);
        }

        rate.reset();
        prop_assert_eq!(rate.total_events(), 0);
        prop_assert!((rate.rate_per_sec() - 0.0).abs() < 1e-10);
    }
}

// =============================================================================
// Property: EwmaWithVariance reset clears
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn variance_reset_clears(
        values in proptest::collection::vec(arb_value(), 2..10),
    ) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(1000.0);
        for (i, &v) in values.iter().enumerate() {
            tracker.observe(v, i as u64 * 100);
        }

        tracker.reset();
        prop_assert_eq!(tracker.count(), 0);
        prop_assert!((tracker.variance() - 0.0).abs() < 1e-10);
    }
}

// =============================================================================
// Property: Stats snapshot consistent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stats_consistent(
        hl in arb_half_life_ms(),
        values in proptest::collection::vec(arb_value(), 1..10),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        for (i, &v) in values.iter().enumerate() {
            ewma.observe(v, i as u64 * 100);
        }

        let stats = ewma.stats();
        prop_assert!((stats.value - ewma.value()).abs() < 1e-10);
        prop_assert_eq!(stats.count, ewma.count());
        prop_assert!((stats.half_life_ms - hl).abs() < 1e-10);
    }
}

// =============================================================================
// Property: Constant input → EWMA equals that constant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn constant_input_converges(
        hl in arb_half_life_ms(),
        constant in arb_value(),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        for i in 0..20_u64 {
            ewma.observe(constant, i * 100);
        }

        prop_assert!((ewma.value() - constant).abs() < 1e-6,
            "constant input {} should produce EWMA ~{}, got {}",
            constant, constant, ewma.value());
    }
}

// =============================================================================
// Property: New EWMA is not initialized
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn new_ewma_not_initialized(hl in arb_half_life_ms()) {
        let ewma = Ewma::with_half_life_ms(hl);
        prop_assert!(!ewma.is_initialized());
        prop_assert_eq!(ewma.count(), 0);
        prop_assert!((ewma.value() - 0.0).abs() < 1e-10,
            "uninit EWMA should have value 0");
    }
}

// =============================================================================
// Property: Double reset is idempotent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn double_reset_idempotent(
        values in proptest::collection::vec(arb_value(), 1..10),
    ) {
        let mut ewma = Ewma::with_half_life_ms(1000.0);
        for (i, &v) in values.iter().enumerate() {
            ewma.observe(v, i as u64 * 100);
        }

        ewma.reset();
        ewma.reset();
        prop_assert!(!ewma.is_initialized());
        prop_assert_eq!(ewma.count(), 0);
    }
}

// =============================================================================
// Property: Stats serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stats_serde_roundtrip(
        hl in arb_half_life_ms(),
        values in proptest::collection::vec(arb_value(), 1..10),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        for (i, &v) in values.iter().enumerate() {
            ewma.observe(v, i as u64 * 100);
        }

        let stats = ewma.stats();
        let json = serde_json::to_string(&stats).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(back.is_object());
        prop_assert!(back.get("value").is_some());
        prop_assert!(back.get("count").is_some());
        prop_assert!(back.get("half_life_ms").is_some());
    }
}

// =============================================================================
// Property: Constant input → zero variance
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn constant_input_zero_variance(
        hl in arb_half_life_ms(),
        constant in arb_value(),
    ) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(hl);
        for i in 0..20_u64 {
            tracker.observe(constant, i * 100);
        }

        prop_assert!(tracker.variance() < 1e-6,
            "constant input should have near-zero variance, got {}", tracker.variance());
    }
}

// =============================================================================
// Property: EwmaWithVariance mean tracks EWMA value
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn variance_mean_tracks_ewma(
        hl in arb_half_life_ms(),
        values in proptest::collection::vec(arb_value(), 2..20),
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        let mut tracker = EwmaWithVariance::with_half_life_ms(hl);
        for (i, &v) in values.iter().enumerate() {
            ewma.observe(v, i as u64 * 100);
            tracker.observe(v, i as u64 * 100);
        }

        prop_assert!((tracker.mean() - ewma.value()).abs() < 1e-6,
            "EwmaWithVariance mean {} should track Ewma value {}",
            tracker.mean(), ewma.value());
    }
}

// =============================================================================
// Property: EwmaWithVariance count tracks observations
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn variance_count_tracks(n in 1_usize..50) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(1000.0);
        for i in 0..n {
            tracker.observe(i as f64, i as u64 * 100);
        }
        prop_assert_eq!(tracker.count(), n as u64);
    }
}

// =============================================================================
// Property: RateEstimator single tick
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn rate_single_tick(hl in 100.0_f64..50_000.0) {
        let mut rate = RateEstimator::with_half_life_ms(hl);
        rate.tick(1000); // 1 second
        prop_assert_eq!(rate.total_events(), 1);
    }
}

// =============================================================================
// Property: EWMA monotone toward new value
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// When all observations are the same new value, EWMA moves monotonically
    /// toward that value from the initial observation.
    #[test]
    fn ewma_monotone_toward_target(
        hl in 10.0_f64..5000.0,
        initial in -500.0_f64..500.0,
        target in -500.0_f64..500.0,
    ) {
        let mut ewma = Ewma::with_half_life_ms(hl);
        ewma.observe(initial, 0);

        let mut prev = initial;
        for i in 1..=20_u64 {
            ewma.observe(target, i * (hl.round() as u64 / 10).max(1));
            let current = ewma.value();
            // EWMA should move toward target (or stay same if already there)
            if (target - initial).abs() > 1e-10 {
                let prev_dist = (target - prev).abs();
                let curr_dist = (target - current).abs();
                prop_assert!(curr_dist <= prev_dist + 1e-6,
                    "EWMA should move toward target: prev_dist={}, curr_dist={}", prev_dist, curr_dist);
            }
            prev = current;
        }
    }
}

// =============================================================================
// Property: RateEstimator double reset idempotent
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn rate_double_reset(n in 2_usize..20) {
        let mut rate = RateEstimator::with_half_life_ms(5000.0);
        for i in 0..n {
            rate.tick(i as u64 * 100);
        }
        rate.reset();
        rate.reset();
        prop_assert_eq!(rate.total_events(), 0);
        prop_assert!((rate.rate_per_sec() - 0.0).abs() < 1e-10);
    }
}

// =============================================================================
// Property: EwmaWithVariance stddev is sqrt of variance
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stddev_is_sqrt_variance(
        values in proptest::collection::vec(arb_value(), 2..20),
    ) {
        let mut tracker = EwmaWithVariance::with_half_life_ms(1000.0);
        for (i, &v) in values.iter().enumerate() {
            tracker.observe(v, i as u64 * 100);
        }

        let variance = tracker.variance();
        let stddev = tracker.stddev();
        if variance > 0.0 {
            prop_assert!((stddev - variance.sqrt()).abs() < 1e-10,
                "stddev {} should be sqrt of variance {}", stddev, variance);
        } else {
            prop_assert!(stddev < 1e-10, "stddev should be ~0 when variance is ~0");
        }
    }
}
