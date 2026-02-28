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
    CpuPressureConfig, CpuPressureMonitor, CpuPressureTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_monitor() -> CpuPressureMonitor {
    CpuPressureMonitor::new(CpuPressureConfig::default())
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
}
