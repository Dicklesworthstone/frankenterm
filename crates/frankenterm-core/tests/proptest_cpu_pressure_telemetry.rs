//! Property-based tests for CPU pressure monitor telemetry counters (ft-3kxe.31).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. samples_taken tracks sample() calls
//! 3. tier-specific sample counters sum to samples_taken
//! 4. Serde roundtrip for snapshot
//! 5. Counter monotonicity across samples

use proptest::prelude::*;

use frankenterm_core::cpu_pressure::{
    CpuPressureConfig, CpuPressureMonitor, CpuPressureTelemetrySnapshot, CpuPressureTier,
};
use std::sync::atomic::Ordering;

// =============================================================================
// Helpers
// =============================================================================

fn test_monitor() -> CpuPressureMonitor {
    CpuPressureMonitor::new(CpuPressureConfig::default())
}

fn arb_tier() -> impl Strategy<Value = CpuPressureTier> {
    prop_oneof![
        Just(CpuPressureTier::Green),
        Just(CpuPressureTier::Yellow),
        Just(CpuPressureTier::Orange),
        Just(CpuPressureTier::Red),
    ]
}

fn expected_tier_from_atomic(value: u64) -> CpuPressureTier {
    match value {
        1 => CpuPressureTier::Yellow,
        2 => CpuPressureTier::Orange,
        3 => CpuPressureTier::Red,
        _ => CpuPressureTier::Green,
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let mon = test_monitor();
    let snap = mon.telemetry().snapshot();

    assert_eq!(snap.samples_taken, 0);
    assert_eq!(snap.green_samples, 0);
    assert_eq!(snap.yellow_samples, 0);
    assert_eq!(snap.orange_samples, 0);
    assert_eq!(snap.red_samples, 0);
}

#[test]
fn sample_increments_samples_taken() {
    let mon = test_monitor();
    mon.sample();
    mon.sample();
    mon.sample();

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.samples_taken, 3);
}

#[test]
fn tier_counts_sum_to_total() {
    let mon = test_monitor();
    for _ in 0..5 {
        mon.sample();
    }

    let snap = mon.telemetry().snapshot();
    let tier_sum =
        snap.green_samples + snap.yellow_samples + snap.orange_samples + snap.red_samples;
    assert_eq!(tier_sum, snap.samples_taken);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = CpuPressureTelemetrySnapshot {
        samples_taken: 10000,
        green_samples: 8000,
        yellow_samples: 1500,
        orange_samples: 400,
        red_samples: 100,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: CpuPressureTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn samples_taken_equals_call_count(
        count in 1usize..20,
    ) {
        let mon = test_monitor();
        for _ in 0..count {
            mon.sample();
        }
        let snap = mon.telemetry().snapshot();
        prop_assert_eq!(snap.samples_taken, count as u64);
    }

    #[test]
    fn tier_counts_always_sum_to_total(
        count in 1usize..20,
    ) {
        let mon = test_monitor();
        for _ in 0..count {
            mon.sample();
        }
        let snap = mon.telemetry().snapshot();
        let tier_sum = snap.green_samples + snap.yellow_samples
            + snap.orange_samples + snap.red_samples;
        prop_assert_eq!(
            tier_sum, snap.samples_taken,
            "tier sum ({}) != samples_taken ({})",
            tier_sum, snap.samples_taken,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        count in 2usize..20,
    ) {
        let mon = test_monitor();
        let mut prev = mon.telemetry().snapshot();

        for _ in 0..count {
            mon.sample();
            let snap = mon.telemetry().snapshot();
            prop_assert!(snap.samples_taken >= prev.samples_taken,
                "samples_taken decreased: {} -> {}",
                prev.samples_taken, snap.samples_taken);
            prop_assert!(snap.green_samples >= prev.green_samples,
                "green_samples decreased");
            prop_assert!(snap.yellow_samples >= prev.yellow_samples,
                "yellow_samples decreased");
            prop_assert!(snap.orange_samples >= prev.orange_samples,
                "orange_samples decreased");
            prop_assert!(snap.red_samples >= prev.red_samples,
                "red_samples decreased");
            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        samples_taken in 0u64..100000,
        green_samples in 0u64..50000,
        yellow_samples in 0u64..30000,
        orange_samples in 0u64..10000,
        red_samples in 0u64..5000,
    ) {
        let snap = CpuPressureTelemetrySnapshot {
            samples_taken,
            green_samples,
            yellow_samples,
            orange_samples,
            red_samples,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: CpuPressureTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn proptest_cpu_pressure_tier_public_mapping_is_stable(tier in arb_tier()) {
        let (numeric, multiplier, display) = match tier {
            CpuPressureTier::Green => (0, 1, "GREEN"),
            CpuPressureTier::Yellow => (1, 2, "YELLOW"),
            CpuPressureTier::Orange => (2, 4, "ORANGE"),
            CpuPressureTier::Red => (3, 8, "RED"),
        };

        prop_assert_eq!(tier.as_u8(), numeric);
        prop_assert_eq!(tier.capture_interval_multiplier(), multiplier);
        prop_assert_eq!(tier.to_string(), display);
    }

    #[test]
    fn proptest_cpu_pressure_tier_serde_roundtrips(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).expect("serialize tier");
        let back: CpuPressureTier = serde_json::from_str(&json).expect("deserialize tier");

        prop_assert_eq!(back, tier);
        prop_assert_eq!(json, format!("\"{}\"", tier.to_string().to_ascii_lowercase()));
    }

    #[test]
    fn proptest_cpu_pressure_config_serde_preserves_public_fields(
        enabled in any::<bool>(),
        sample_interval_ms in 0u64..120_000,
        yellow_threshold in -1000.0f64..1000.0,
        orange_threshold in -1000.0f64..1000.0,
        red_threshold in -1000.0f64..1000.0,
    ) {
        let config = CpuPressureConfig {
            enabled,
            sample_interval_ms,
            yellow_threshold,
            orange_threshold,
            red_threshold,
        };

        let json = serde_json::to_string(&config).expect("serialize config");
        let back: CpuPressureConfig = serde_json::from_str(&json).expect("deserialize config");

        prop_assert_eq!(back.enabled, enabled);
        prop_assert_eq!(back.sample_interval_ms, sample_interval_ms);
        prop_assert_eq!(back.yellow_threshold.to_bits(), yellow_threshold.to_bits());
        prop_assert_eq!(back.orange_threshold.to_bits(), orange_threshold.to_bits());
        prop_assert_eq!(back.red_threshold.to_bits(), red_threshold.to_bits());
    }

    #[test]
    fn proptest_cpu_pressure_tier_handle_decodes_public_atomic_values(value in any::<u64>()) {
        let monitor = test_monitor();
        let handle = monitor.tier_handle();

        handle.store(value, Ordering::Relaxed);

        prop_assert_eq!(monitor.current_tier(), expected_tier_from_atomic(value));
    }
}
