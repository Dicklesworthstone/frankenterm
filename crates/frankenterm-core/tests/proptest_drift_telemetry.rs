//! Property-based tests for drift monitor telemetry counters (ft-3kxe.35).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. rules_registered tracks register_rule / auto-register calls
//! 3. rules_unregistered tracks unregister_rule calls
//! 4. observations tracks observe() calls
//! 5. drifts_detected = rate_drops + rate_spikes
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::drift::{DriftConfig, DriftMonitor, DriftTelemetrySnapshot};

// =============================================================================
// Helpers
// =============================================================================

fn test_monitor() -> DriftMonitor {
    DriftMonitor::new(DriftConfig::default())
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let mon = test_monitor();
    let snap = mon.telemetry().snapshot();

    assert_eq!(snap.rules_registered, 0);
    assert_eq!(snap.rules_unregistered, 0);
    assert_eq!(snap.observations, 0);
    assert_eq!(snap.drifts_detected, 0);
    assert_eq!(snap.rate_drops, 0);
    assert_eq!(snap.rate_spikes, 0);
    assert_eq!(snap.resets, 0);
}

#[test]
fn rules_registered_tracked() {
    let mut mon = test_monitor();
    mon.register_rule("rule_a");
    mon.register_rule("rule_b");
    mon.register_rule("rule_c");

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.rules_registered, 3);
}

#[test]
fn duplicate_registration_not_counted() {
    let mut mon = test_monitor();
    mon.register_rule("rule_a");
    mon.register_rule("rule_a");
    mon.register_rule("rule_a");

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.rules_registered, 1);
}

#[test]
fn rules_unregistered_tracked() {
    let mut mon = test_monitor();
    mon.register_rule("rule_a");
    mon.register_rule("rule_b");
    mon.unregister_rule("rule_a");

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.rules_unregistered, 1);
}

#[test]
fn unregister_nonexistent_not_counted() {
    let mut mon = test_monitor();
    mon.unregister_rule("nonexistent");

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.rules_unregistered, 0);
}

#[test]
fn observations_tracked() {
    let mut mon = test_monitor();
    mon.observe("rule_a", 5.0);
    mon.observe("rule_a", 5.0);
    mon.observe("rule_b", 3.0);

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
}

#[test]
fn auto_register_via_observe_counted() {
    let mut mon = test_monitor();
    mon.observe("new_rule", 1.0);

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.rules_registered, 1);
    assert_eq!(snap.observations, 1);
}

#[test]
fn resets_tracked() {
    let mut mon = test_monitor();
    mon.reset();
    mon.reset();

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.resets, 2);
}

#[test]
fn drift_type_invariant() {
    let mut mon = test_monitor();
    // Feed enough data to potentially trigger drift
    for _ in 0..50 {
        mon.observe("rule_a", 10.0);
    }
    for _ in 0..50 {
        mon.observe("rule_a", 0.1);
    }

    let snap = mon.telemetry().snapshot();
    assert_eq!(snap.drifts_detected, snap.rate_drops + snap.rate_spikes);
}

#[test]
fn drifts_bounded_by_observations() {
    let mut mon = test_monitor();
    for i in 0..20 {
        mon.observe("rule_a", (i % 10) as f64);
    }

    let snap = mon.telemetry().snapshot();
    assert!(snap.drifts_detected <= snap.observations);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = DriftTelemetrySnapshot {
        rules_registered: 100,
        rules_unregistered: 20,
        observations: 50000,
        drifts_detected: 150,
        rate_drops: 80,
        rate_spikes: 70,
        resets: 5,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: DriftTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn observations_equals_call_count(
        count in 1usize..30,
    ) {
        let mut mon = test_monitor();
        for i in 0..count {
            mon.observe(&format!("rule_{}", i % 5), 1.0);
        }
        let snap = mon.telemetry().snapshot();
        prop_assert_eq!(snap.observations, count as u64);
    }

    #[test]
    fn drift_type_sum_invariant(
        ops in prop::collection::vec(0u8..3, 1..40),
    ) {
        let mut mon = test_monitor();
        let mut op_idx = 0u64;

        for op in &ops {
            match op {
                0 => {
                    // Stable observations (unlikely to drift)
                    mon.observe("stable", 5.0);
                }
                1 => {
                    // Varied observations
                    let rate = if op_idx % 2 == 0 { 100.0 } else { 0.1 };
                    mon.observe("varying", rate);
                }
                2 => {
                    mon.register_rule(&format!("rule_{}", op_idx));
                }
                _ => unreachable!(),
            }
            op_idx += 1;
        }

        let snap = mon.telemetry().snapshot();
        prop_assert_eq!(
            snap.drifts_detected,
            snap.rate_drops + snap.rate_spikes,
            "drifts ({}) != drops ({}) + spikes ({})",
            snap.drifts_detected, snap.rate_drops, snap.rate_spikes,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..30),
    ) {
        let mut mon = test_monitor();
        let mut prev = mon.telemetry().snapshot();
        let mut next_id = 0u64;

        for op in &ops {
            match op {
                0 => { mon.register_rule(&format!("r{}", next_id)); next_id += 1; }
                1 => { mon.observe(&format!("r{}", next_id % 3), 5.0); }
                2 => { mon.observe(&format!("r{}", next_id % 3), 0.1); }
                3 => { mon.unregister_rule(&format!("r{}", next_id.saturating_sub(1))); }
                4 => { mon.reset(); }
                _ => unreachable!(),
            }

            let snap = mon.telemetry().snapshot();
            prop_assert!(snap.rules_registered >= prev.rules_registered,
                "rules_registered decreased");
            prop_assert!(snap.rules_unregistered >= prev.rules_unregistered,
                "rules_unregistered decreased");
            prop_assert!(snap.observations >= prev.observations,
                "observations decreased");
            prop_assert!(snap.drifts_detected >= prev.drifts_detected,
                "drifts_detected decreased");
            prop_assert!(snap.rate_drops >= prev.rate_drops,
                "rate_drops decreased");
            prop_assert!(snap.rate_spikes >= prev.rate_spikes,
                "rate_spikes decreased");
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        rules_registered in 0u64..10000,
        rules_unregistered in 0u64..5000,
        observations in 0u64..100000,
        drifts_detected in 0u64..10000,
        rate_drops in 0u64..5000,
        rate_spikes in 0u64..5000,
        resets in 0u64..100,
    ) {
        let snap = DriftTelemetrySnapshot {
            rules_registered,
            rules_unregistered,
            observations,
            drifts_detected,
            rate_drops,
            rate_spikes,
            resets,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: DriftTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
