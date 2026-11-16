//! Property-based tests for BOCPD manager telemetry counters (ft-3kxe.33).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. panes_registered tracks register_pane() calls
//! 3. panes_unregistered tracks unregister_pane() calls
//! 4. observations tracks observe/observe_text_chunk calls
//! 5. change_points_detected bounded by observations
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;
use std::time::Duration;

use frankenterm_core::bocpd::{BocpdConfig, BocpdManager, BocpdTelemetrySnapshot, OutputFeatures};

// =============================================================================
// Helpers
// =============================================================================

fn test_manager() -> BocpdManager {
    BocpdManager::new(BocpdConfig::default())
}

fn normal_features() -> OutputFeatures {
    OutputFeatures::compute("hello world output", Duration::from_millis(100))
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let mgr = test_manager();
    let snap = mgr.telemetry().snapshot();

    assert_eq!(snap.panes_registered, 0);
    assert_eq!(snap.panes_unregistered, 0);
    assert_eq!(snap.observations, 0);
    assert_eq!(snap.change_points_detected, 0);
}

#[test]
fn panes_registered_tracked() {
    let mut mgr = test_manager();
    mgr.register_pane(1);
    mgr.register_pane(2);
    mgr.register_pane(3);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 3);
}

#[test]
fn panes_unregistered_tracked() {
    let mut mgr = test_manager();
    mgr.register_pane(1);
    mgr.register_pane(2);
    mgr.unregister_pane(1);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_unregistered, 1);
}

#[test]
fn observations_tracked() {
    let mut mgr = test_manager();
    mgr.register_pane(1);
    mgr.observe(1, normal_features());
    mgr.observe(1, normal_features());

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.observations, 2);
}

#[test]
fn text_chunk_observations_tracked() {
    let mut mgr = test_manager();
    mgr.observe_text_chunk(1, "test output", Duration::from_millis(50));
    mgr.observe_text_chunk(1, "more output", Duration::from_millis(50));
    mgr.observe_text_chunk(2, "other pane", Duration::from_millis(50));

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
}

#[test]
fn change_points_bounded_by_observations() {
    let mut mgr = test_manager();
    for _ in 0..10 {
        mgr.observe(1, normal_features());
    }

    let snap = mgr.telemetry().snapshot();
    assert!(snap.change_points_detected <= snap.observations);
}

#[test]
fn mixed_operations() {
    let mut mgr = test_manager();
    mgr.register_pane(1);
    mgr.register_pane(2);
    mgr.observe(1, normal_features());
    mgr.observe_text_chunk(2, "hello", Duration::from_millis(100));
    mgr.unregister_pane(1);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 2);
    assert_eq!(snap.panes_unregistered, 1);
    assert_eq!(snap.observations, 2);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = BocpdTelemetrySnapshot {
        panes_registered: 100,
        panes_unregistered: 30,
        observations: 5000,
        change_points_detected: 50,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: BocpdTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn panes_registered_equals_call_count(
        count in 1usize..20,
    ) {
        let mut mgr = test_manager();
        for i in 0..count {
            mgr.register_pane(i as u64);
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert_eq!(snap.panes_registered, count as u64);
    }

    #[test]
    fn observations_equals_call_count(
        n_observe in 0usize..10,
        n_text in 0usize..10,
    ) {
        let mut mgr = test_manager();
        mgr.register_pane(1);
        for _ in 0..n_observe {
            mgr.observe(1, normal_features());
        }
        for _ in 0..n_text {
            mgr.observe_text_chunk(1, "chunk", Duration::from_millis(50));
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert_eq!(snap.observations, (n_observe + n_text) as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..20),
    ) {
        let mut mgr = test_manager();
        let mut prev = mgr.telemetry().snapshot();
        let mut next_id = 0u64;

        for op in &ops {
            match op {
                0 => { mgr.register_pane(next_id); next_id += 1; }
                1 => { mgr.unregister_pane(next_id.saturating_sub(1)); }
                2 => { mgr.observe(0, normal_features()); }
                3 => { mgr.observe_text_chunk(0, "test", Duration::from_millis(50)); }
                4 => { mgr.register_pane(next_id); next_id += 1; }
                _ => unreachable!(),
            }

            let snap = mgr.telemetry().snapshot();
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased");
            prop_assert!(snap.panes_unregistered >= prev.panes_unregistered,
                "panes_unregistered decreased");
            prop_assert!(snap.observations >= prev.observations,
                "observations decreased");
            prop_assert!(snap.change_points_detected >= prev.change_points_detected,
                "change_points_detected decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        panes_registered in 0u64..100000,
        panes_unregistered in 0u64..50000,
        observations in 0u64..100000,
        change_points_detected in 0u64..10000,
    ) {
        let snap = BocpdTelemetrySnapshot {
            panes_registered,
            panes_unregistered,
            observations,
            change_points_detected,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: BocpdTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
