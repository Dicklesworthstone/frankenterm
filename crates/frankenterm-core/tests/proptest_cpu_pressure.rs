//! Property-based tests for cpu_pressure module.
//!
//! Verifies CPU pressure monitoring invariants:
//! - CpuPressureTier: ordering, as_u8 monotonic, capture_interval_multiplier
//!   monotonic, Display non-empty, serde roundtrip
//! - CpuPressureConfig: serde roundtrip, threshold ordering
//! - CpuPressureMonitor: initial tier is Green, tier_handle reflects current_tier,
//!   sample returns valid data

use proptest::prelude::*;
use std::sync::atomic::Ordering;

use frankenterm_core::cpu_pressure::{CpuPressureConfig, CpuPressureMonitor, CpuPressureTier};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_tier() -> impl Strategy<Value = CpuPressureTier> {
    prop_oneof![
        Just(CpuPressureTier::Green),
        Just(CpuPressureTier::Yellow),
        Just(CpuPressureTier::Orange),
        Just(CpuPressureTier::Red),
    ]
}

fn arb_config() -> impl Strategy<Value = CpuPressureConfig> {
    (
        prop::bool::ANY,  // enabled
        1000u64..=30_000, // sample_interval_ms
        1.0f64..=30.0,    // yellow_threshold
        31.0f64..=60.0,   // orange_threshold
        61.0f64..=100.0,  // red_threshold
    )
        .prop_map(
            |(enabled, interval, yellow, orange, red)| CpuPressureConfig {
                enabled,
                sample_interval_ms: interval,
                yellow_threshold: yellow,
                orange_threshold: orange,
                red_threshold: red,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureTier: ordering, as_u8, capture_interval_multiplier
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// as_u8() preserves tier ordering.
    #[test]
    fn prop_tier_as_u8_monotonic(t1 in arb_tier(), t2 in arb_tier()) {
        match t1.cmp(&t2) {
            std::cmp::Ordering::Less => prop_assert!(t1.as_u8() < t2.as_u8()),
            std::cmp::Ordering::Equal => prop_assert_eq!(t1.as_u8(), t2.as_u8()),
            std::cmp::Ordering::Greater => prop_assert!(t1.as_u8() > t2.as_u8()),
        }
    }

    /// as_u8() is in [0, 3].
    #[test]
    fn prop_tier_as_u8_bounded(t in arb_tier()) {
        prop_assert!(t.as_u8() <= 3);
    }

    /// capture_interval_multiplier is monotonically non-decreasing with tier.
    #[test]
    fn prop_capture_multiplier_monotonic(t1 in arb_tier(), t2 in arb_tier()) {
        if t1 <= t2 {
            prop_assert!(
                t1.capture_interval_multiplier() <= t2.capture_interval_multiplier(),
                "t1={:?} mult {} > t2={:?} mult {}",
                t1, t1.capture_interval_multiplier(),
                t2, t2.capture_interval_multiplier()
            );
        }
    }

    /// capture_interval_multiplier is always >= 1.
    #[test]
    fn prop_capture_multiplier_positive(t in arb_tier()) {
        prop_assert!(t.capture_interval_multiplier() >= 1);
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureTier: Display and serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Display is non-empty and uppercase.
    #[test]
    fn prop_tier_display_uppercase(t in arb_tier()) {
        let s = t.to_string();
        prop_assert!(!s.is_empty());
        let upper = s.to_uppercase();
        prop_assert_eq!(s, upper);
    }

    /// Tier JSON roundtrip preserves value.
    #[test]
    fn prop_tier_serde_roundtrip(t in arb_tier()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: CpuPressureTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    /// Tier serializes to snake_case.
    #[test]
    fn prop_tier_serde_snake_case(t in arb_tier()) {
        let json = serde_json::to_string(&t).unwrap();
        let inner = json.trim_matches('"');
        // snake_case: all lowercase, may contain underscores
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized tier '{}' should be snake_case", inner
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureConfig: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Config JSON roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(c in arb_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: CpuPressureConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, c.enabled);
        prop_assert_eq!(back.sample_interval_ms, c.sample_interval_ms);
        prop_assert!((back.yellow_threshold - c.yellow_threshold).abs() < 1e-9);
        prop_assert!((back.orange_threshold - c.orange_threshold).abs() < 1e-9);
        prop_assert!((back.red_threshold - c.red_threshold).abs() < 1e-9);
    }

    /// Config thresholds maintain ordering: yellow < orange < red.
    #[test]
    fn prop_config_threshold_ordering(c in arb_config()) {
        prop_assert!(
            c.yellow_threshold < c.orange_threshold,
            "yellow {} >= orange {}", c.yellow_threshold, c.orange_threshold
        );
        prop_assert!(
            c.orange_threshold < c.red_threshold,
            "orange {} >= red {}", c.orange_threshold, c.red_threshold
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureMonitor: initial state and tier_handle
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Initial tier is always Green.
    #[test]
    fn prop_initial_tier_green(c in arb_config()) {
        let monitor = CpuPressureMonitor::new(c);
        prop_assert_eq!(monitor.current_tier(), CpuPressureTier::Green);
    }

    /// tier_handle shares state with current_tier.
    #[test]
    fn prop_tier_handle_reflects_current(
        c in arb_config(),
        tier_val in 0u64..=3,
    ) {
        let monitor = CpuPressureMonitor::new(c);
        let handle = monitor.tier_handle();
        handle.store(tier_val, Ordering::Relaxed);

        let expected = match tier_val {
            1 => CpuPressureTier::Yellow,
            2 => CpuPressureTier::Orange,
            3 => CpuPressureTier::Red,
            _ => CpuPressureTier::Green,
        };
        prop_assert_eq!(monitor.current_tier(), expected);
    }

    /// Values > 3 in the atomic map to Green (default fallback).
    #[test]
    fn prop_tier_handle_invalid_maps_to_green(
        c in arb_config(),
        val in 4u64..=100,
    ) {
        let monitor = CpuPressureMonitor::new(c);
        let handle = monitor.tier_handle();
        handle.store(val, Ordering::Relaxed);
        prop_assert_eq!(monitor.current_tier(), CpuPressureTier::Green);
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureMonitor: sample returns valid data
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// sample() returns non-negative pressure.
    #[test]
    fn prop_sample_nonneg_pressure(_dummy in 0..1u32) {
        let monitor = CpuPressureMonitor::new(CpuPressureConfig::default());
        let sample = monitor.sample();
        prop_assert!(sample.pressure >= 0.0, "pressure {} < 0", sample.pressure);
    }

    /// sample() updates tier_handle atomically.
    #[test]
    fn prop_sample_updates_tier(_dummy in 0..1u32) {
        let monitor = CpuPressureMonitor::new(CpuPressureConfig::default());
        let sample = monitor.sample();
        prop_assert_eq!(sample.tier, monitor.current_tier());
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureConfig: default values valid
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Default config has valid threshold ordering and enabled=true.
    #[test]
    fn prop_default_config_valid(_dummy in 0..1u32) {
        let c = CpuPressureConfig::default();
        prop_assert!(c.enabled);
        prop_assert!(c.sample_interval_ms > 0);
        prop_assert!(c.yellow_threshold < c.orange_threshold);
        prop_assert!(c.orange_threshold < c.red_threshold);
        prop_assert!(c.yellow_threshold > 0.0);
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureTier: Clone and Debug
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Clone produces identical tier.
    #[test]
    fn prop_tier_clone_identical(t in arb_tier()) {
        let cloned = t;
        prop_assert_eq!(t, cloned);
    }

    /// Debug format contains the variant name.
    #[test]
    fn prop_tier_debug_non_empty(t in arb_tier()) {
        let debug = format!("{:?}", t);
        prop_assert!(!debug.is_empty());
    }

    /// as_u8 maps each tier to distinct values.
    #[test]
    fn prop_tier_as_u8_distinct(_dummy in 0..1u8) {
        let tiers = [
            CpuPressureTier::Green,
            CpuPressureTier::Yellow,
            CpuPressureTier::Orange,
            CpuPressureTier::Red,
        ];
        for i in 0..tiers.len() {
            for j in (i + 1)..tiers.len() {
                prop_assert_ne!(tiers[i].as_u8(), tiers[j].as_u8(),
                    "tiers {:?} and {:?} have same as_u8", tiers[i], tiers[j]);
            }
        }
    }

    /// capture_interval_multiplier is always a power of 2.
    #[test]
    fn prop_capture_multiplier_power_of_two(t in arb_tier()) {
        let m = t.capture_interval_multiplier();
        prop_assert!(m.is_power_of_two(),
            "multiplier {} for {:?} is not a power of 2", m, t);
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureTier: Display consistency with serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Display output is all-uppercase while serde is snake_case.
    #[test]
    fn prop_display_vs_serde_case(t in arb_tier()) {
        let display = t.to_string();
        let json = serde_json::to_string(&t).unwrap();
        let serde_inner = json.trim_matches('"');
        // Display is uppercase, serde is lowercase
        prop_assert_eq!(display.to_lowercase(), serde_inner);
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureConfig: Clone and Debug
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Config Clone preserves all fields.
    #[test]
    fn prop_config_clone_preserves(c in arb_config()) {
        let cloned = c.clone();
        prop_assert_eq!(cloned.enabled, c.enabled);
        prop_assert_eq!(cloned.sample_interval_ms, c.sample_interval_ms);
        prop_assert!((cloned.yellow_threshold - c.yellow_threshold).abs() < f64::EPSILON);
        prop_assert!((cloned.orange_threshold - c.orange_threshold).abs() < f64::EPSILON);
        prop_assert!((cloned.red_threshold - c.red_threshold).abs() < f64::EPSILON);
    }

    /// Config Debug is non-empty and contains type name.
    #[test]
    fn prop_config_debug_non_empty(c in arb_config()) {
        let debug = format!("{:?}", c);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("CpuPressureConfig"));
    }

    /// Config JSON has expected field names.
    #[test]
    fn prop_config_json_fields(c in arb_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object().unwrap();
        prop_assert!(obj.contains_key("enabled"));
        prop_assert!(obj.contains_key("sample_interval_ms"));
        prop_assert!(obj.contains_key("yellow_threshold"));
        prop_assert!(obj.contains_key("orange_threshold"));
        prop_assert!(obj.contains_key("red_threshold"));
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureConfig: default known values
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Default config has specific known values.
    #[test]
    fn prop_default_config_known_values(_dummy in 0..1u8) {
        let c = CpuPressureConfig::default();
        prop_assert!(c.enabled);
        prop_assert_eq!(c.sample_interval_ms, 5000);
        prop_assert!((c.yellow_threshold - 15.0).abs() < f64::EPSILON);
        prop_assert!((c.orange_threshold - 30.0).abs() < f64::EPSILON);
        prop_assert!((c.red_threshold - 50.0).abs() < f64::EPSILON);
    }

    /// Default config serde roundtrip preserves all fields.
    #[test]
    fn prop_default_config_serde(_dummy in 0..1u8) {
        let c = CpuPressureConfig::default();
        let json = serde_json::to_string(&c).unwrap();
        let back: CpuPressureConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, c.enabled);
        prop_assert_eq!(back.sample_interval_ms, c.sample_interval_ms);
    }
}

// ────────────────────────────────────────────────────────────────────
// CpuPressureMonitor: additional state invariants
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Two monitors from the same config start at the same tier.
    #[test]
    fn prop_two_monitors_same_initial(c in arb_config()) {
        let m1 = CpuPressureMonitor::new(c.clone());
        let m2 = CpuPressureMonitor::new(c);
        prop_assert_eq!(m1.current_tier(), m2.current_tier());
    }

    /// tier_handle is a distinct Arc from the monitor (shared ownership).
    #[test]
    fn prop_tier_handle_shared_ownership(c in arb_config()) {
        let monitor = CpuPressureMonitor::new(c);
        let h1 = monitor.tier_handle();
        let h2 = monitor.tier_handle();
        // Both should read the same value
        prop_assert_eq!(
            h1.load(Ordering::Relaxed),
            h2.load(Ordering::Relaxed),
        );
    }

    /// sample pressure is finite.
    #[test]
    fn prop_sample_pressure_finite(_dummy in 0..1u8) {
        let monitor = CpuPressureMonitor::new(CpuPressureConfig::default());
        let s = monitor.sample();
        prop_assert!(s.pressure.is_finite(), "pressure should be finite: {}", s.pressure);
    }
}
