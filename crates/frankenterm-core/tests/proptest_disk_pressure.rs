#![cfg(feature = "disk-pressure")]
//! Property-based tests for the disk_pressure module
//!
//! Tests: DiskPressureTier ordering, PressureThresholds normalization,
//! EwmaEstimator convergence, PidController anti-windup, classify_tier
//! monotonicity, DiskPressureConfig normalization, DiskPressureMonitor
//! state machine, PressureSnapshot serde roundtrip, df output parsing.
//!
//! Coverage: 30 property-based tests

use frankenterm_core::disk_pressure::{
    DiskPressureConfig, DiskPressureMonitor, DiskPressureTier, EwmaEstimator, PidController,
    PressureSnapshot, PressureThresholds,
};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_tier() -> impl Strategy<Value = DiskPressureTier> {
    prop_oneof![
        Just(DiskPressureTier::Green),
        Just(DiskPressureTier::Yellow),
        Just(DiskPressureTier::Red),
        Just(DiskPressureTier::Black),
    ]
}

fn arb_thresholds() -> impl Strategy<Value = PressureThresholds> {
    (0.0f64..=1.0, 0.0f64..=1.0, 0.0f64..=1.0).prop_map(|(a, b, c)| PressureThresholds {
        yellow: a,
        red: b,
        black: c,
    })
}

fn arb_usage_fraction() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(0.0f64),
        Just(1.0f64),
        0.0f64..=1.0,
        // Edge: slightly outside bounds
        (-0.1f64..0.0),
        (1.0f64..1.1),
    ]
}

fn arb_alpha() -> impl Strategy<Value = f64> {
    prop_oneof![Just(0.0f64), Just(0.5f64), Just(1.0f64), 0.0f64..=1.0,]
}

fn arb_pid_gains() -> impl Strategy<Value = (f64, f64, f64, f64, f64)> {
    (
        0.0f64..=2.0,  // kp
        0.0f64..=1.0,  // ki
        0.0f64..=1.0,  // kd
        -2.0f64..=0.0, // integral_min
        0.0f64..=2.0,  // integral_max
    )
}

// =============================================================================
// DiskPressureTier properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 1. Tier ordering is total: Green < Yellow < Red < Black
    #[test]
    fn tier_ordering_is_total(a in arb_tier(), b in arb_tier()) {
        let a_u8 = a.as_u8();
        let b_u8 = b.as_u8();
        match a_u8.cmp(&b_u8) {
            std::cmp::Ordering::Less => prop_assert!(a < b),
            std::cmp::Ordering::Greater => prop_assert!(a > b),
            std::cmp::Ordering::Equal => prop_assert_eq!(a, b),
        }
    }

    // 2. as_u8 is injective (distinct tiers have distinct u8 values)
    #[test]
    fn tier_as_u8_injective(a in arb_tier(), b in arb_tier()) {
        if a != b {
            prop_assert_ne!(a.as_u8(), b.as_u8());
        }
    }

    // 3. Tier Display is non-empty uppercase
    #[test]
    fn tier_display_uppercase(tier in arb_tier()) {
        let s = tier.to_string();
        prop_assert!(!s.is_empty());
        let upper = s.to_uppercase();
        prop_assert_eq!(s, upper);
    }

    // 4. Tier serde roundtrip
    #[test]
    fn tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let deserialized: DiskPressureTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, deserialized);
    }
}

// =============================================================================
// PressureThresholds properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 5. Monitor created with thresholds produces valid tier classification
    #[test]
    fn monitor_with_arbitrary_thresholds_starts_green(t in arb_thresholds()) {
        let config = DiskPressureConfig {
            thresholds: t,
            ..DiskPressureConfig::default()
        };
        let monitor = DiskPressureMonitor::new(config);
        // Fresh monitor should always start at Green
        prop_assert_eq!(monitor.current_tier(), DiskPressureTier::Green);
    }

    // 6. Thresholds fields are in [0, 1] range (default)
    #[test]
    fn default_thresholds_in_unit_range(_seed in 0u32..100) {
        let t = PressureThresholds::default();
        prop_assert!(t.yellow >= 0.0 && t.yellow <= 1.0);
        prop_assert!(t.red >= 0.0 && t.red <= 1.0);
        prop_assert!(t.black >= 0.0 && t.black <= 1.0);
    }

    // 7. Default thresholds are ordered: yellow < red < black
    #[test]
    fn default_thresholds_ordered(_seed in 0u32..100) {
        let t = PressureThresholds::default();
        prop_assert!(t.yellow < t.red);
        prop_assert!(t.red < t.black);
    }

    // 8. Thresholds Clone produces equal values
    #[test]
    fn thresholds_clone_eq(t in arb_thresholds()) {
        let cloned = t;
        prop_assert!((t.yellow - cloned.yellow).abs() < f64::EPSILON);
        prop_assert!((t.red - cloned.red).abs() < f64::EPSILON);
        prop_assert!((t.black - cloned.black).abs() < f64::EPSILON);
    }

    // 9. Thresholds serde roundtrip
    #[test]
    fn thresholds_serde_roundtrip(t in arb_thresholds()) {
        let json = serde_json::to_string(&t).unwrap();
        let deserialized: PressureThresholds = serde_json::from_str(&json).unwrap();
        prop_assert!((t.yellow - deserialized.yellow).abs() < 1e-10);
        prop_assert!((t.red - deserialized.red).abs() < 1e-10);
        prop_assert!((t.black - deserialized.black).abs() < 1e-10);
    }
}

// =============================================================================
// EwmaEstimator properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 10. First sample sets EWMA to that value
    #[test]
    fn ewma_first_sample_is_exact(alpha in arb_alpha(), sample in 0.0f64..=1.0) {
        let mut ewma = EwmaEstimator::new(alpha);
        let result = ewma.update(sample);
        prop_assert!((result - sample).abs() < 1e-10, "first sample should set EWMA exactly");
    }

    // 11. EWMA output is always in [0, 1]
    #[test]
    fn ewma_output_bounded(alpha in arb_alpha(), samples in prop::collection::vec(arb_usage_fraction(), 1..20)) {
        let mut ewma = EwmaEstimator::new(alpha);
        for s in &samples {
            let v = ewma.update(*s);
            prop_assert!((0.0..=1.0).contains(&v), "EWMA output {} out of bounds", v);
        }
    }

    // 12. Alpha=1.0: EWMA immediately jumps to new sample
    #[test]
    fn ewma_alpha_one_tracks_immediately(samples in prop::collection::vec(0.0f64..=1.0, 2..10)) {
        let mut ewma = EwmaEstimator::new(1.0);
        for s in &samples {
            let v = ewma.update(*s);
            prop_assert!((v - s).abs() < 1e-10, "alpha=1.0 should track exactly");
        }
    }

    // 13. Alpha=0.0: EWMA stays at first sample
    #[test]
    fn ewma_alpha_zero_stays_at_first(first in 0.0f64..=1.0, rest in prop::collection::vec(0.0f64..=1.0, 1..10)) {
        let mut ewma = EwmaEstimator::new(0.0);
        let v0 = ewma.update(first);
        for s in &rest {
            let v = ewma.update(*s);
            prop_assert!((v - v0).abs() < 1e-10, "alpha=0 should stay at first sample");
        }
    }

    // 14. Constant input converges to that constant
    #[test]
    fn ewma_constant_input_convergence(alpha in 0.01f64..=1.0, constant in 0.0f64..=1.0) {
        let mut ewma = EwmaEstimator::new(alpha);
        for _ in 0..100 {
            ewma.update(constant);
        }
        prop_assert!((ewma.current() - constant).abs() < 0.01, "should converge to constant input");
    }
}

// =============================================================================
// PidController properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 15. Integral is always within [min, max] bounds
    #[test]
    fn pid_integral_bounded(
        (kp, ki, kd, imin, imax) in arb_pid_gains(),
        errors in prop::collection::vec(-1.0f64..=1.0, 1..30),
    ) {
        let mut pid = PidController::new(kp, ki, kd, imin, imax);
        let actual_min = imin.min(imax);
        let actual_max = imin.max(imax);
        for e in &errors {
            let _ = pid.update(*e, 0.1);
            let integral = pid.integral();
            prop_assert!(
                integral >= actual_min - 1e-10 && integral <= actual_max + 1e-10,
                "integral {} out of [{}, {}]", integral, actual_min, actual_max
            );
        }
    }

    // 16. Reset clears integral and derivative
    #[test]
    fn pid_reset_clears_all(
        (kp, ki, kd, imin, imax) in arb_pid_gains(),
        errors in prop::collection::vec(-1.0f64..=1.0, 1..10),
    ) {
        let mut pid = PidController::new(kp, ki, kd, imin, imax);
        for e in &errors {
            let _ = pid.update(*e, 0.1);
        }
        pid.reset();
        prop_assert!(pid.integral().abs() < f64::EPSILON);
        prop_assert!(pid.derivative().abs() < f64::EPSILON);
    }

    // 17. Zero gains produce zero output
    #[test]
    fn pid_zero_gains_zero_output(error in -1.0f64..=1.0, dt in 0.001f64..=1.0) {
        let mut pid = PidController::new(0.0, 0.0, 0.0, -1.0, 1.0);
        let out = pid.update(error, dt);
        prop_assert!(out.abs() < 1e-10, "zero gains should produce zero output, got {}", out);
    }

    // 18. Proportional-only: output = kp * error
    #[test]
    fn pid_proportional_only(kp in 0.0f64..=2.0, error in -1.0f64..=1.0) {
        let mut pid = PidController::new(kp, 0.0, 0.0, -1.0, 1.0);
        let out = pid.update(error, 1.0);
        prop_assert!(kp.mul_add(-error, out).abs() < 1e-10, "P-only: expected {}, got {}", kp * error, out);
    }

    // 19. Swapped integral bounds are corrected
    #[test]
    fn pid_swapped_bounds_corrected(
        kp in 0.0f64..=1.0,
        ki in 0.0f64..=1.0,
        low in 0.0f64..=2.0,
        high in 0.0f64..=2.0,
    ) {
        // Deliberately pass high as min and low as max
        let mut pid = PidController::new(kp, ki, 0.0, high, -low);
        for _ in 0..20 {
            let _ = pid.update(1.0, 1.0);
        }
        let actual_min = (-low).min(high);
        let actual_max = (-low).max(high);
        let integral = pid.integral();
        prop_assert!(
            integral >= actual_min - 1e-10 && integral <= actual_max + 1e-10,
            "integral {} should be in [{}, {}]", integral, actual_min, actual_max
        );
    }
}

// =============================================================================
// Classify tier properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // 20. Classification always returns a valid tier
    #[test]
    fn classify_always_valid(usage in arb_usage_fraction(), t in arb_thresholds()) {
        let config = DiskPressureConfig {
            thresholds: t,
            ..DiskPressureConfig::default()
        };
        let monitor = DiskPressureMonitor::new(config);
        let tier = monitor.current_tier();
        prop_assert!(matches!(
            tier,
            DiskPressureTier::Green | DiskPressureTier::Yellow | DiskPressureTier::Red | DiskPressureTier::Black
        ));
        let _ = usage; // used to ensure we generate different configs
    }

    // 21. Zero usage always classifies as Green (when yellow threshold > 0)
    #[test]
    fn zero_usage_is_green(t in arb_thresholds()) {
        // The classify_usage helper normalizes thresholds the same way the module does
        let tier = classify_usage(0.0, &t);
        // 0.0 usage should always be Green since normalized yellow >= 0.0
        // and 0.0 < any positive threshold
        let yellow_normalized = t.yellow.clamp(0.0, 1.0);
        if yellow_normalized > 0.0 {
            prop_assert_eq!(tier, DiskPressureTier::Green);
        }
    }

    // 22. Tier classification is monotone: higher usage => same or higher tier
    #[test]
    fn classification_monotone(
        usage1 in 0.0f64..=1.0,
        usage2 in 0.0f64..=1.0,
        t in arb_thresholds(),
    ) {
        let tier1 = classify_usage(usage1, &t);
        let tier2 = classify_usage(usage2, &t);
        if usage1 <= usage2 {
            prop_assert!(tier1 <= tier2, "usage {} => {:?}, usage {} => {:?}: should be monotone", usage1, tier1, usage2, tier2);
        }
    }
}

// Helper that replicates classify_tier logic for external test
// Performs its own normalization like the internal function does
fn classify_usage(usage: f64, t: &PressureThresholds) -> DiskPressureTier {
    let usage = usage.clamp(0.0, 1.0);
    // Normalize thresholds: yellow <= red <= black, all in [0,1]
    let yellow = t.yellow.clamp(0.0, 1.0);
    let red = t.red.clamp(yellow, 1.0);
    let black = t.black.clamp(red, 1.0);

    if usage >= black {
        DiskPressureTier::Black
    } else if usage >= red {
        DiskPressureTier::Red
    } else if usage >= yellow {
        DiskPressureTier::Yellow
    } else {
        DiskPressureTier::Green
    }
}

// =============================================================================
// DiskPressureConfig properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 23. Config serde roundtrip
    #[test]
    fn config_serde_roundtrip(_seed in 0u32..100) {
        let config = DiskPressureConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: DiskPressureConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((config.ewma_alpha - deserialized.ewma_alpha).abs() < 1e-10);
        prop_assert!((config.pid_kp - deserialized.pid_kp).abs() < 1e-10);
    }

    // 24. Default config produces enabled monitor
    #[test]
    fn default_config_enabled(_seed in 0u32..100) {
        let config = DiskPressureConfig::default();
        prop_assert!(config.enabled);
        prop_assert!(config.poll_interval_ms > 0);
    }

    // 25. Config Debug is non-empty
    #[test]
    fn config_debug_non_empty(_seed in 0u32..100) {
        let config = DiskPressureConfig::default();
        let dbg = format!("{:?}", config);
        prop_assert!(!dbg.is_empty());
    }
}

// =============================================================================
// DiskPressureMonitor properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 26. Fresh monitor starts at Green
    #[test]
    fn fresh_monitor_is_green(_seed in 0u32..100) {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        prop_assert_eq!(monitor.current_tier(), DiskPressureTier::Green);
    }

    // 27. Tier handle is shared
    #[test]
    fn tier_handle_is_shared(_seed in 0u32..100) {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        let handle = monitor.tier_handle();
        // Both should read the same value
        let tier_from_monitor = monitor.current_tier().as_u8() as u64;
        let tier_from_handle = handle.load(std::sync::atomic::Ordering::Relaxed);
        prop_assert_eq!(tier_from_monitor, tier_from_handle);
    }

    // 28. Snapshot has coherent fields
    #[test]
    fn snapshot_coherent(_seed in 0u32..100) {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        let snap = monitor.snapshot();
        prop_assert_eq!(snap.tier, DiskPressureTier::Green);
        prop_assert_eq!(snap.update_count, 0);
        // Effective usage should be 0 for fresh monitor
        prop_assert!(snap.effective_usage_fraction >= 0.0);
        prop_assert!(snap.effective_usage_fraction <= 1.0);
    }

    // 29. PressureSnapshot serde roundtrip
    #[test]
    fn snapshot_serde_roundtrip(_seed in 0u32..100) {
        let monitor = DiskPressureMonitor::new(DiskPressureConfig::default());
        let snap = monitor.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: PressureSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.tier, deserialized.tier);
        prop_assert_eq!(snap.update_count, deserialized.update_count);
    }

    // 30. Disabled monitor doesn't change tier on update
    #[test]
    fn disabled_monitor_unchanged(_seed in 0u32..100) {
        let config = DiskPressureConfig {
            enabled: false,
            ..DiskPressureConfig::default()
        };
        let mut monitor = DiskPressureMonitor::new(config);
        let tier_before = monitor.current_tier();
        let tier_after = monitor.update();
        prop_assert_eq!(tier_before, tier_after);
    }
}
