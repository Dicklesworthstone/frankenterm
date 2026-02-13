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
        if t1 < t2 {
            prop_assert!(t1.as_u8() < t2.as_u8());
        } else if t1 == t2 {
            prop_assert_eq!(t1.as_u8(), t2.as_u8());
        } else {
            prop_assert!(t1.as_u8() > t2.as_u8());
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
