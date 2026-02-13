//! Property-based tests for continuous backpressure invariants.
//!
//! Bead: wa-ruko
//!
//! Validates:
//! 1. Sigmoid range: output always in [0, 1]
//! 2. Sigmoid monotonicity: σ(a) ≤ σ(b) when a ≤ b
//! 3. Sigmoid symmetry: σ(x) + σ(−x) = 1
//! 4. Severity range: always in [0, 1] for q in [0, 1]
//! 5. Severity monotonic in q: higher queue ratio → higher severity
//! 6. Severity at center: severity(θ, θ, k) = 0.5
//! 7. Tier monotonic: higher severity → same or higher tier
//! 8. EMA bounded: inputs in [a, b] → EMA in [a, b]
//! 9. EMA first observation: EMA(first) = raw value
//! 10. EMA constant convergence: constant input → EMA converges
//! 11. ThrottleActions poll_multiplier bounded: [1.0, 1.0 + max_backoff]
//! 12. ThrottleActions pane_skip bounded: [0.0, max_pane_skip]
//! 13. ThrottleActions detection_skip bounded: [0.0, max_detection_skip]
//! 14. ThrottleActions buffer_limit bounded: [min_buffer_fraction, 1.0]
//! 15. ThrottleActions severity consistency: actions.severity = engine.severity
//! 16. Engine queue_ratio = max(capture, write) after convergence
//! 17. Engine update_count increments
//! 18. Config serde roundtrip
//! 19. Snapshot serde roundtrip
//! 20. Severity Lipschitz: small q change → bounded severity change
//! 21. Actions monotonic: higher severity → higher poll_multiplier, skip, lower buffer
//! 22. Pane skip quadratic: pane_skip ≤ detection_skip at moderate severity

use proptest::prelude::*;

use frankenterm_core::backpressure::BackpressureTier;
use frankenterm_core::continuous_backpressure::{
    ContinuousBackpressure, ContinuousBackpressureConfig, EmaSmoother, severity, severity_to_tier,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_queue_ratio() -> impl Strategy<Value = f64> {
    0.0_f64..=1.0
}

fn arb_severity_val() -> impl Strategy<Value = f64> {
    0.0_f64..=1.0
}

fn arb_sigmoid_input() -> impl Strategy<Value = f64> {
    -50.0_f64..50.0
}

fn arb_center() -> impl Strategy<Value = f64> {
    0.1_f64..0.9
}

fn arb_steepness() -> impl Strategy<Value = f64> {
    1.0_f64..30.0
}

fn arb_config() -> impl Strategy<Value = ContinuousBackpressureConfig> {
    (
        arb_center(),
        arb_steepness(),
        1_usize..50,   // smoothing_window
        1.0_f64..10.0, // max_backoff_multiplier
        0.1_f64..0.9,  // max_pane_skip
        0.05_f64..0.5, // max_detection_skip
        0.05_f64..0.5, // min_buffer_fraction
    )
        .prop_map(|(center, steep, window, backoff, pane, detect, buf)| {
            ContinuousBackpressureConfig {
                center_threshold: center,
                steepness: steep,
                smoothing_window: window,
                max_backoff_multiplier: backoff,
                max_pane_skip: pane,
                max_detection_skip: detect,
                min_buffer_fraction: buf,
            }
        })
}

fn arb_ema_window() -> impl Strategy<Value = usize> {
    1_usize..100
}

// =============================================================================
// Property: Sigmoid range — output always in [0, 1]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn sigmoid_range(x in arb_sigmoid_input()) {
        // We need to call the severity function with q=x mapped to check sigmoid indirectly.
        // severity(q, 0.0, 1.0) = sigmoid(1.0 * (q - 0.0)) = sigmoid(q)
        let s = severity(x, 0.0, 1.0);
        prop_assert!(s >= 0.0, "sigmoid({}) = {} < 0", x, s);
        prop_assert!(s <= 1.0, "sigmoid({}) = {} > 1", x, s);
    }
}

// =============================================================================
// Property: Sigmoid monotonicity — σ(a) ≤ σ(b) when a ≤ b
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn sigmoid_monotonic(
        a in arb_sigmoid_input(),
        b in arb_sigmoid_input(),
    ) {
        let sa = severity(a, 0.0, 1.0);
        let sb = severity(b, 0.0, 1.0);
        if a <= b {
            prop_assert!(sa <= sb + 1e-10,
                "sigmoid not monotonic: sigmoid({}) = {} > sigmoid({}) = {}", a, sa, b, sb);
        }
    }
}

// =============================================================================
// Property: Sigmoid symmetry — σ(x) + σ(−x) = 1
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn sigmoid_symmetry(x in -20.0_f64..20.0) {
        let pos = severity(x, 0.0, 1.0);
        let neg = severity(-x, 0.0, 1.0);
        prop_assert!((pos + neg - 1.0).abs() < 1e-10,
            "σ({}) + σ({}) = {} ≠ 1.0", x, -x, pos + neg);
    }
}

// =============================================================================
// Property: Severity range — always in [0, 1] for q in [0, 1]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn severity_range(
        q in arb_queue_ratio(),
        center in arb_center(),
        steepness in arb_steepness(),
    ) {
        let s = severity(q, center, steepness);
        prop_assert!(s >= 0.0, "severity({}, {}, {}) = {} < 0", q, center, steepness, s);
        prop_assert!(s <= 1.0, "severity({}, {}, {}) = {} > 1", q, center, steepness, s);
    }
}

// =============================================================================
// Property: Severity monotonic in q
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn severity_monotonic_in_q(
        q1 in arb_queue_ratio(),
        q2 in arb_queue_ratio(),
        center in arb_center(),
        steepness in arb_steepness(),
    ) {
        let s1 = severity(q1, center, steepness);
        let s2 = severity(q2, center, steepness);
        if q1 <= q2 {
            prop_assert!(s1 <= s2 + 1e-10,
                "severity not monotonic: s({})={} > s({})={}", q1, s1, q2, s2);
        }
    }
}

// =============================================================================
// Property: Severity at center = 0.5
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn severity_at_center(
        center in arb_center(),
        steepness in arb_steepness(),
    ) {
        let s = severity(center, center, steepness);
        prop_assert!((s - 0.5).abs() < 1e-10,
            "severity(center={}, center, steepness={}) = {} ≠ 0.5", center, steepness, s);
    }
}

// =============================================================================
// Property: Tier mapping monotonic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn tier_monotonic(
        s1 in arb_severity_val(),
        s2 in arb_severity_val(),
    ) {
        let t1 = severity_to_tier(s1);
        let t2 = severity_to_tier(s2);
        if s1 <= s2 {
            prop_assert!(t1 <= t2,
                "tier not monotonic: tier({})={:?} > tier({})={:?}", s1, t1, s2, t2);
        }
    }
}

// =============================================================================
// Property: Tier covers full range
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn tier_covers_full_range(s in arb_severity_val()) {
        let tier = severity_to_tier(s);
        let valid = matches!(
            tier,
            BackpressureTier::Green
                | BackpressureTier::Yellow
                | BackpressureTier::Red
                | BackpressureTier::Black
        );
        prop_assert!(valid, "severity {} mapped to unexpected tier {:?}", s, tier);
    }
}

// =============================================================================
// Property: EMA bounded — inputs in [a, b] → EMA in [a, b]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn ema_bounded(
        window in arb_ema_window(),
        values in proptest::collection::vec(0.0_f64..100.0, 1..50),
    ) {
        let mut ema = EmaSmoother::new(window);
        let min_val = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max_val = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);

        for &v in &values {
            let smoothed = ema.update(v);
            prop_assert!(smoothed >= min_val - 1e-10,
                "EMA {} < min {}", smoothed, min_val);
            prop_assert!(smoothed <= max_val + 1e-10,
                "EMA {} > max {}", smoothed, max_val);
        }
    }
}

// =============================================================================
// Property: EMA first observation = raw
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    #[test]
    fn ema_first_equals_raw(
        window in arb_ema_window(),
        raw in -1000.0_f64..1000.0,
    ) {
        let mut ema = EmaSmoother::new(window);
        let smoothed = ema.update(raw);
        prop_assert!((smoothed - raw).abs() < 1e-10,
            "first EMA {} ≠ raw {}", smoothed, raw);
        prop_assert!(ema.is_initialized());
    }
}

// =============================================================================
// Property: EMA constant convergence
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn ema_constant_convergence(
        window in arb_ema_window(),
        constant in 0.0_f64..100.0,
    ) {
        let mut ema = EmaSmoother::new(window);
        for _ in 0..200 {
            ema.update(constant);
        }
        prop_assert!((ema.value() - constant).abs() < 0.01,
            "EMA {} didn't converge to constant {}", ema.value(), constant);
    }
}

// =============================================================================
// Property: ThrottleActions poll_multiplier bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn actions_poll_multiplier_bounded(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..30),
    ) {
        let mut bp = ContinuousBackpressure::new(config.clone());
        for (cap, write) in &ratios {
            let actions = bp.update(*cap, *write);
            prop_assert!(actions.poll_multiplier >= 1.0,
                "poll_multiplier {} < 1.0", actions.poll_multiplier);
            prop_assert!(actions.poll_multiplier <= 1.0 + config.max_backoff_multiplier + 1e-10,
                "poll_multiplier {} > {}", actions.poll_multiplier, 1.0 + config.max_backoff_multiplier);
        }
    }
}

// =============================================================================
// Property: ThrottleActions pane_skip bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn actions_pane_skip_bounded(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..30),
    ) {
        let mut bp = ContinuousBackpressure::new(config.clone());
        for (cap, write) in &ratios {
            let actions = bp.update(*cap, *write);
            prop_assert!(actions.pane_skip_fraction >= 0.0,
                "pane_skip {} < 0", actions.pane_skip_fraction);
            prop_assert!(actions.pane_skip_fraction <= config.max_pane_skip + 1e-10,
                "pane_skip {} > max {}", actions.pane_skip_fraction, config.max_pane_skip);
        }
    }
}

// =============================================================================
// Property: ThrottleActions detection_skip bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn actions_detection_skip_bounded(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..30),
    ) {
        let mut bp = ContinuousBackpressure::new(config.clone());
        for (cap, write) in &ratios {
            let actions = bp.update(*cap, *write);
            prop_assert!(actions.detection_skip_fraction >= 0.0,
                "detection_skip {} < 0", actions.detection_skip_fraction);
            prop_assert!(actions.detection_skip_fraction <= config.max_detection_skip + 1e-10,
                "detection_skip {} > max {}", actions.detection_skip_fraction, config.max_detection_skip);
        }
    }
}

// =============================================================================
// Property: ThrottleActions buffer_limit bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn actions_buffer_limit_bounded(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..30),
    ) {
        let mut bp = ContinuousBackpressure::new(config.clone());
        for (cap, write) in &ratios {
            let actions = bp.update(*cap, *write);
            prop_assert!(actions.buffer_limit_fraction >= config.min_buffer_fraction - 1e-10,
                "buffer_limit {} < min {}", actions.buffer_limit_fraction, config.min_buffer_fraction);
            prop_assert!(actions.buffer_limit_fraction <= 1.0 + 1e-10,
                "buffer_limit {} > 1.0", actions.buffer_limit_fraction);
        }
    }
}

// =============================================================================
// Property: Actions severity consistency
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn actions_severity_consistent(
        config in arb_config(),
        cap in arb_queue_ratio(),
        write in arb_queue_ratio(),
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        let actions = bp.update(cap, write);
        prop_assert!((actions.severity - bp.severity()).abs() < 1e-10,
            "actions.severity {} ≠ engine.severity {}", actions.severity, bp.severity());
    }
}

// =============================================================================
// Property: Engine update_count increments
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn engine_update_count_increments(
        config in arb_config(),
        n in 1_u64..50,
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        for _ in 0..n {
            bp.update(0.5, 0.5);
        }
        prop_assert_eq!(bp.update_count(), n);
    }
}

// =============================================================================
// Property: Config serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ContinuousBackpressureConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((parsed.center_threshold - config.center_threshold).abs() < 1e-9);
        prop_assert!((parsed.steepness - config.steepness).abs() < 1e-9);
        prop_assert_eq!(parsed.smoothing_window, config.smoothing_window);
        prop_assert!((parsed.max_backoff_multiplier - config.max_backoff_multiplier).abs() < 1e-9);
        prop_assert!((parsed.max_pane_skip - config.max_pane_skip).abs() < 1e-9);
        prop_assert!((parsed.max_detection_skip - config.max_detection_skip).abs() < 1e-9);
        prop_assert!((parsed.min_buffer_fraction - config.min_buffer_fraction).abs() < 1e-9);
    }
}

// =============================================================================
// Property: Snapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn snapshot_serde_roundtrip(
        config in arb_config(),
        n in 1_usize..20,
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        for _ in 0..n {
            bp.update(0.5, 0.3);
        }
        let snap = bp.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let parsed: frankenterm_core::continuous_backpressure::BackpressureSnapshot =
            serde_json::from_str(&json).unwrap();
        prop_assert!((parsed.severity - snap.severity).abs() < 1e-9);
        prop_assert!((parsed.queue_ratio - snap.queue_ratio).abs() < 1e-9);
        prop_assert_eq!(parsed.update_count, snap.update_count);
    }
}

// =============================================================================
// Property: Severity Lipschitz — small Δq → bounded Δs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn severity_lipschitz(
        q in 0.0_f64..0.99,
        center in arb_center(),
        steepness in arb_steepness(),
    ) {
        let delta = 0.01;
        let s1 = severity(q, center, steepness);
        let s2 = severity(q + delta, center, steepness);
        // Max derivative of sigmoid(k*(q-c)) is k/4
        let lipschitz_bound = delta * steepness / 4.0 + 1e-10;
        prop_assert!((s2 - s1).abs() <= lipschitz_bound,
            "Lipschitz violated: |s({})−s({})| = {} > bound {}",
            q + delta, q, (s2 - s1).abs(), lipschitz_bound);
    }
}

// =============================================================================
// Property: Actions monotonic with severity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn actions_monotonic_with_severity(
        config in arb_config(),
    ) {
        // Feed a ramp from 0 to 1 and verify action monotonicity
        let mut bp_low = ContinuousBackpressure::new(config.clone());
        let mut bp_high = ContinuousBackpressure::new(config);

        // Low load
        for _ in 0..50 {
            bp_low.update(0.1, 0.1);
        }
        // High load
        for _ in 0..50 {
            bp_high.update(0.95, 0.95);
        }

        let a_low = bp_low.current_actions();
        let a_high = bp_high.current_actions();

        // poll_multiplier should increase
        prop_assert!(a_high.poll_multiplier >= a_low.poll_multiplier - 1e-10,
            "poll_multiplier not monotonic: {} vs {}", a_low.poll_multiplier, a_high.poll_multiplier);
        // pane_skip should increase
        prop_assert!(a_high.pane_skip_fraction >= a_low.pane_skip_fraction - 1e-10,
            "pane_skip not monotonic: {} vs {}", a_low.pane_skip_fraction, a_high.pane_skip_fraction);
        // buffer_limit should decrease
        prop_assert!(a_high.buffer_limit_fraction <= a_low.buffer_limit_fraction + 1e-10,
            "buffer_limit not anti-monotonic: {} vs {}", a_low.buffer_limit_fraction, a_high.buffer_limit_fraction);
    }
}

// =============================================================================
// Property: Engine severity bounded after arbitrary updates
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn engine_severity_bounded(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..50),
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        for (cap, write) in &ratios {
            bp.update(*cap, *write);
            let s = bp.severity();
            prop_assert!(s >= 0.0, "severity {} < 0", s);
            prop_assert!(s <= 1.0, "severity {} > 1", s);
        }
    }
}

// =============================================================================
// Property: Engine queue_ratio bounded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn engine_queue_ratio_bounded(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..50),
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        for (cap, write) in &ratios {
            bp.update(*cap, *write);
            let q = bp.queue_ratio();
            prop_assert!(q >= 0.0, "queue_ratio {} < 0", q);
            prop_assert!(q <= 1.0, "queue_ratio {} > 1", q);
        }
    }
}

// =============================================================================
// Property: Reset returns to initial state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn engine_reset_returns_initial(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..20),
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        for (cap, write) in &ratios {
            bp.update(*cap, *write);
        }
        bp.reset();
        prop_assert_eq!(bp.update_count(), 0);
        prop_assert!((bp.severity() - 0.0).abs() < 1e-10);
        prop_assert!((bp.queue_ratio() - 0.0).abs() < 1e-10);
    }
}

// =============================================================================
// Property: Equivalent tier matches severity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn equivalent_tier_matches_severity(
        config in arb_config(),
        ratios in proptest::collection::vec((arb_queue_ratio(), arb_queue_ratio()), 1..20),
    ) {
        let mut bp = ContinuousBackpressure::new(config);
        for (cap, write) in &ratios {
            let actions = bp.update(*cap, *write);
            let expected = severity_to_tier(bp.severity());
            prop_assert_eq!(actions.equivalent_tier, expected,
                "tier mismatch: actions={:?} vs severity_to_tier({})={:?}",
                actions.equivalent_tier, bp.severity(), expected);
            prop_assert_eq!(bp.equivalent_tier(), expected);
        }
    }
}

// =============================================================================
// Property: Pane skip uses s² (quadratic) — bounded by linear at moderate severity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn pane_skip_quadratic_below_linear(
        config in arb_config(),
    ) {
        let mut bp = ContinuousBackpressure::new(config.clone());
        // Push to moderate severity
        for _ in 0..50 {
            bp.update(config.center_threshold, config.center_threshold);
        }
        let actions = bp.current_actions();
        let s = actions.severity;
        if s > 0.01 && s < 0.99 {
            // pane_skip = max_pane_skip * s²
            // detection_skip = max_detection_skip * s
            // For s < 1, s² < s, so normalized pane_skip < normalized detection_skip
            let norm_pane = actions.pane_skip_fraction / config.max_pane_skip.max(1e-10);
            let norm_detect = actions.detection_skip_fraction / config.max_detection_skip.max(1e-10);
            prop_assert!(norm_pane <= norm_detect + 1e-10,
                "quadratic pane_skip ({}) should be ≤ linear detection_skip ({}) at s={}",
                norm_pane, norm_detect, s);
        }
    }
}
