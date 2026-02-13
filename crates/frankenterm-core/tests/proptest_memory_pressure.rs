//! Property-based tests for memory_pressure module.
//!
//! Verifies memory pressure monitoring invariants:
//! - MemoryPressureTier: ordering, as_u8 monotonic, Display UPPERCASE, serde roundtrip
//! - MemoryAction: Display, serde roundtrip
//! - suggested_action monotonic with tier
//! - MemoryPressureConfig: serde roundtrip, default values, threshold ordering
//! - PaneMemoryInfo: serde roundtrip
//! - MemoryPressureMonitor: initial tier Green, classify monotonic, tier_handle

use proptest::prelude::*;
use std::sync::atomic::Ordering;

use frankenterm_core::memory_pressure::{
    MemoryAction, MemoryPressureConfig, MemoryPressureMonitor, MemoryPressureTier, PaneMemoryInfo,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_tier() -> impl Strategy<Value = MemoryPressureTier> {
    prop_oneof![
        Just(MemoryPressureTier::Green),
        Just(MemoryPressureTier::Yellow),
        Just(MemoryPressureTier::Orange),
        Just(MemoryPressureTier::Red),
    ]
}

fn arb_action() -> impl Strategy<Value = MemoryAction> {
    prop_oneof![
        Just(MemoryAction::None),
        Just(MemoryAction::CompressIdle),
        Just(MemoryAction::EvictToDisk),
        Just(MemoryAction::EmergencyCleanup),
    ]
}

fn arb_config() -> impl Strategy<Value = MemoryPressureConfig> {
    (
        prop::bool::ANY,         // enabled
        1000u64..=60_000,        // sample_interval_ms
        1.0f64..=40.0,           // yellow_threshold
        41.0f64..=70.0,          // orange_threshold
        71.0f64..=100.0,         // red_threshold
        60u64..=600,             // compress_idle_secs
        600u64..=7200,           // evict_idle_secs
    )
        .prop_map(|(enabled, interval, yellow, orange, red, compress, evict)| {
            MemoryPressureConfig {
                enabled,
                sample_interval_ms: interval,
                yellow_threshold: yellow,
                orange_threshold: orange,
                red_threshold: red,
                compress_idle_secs: compress,
                evict_idle_secs: evict,
            }
        })
}

fn arb_pane_memory_info() -> impl Strategy<Value = PaneMemoryInfo> {
    (
        0u64..=1_000_000,      // pane_id
        0u64..=100_000_000,    // rss_kb
        prop::bool::ANY,       // scrollback_compressed
        prop::bool::ANY,       // scrollback_evicted
        0u64..=86_400,         // idle_secs
    )
        .prop_map(|(pane_id, rss_kb, compressed, evicted, idle)| PaneMemoryInfo {
            pane_id,
            rss_kb,
            scrollback_compressed: compressed,
            scrollback_evicted: evicted,
            idle_secs: idle,
        })
}

// ────────────────────────────────────────────────────────────────────
// MemoryPressureTier: ordering, as_u8, Display
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

    /// as_u8() is bounded in [0, 3].
    #[test]
    fn prop_tier_as_u8_bounded(t in arb_tier()) {
        prop_assert!(t.as_u8() <= 3);
    }

    /// Display is non-empty and UPPERCASE.
    #[test]
    fn prop_tier_display_uppercase(t in arb_tier()) {
        let d = t.to_string();
        prop_assert!(!d.is_empty());
        let upper = d.to_uppercase();
        prop_assert!(d == upper, "Display should be UPPERCASE, got '{}'", d);
    }

    /// Tier serde roundtrip.
    #[test]
    fn prop_tier_serde_roundtrip(t in arb_tier()) {
        let json = serde_json::to_string(&t).unwrap();
        let back: MemoryPressureTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(t, back);
    }

    /// Tier serializes to snake_case.
    #[test]
    fn prop_tier_serde_snake_case(t in arb_tier()) {
        let json = serde_json::to_string(&t).unwrap();
        let inner = json.trim_matches('"');
        prop_assert!(
            inner.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "serialized tier should be snake_case, got '{}'", inner
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// MemoryAction: Display, serde
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// MemoryAction Display is non-empty and snake_case.
    #[test]
    fn prop_action_display_snake_case(a in arb_action()) {
        let d = a.to_string();
        prop_assert!(!d.is_empty());
        prop_assert!(
            d.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display should be snake_case, got '{}'", d
        );
    }

    /// MemoryAction serde roundtrip.
    #[test]
    fn prop_action_serde_roundtrip(a in arb_action()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: MemoryAction = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// ────────────────────────────────────────────────────────────────────
// suggested_action monotonic with tier
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// suggested_action severity is monotonically non-decreasing with tier.
    #[test]
    fn prop_suggested_action_monotonic(t1 in arb_tier(), t2 in arb_tier()) {
        if t1 <= t2 {
            let a1 = action_severity(t1.suggested_action());
            let a2 = action_severity(t2.suggested_action());
            prop_assert!(a1 <= a2,
                "tier {:?} action {:?} (sev {}) > tier {:?} action {:?} (sev {})",
                t1, t1.suggested_action(), a1,
                t2, t2.suggested_action(), a2
            );
        }
    }

    /// Green tier has no action.
    #[test]
    fn prop_green_no_action(_dummy in 0..1u32) {
        prop_assert_eq!(MemoryPressureTier::Green.suggested_action(), MemoryAction::None);
    }

    /// Red tier has emergency cleanup.
    #[test]
    fn prop_red_emergency(_dummy in 0..1u32) {
        prop_assert_eq!(
            MemoryPressureTier::Red.suggested_action(),
            MemoryAction::EmergencyCleanup
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// MemoryPressureConfig: serde roundtrip, defaults
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Config serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(c in arb_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: MemoryPressureConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.enabled, c.enabled);
        prop_assert_eq!(back.sample_interval_ms, c.sample_interval_ms);
        prop_assert!((back.yellow_threshold - c.yellow_threshold).abs() < 1e-9);
        prop_assert!((back.orange_threshold - c.orange_threshold).abs() < 1e-9);
        prop_assert!((back.red_threshold - c.red_threshold).abs() < 1e-9);
        prop_assert_eq!(back.compress_idle_secs, c.compress_idle_secs);
        prop_assert_eq!(back.evict_idle_secs, c.evict_idle_secs);
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

    /// Default config has valid threshold ordering and enabled=true.
    #[test]
    fn prop_default_config_valid(_dummy in 0..1u32) {
        let c = MemoryPressureConfig::default();
        prop_assert!(c.enabled);
        prop_assert!(c.sample_interval_ms > 0);
        prop_assert!(c.yellow_threshold < c.orange_threshold);
        prop_assert!(c.orange_threshold < c.red_threshold);
        prop_assert!(c.yellow_threshold > 0.0);
        prop_assert!(c.compress_idle_secs > 0);
        prop_assert!(c.evict_idle_secs > c.compress_idle_secs);
    }
}

// ────────────────────────────────────────────────────────────────────
// PaneMemoryInfo: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// PaneMemoryInfo JSON roundtrip preserves all fields.
    #[test]
    fn prop_pane_info_serde_roundtrip(info in arb_pane_memory_info()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: PaneMemoryInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, info.pane_id);
        prop_assert_eq!(back.rss_kb, info.rss_kb);
        prop_assert_eq!(back.scrollback_compressed, info.scrollback_compressed);
        prop_assert_eq!(back.scrollback_evicted, info.scrollback_evicted);
        prop_assert_eq!(back.idle_secs, info.idle_secs);
    }
}

// ────────────────────────────────────────────────────────────────────
// MemoryPressureMonitor: initial state, classify, tier_handle
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Initial tier is always Green.
    #[test]
    fn prop_initial_tier_green(c in arb_config()) {
        let monitor = MemoryPressureMonitor::new(c);
        prop_assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
    }

    /// tier_handle shares state with current_tier.
    #[test]
    fn prop_tier_handle_reflects_current(
        c in arb_config(),
        tier_val in 0u64..=3,
    ) {
        let monitor = MemoryPressureMonitor::new(c);
        let handle = monitor.tier_handle();
        handle.store(tier_val, Ordering::Relaxed);

        let expected = match tier_val {
            1 => MemoryPressureTier::Yellow,
            2 => MemoryPressureTier::Orange,
            3 => MemoryPressureTier::Red,
            _ => MemoryPressureTier::Green,
        };
        prop_assert_eq!(monitor.current_tier(), expected);
    }

    /// Values > 3 in the atomic map to Green (default fallback).
    #[test]
    fn prop_tier_handle_invalid_maps_to_green(
        c in arb_config(),
        val in 4u64..=100,
    ) {
        let monitor = MemoryPressureMonitor::new(c);
        let handle = monitor.tier_handle();
        handle.store(val, Ordering::Relaxed);
        prop_assert_eq!(monitor.current_tier(), MemoryPressureTier::Green);
    }
}

// ────────────────────────────────────────────────────────────────────
// MemoryPressureMonitor: sample returns valid data
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// sample() returns non-negative pressure and tier matches current_tier.
    #[test]
    fn prop_sample_valid(_dummy in 0..1u32) {
        let monitor = MemoryPressureMonitor::new(MemoryPressureConfig::default());
        let sample = monitor.sample();
        prop_assert!(sample.used_percent >= 0.0, "used_percent {} < 0", sample.used_percent);
        prop_assert_eq!(sample.tier, monitor.current_tier());
    }
}

// ────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────

fn action_severity(action: MemoryAction) -> u8 {
    match action {
        MemoryAction::None => 0,
        MemoryAction::CompressIdle => 1,
        MemoryAction::EvictToDisk => 2,
        MemoryAction::EmergencyCleanup => 3,
    }
}
