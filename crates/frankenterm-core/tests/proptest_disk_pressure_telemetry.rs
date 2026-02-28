//! Property-based tests for disk pressure monitor telemetry counters (ft-3kxe.20).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. updates tracks update_with_sample() calls
//! 3. updates_disabled tracks disabled skips
//! 4. tier_* counters track classification outcomes
//! 5. tier_transitions tracks tier changes
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;
use std::time::Instant;

use frankenterm_core::disk_pressure::{
    DiskPressureConfig, DiskPressureMonitor, DiskPressureTelemetrySnapshot,
    DiskPressureTier, DiskSample, PressureThresholds,
};

// =============================================================================
// Helpers
// =============================================================================

fn make_monitor() -> DiskPressureMonitor {
    DiskPressureMonitor::new(DiskPressureConfig {
        pid_kp: 0.0,
        pid_ki: 0.0,
        pid_kd: 0.0,
        target_usage_fraction: 0.0,
        ewma_alpha: 1.0, // no smoothing, raw values
        ..DiskPressureConfig::default()
    })
}

fn make_disabled_monitor() -> DiskPressureMonitor {
    DiskPressureMonitor::new(DiskPressureConfig {
        enabled: false,
        ..DiskPressureConfig::default()
    })
}

fn sample_with_usage(usage: f64) -> DiskSample {
    let total = 1_000_000_000u64;
    let available = ((1.0 - usage.clamp(0.0, 1.0)) * total as f64) as u64;
    DiskSample {
        available_bytes: available,
        total_bytes: total,
        usage_fraction: usage.clamp(0.0, 1.0),
        sampled_at: Instant::now(),
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let monitor = make_monitor();
    let snap = monitor.telemetry().snapshot();

    assert_eq!(snap.updates, 0);
    assert_eq!(snap.updates_disabled, 0);
    assert_eq!(snap.tier_green, 0);
    assert_eq!(snap.tier_yellow, 0);
    assert_eq!(snap.tier_red, 0);
    assert_eq!(snap.tier_black, 0);
    assert_eq!(snap.tier_transitions, 0);
}

#[test]
fn updates_tracked() {
    let mut monitor = make_monitor();

    monitor.update_with_sample(sample_with_usage(0.1));
    monitor.update_with_sample(sample_with_usage(0.2));
    monitor.update_with_sample(sample_with_usage(0.3));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.updates, 3);
}

#[test]
fn disabled_monitor_counts_disabled() {
    let mut monitor = make_disabled_monitor();

    monitor.update_with_sample(sample_with_usage(0.5));
    monitor.update_with_sample(sample_with_usage(0.5));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.updates, 0);
    assert_eq!(snap.updates_disabled, 2);
}

#[test]
fn green_tier_counted() {
    let mut monitor = make_monitor();
    // usage 0.1 < yellow(0.70) → Green
    monitor.update_with_sample(sample_with_usage(0.1));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.tier_green, 1);
    assert_eq!(snap.tier_yellow, 0);
}

#[test]
fn yellow_tier_counted() {
    let mut monitor = make_monitor();
    // usage 0.75 >= yellow(0.70) but < red(0.85) → Yellow
    monitor.update_with_sample(sample_with_usage(0.75));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.tier_yellow, 1);
}

#[test]
fn red_tier_counted() {
    let mut monitor = make_monitor();
    // usage 0.90 >= red(0.85) but < black(0.95) → Red
    monitor.update_with_sample(sample_with_usage(0.90));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.tier_red, 1);
}

#[test]
fn black_tier_counted() {
    let mut monitor = make_monitor();
    // usage 0.98 >= black(0.95) → Black
    monitor.update_with_sample(sample_with_usage(0.98));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.tier_black, 1);
}

#[test]
fn tier_transition_counted() {
    let mut monitor = make_monitor();

    // First update: Green (initial tier is Green, no transition)
    monitor.update_with_sample(sample_with_usage(0.1));
    assert_eq!(monitor.telemetry().snapshot().tier_transitions, 0);

    // Second update: Black → transition
    monitor.update_with_sample(sample_with_usage(0.98));
    assert_eq!(monitor.telemetry().snapshot().tier_transitions, 1);

    // Third update: back to Green → transition
    monitor.update_with_sample(sample_with_usage(0.1));
    assert_eq!(monitor.telemetry().snapshot().tier_transitions, 2);
}

#[test]
fn no_transition_when_tier_unchanged() {
    let mut monitor = make_monitor();

    monitor.update_with_sample(sample_with_usage(0.1));
    monitor.update_with_sample(sample_with_usage(0.2));
    monitor.update_with_sample(sample_with_usage(0.3));

    let snap = monitor.telemetry().snapshot();
    assert_eq!(snap.tier_transitions, 0);
    assert_eq!(snap.tier_green, 3);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = DiskPressureTelemetrySnapshot {
        updates: 500,
        updates_disabled: 10,
        tier_green: 400,
        tier_yellow: 60,
        tier_red: 30,
        tier_black: 10,
        tier_transitions: 15,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: DiskPressureTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn tier_counts_sum_to_updates() {
    let mut monitor = make_monitor();

    monitor.update_with_sample(sample_with_usage(0.1));  // Green
    monitor.update_with_sample(sample_with_usage(0.75)); // Yellow
    monitor.update_with_sample(sample_with_usage(0.90)); // Red
    monitor.update_with_sample(sample_with_usage(0.98)); // Black
    monitor.update_with_sample(sample_with_usage(0.1));  // Green

    let snap = monitor.telemetry().snapshot();
    let total_tiers = snap.tier_green + snap.tier_yellow + snap.tier_red + snap.tier_black;
    assert_eq!(total_tiers, snap.updates);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn updates_equals_call_count(
        count in 1usize..30,
    ) {
        let mut monitor = make_monitor();
        for _ in 0..count {
            monitor.update_with_sample(sample_with_usage(0.1));
        }
        let snap = monitor.telemetry().snapshot();
        prop_assert_eq!(snap.updates, count as u64);
    }

    #[test]
    fn tier_counts_sum_to_updates_prop(
        usages in prop::collection::vec(0.0f64..1.0, 1..30),
    ) {
        let mut monitor = make_monitor();
        for usage in &usages {
            monitor.update_with_sample(sample_with_usage(*usage));
        }
        let snap = monitor.telemetry().snapshot();
        let total = snap.tier_green + snap.tier_yellow + snap.tier_red + snap.tier_black;
        prop_assert_eq!(total, snap.updates,
            "tier counts ({}) != updates ({})", total, snap.updates);
    }

    #[test]
    fn counters_monotonically_increase(
        usages in prop::collection::vec(0.0f64..1.0, 1..30),
    ) {
        let mut monitor = make_monitor();
        let mut prev = monitor.telemetry().snapshot();

        for usage in &usages {
            monitor.update_with_sample(sample_with_usage(*usage));
            let snap = monitor.telemetry().snapshot();

            prop_assert!(snap.updates >= prev.updates,
                "updates decreased: {} -> {}", prev.updates, snap.updates);
            prop_assert!(snap.tier_green >= prev.tier_green,
                "tier_green decreased: {} -> {}", prev.tier_green, snap.tier_green);
            prop_assert!(snap.tier_yellow >= prev.tier_yellow,
                "tier_yellow decreased: {} -> {}", prev.tier_yellow, snap.tier_yellow);
            prop_assert!(snap.tier_red >= prev.tier_red,
                "tier_red decreased: {} -> {}", prev.tier_red, snap.tier_red);
            prop_assert!(snap.tier_black >= prev.tier_black,
                "tier_black decreased: {} -> {}", prev.tier_black, snap.tier_black);
            prop_assert!(snap.tier_transitions >= prev.tier_transitions,
                "tier_transitions decreased: {} -> {}",
                prev.tier_transitions, snap.tier_transitions);

            prev = snap;
        }
    }

    #[test]
    fn transitions_bounded_by_updates(
        usages in prop::collection::vec(0.0f64..1.0, 1..30),
    ) {
        let mut monitor = make_monitor();
        for usage in &usages {
            monitor.update_with_sample(sample_with_usage(*usage));
        }
        let snap = monitor.telemetry().snapshot();
        prop_assert!(
            snap.tier_transitions <= snap.updates,
            "transitions ({}) > updates ({})",
            snap.tier_transitions, snap.updates
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        updates in 0u64..100000,
        disabled in 0u64..10000,
        green in 0u64..50000,
        yellow in 0u64..20000,
        red in 0u64..10000,
        black in 0u64..5000,
        transitions in 0u64..10000,
    ) {
        let snap = DiskPressureTelemetrySnapshot {
            updates,
            updates_disabled: disabled,
            tier_green: green,
            tier_yellow: yellow,
            tier_red: red,
            tier_black: black,
            tier_transitions: transitions,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: DiskPressureTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
