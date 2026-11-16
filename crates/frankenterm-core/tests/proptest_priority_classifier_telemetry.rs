//! Property-based tests for priority classifier telemetry counters (ft-3kxe.28).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. panes_registered tracks register_pane() calls
//! 3. panes_unregistered tracks unregister_pane() calls
//! 4. classifications tracks classify/classify_all calls
//! 5. signals_observed tracks observe_signal() calls
//! 6. overrides_set/overrides_cleared track set_override/clear_override calls
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;
use std::time::Instant;

use frankenterm_core::priority::{
    PanePriority, PriorityClassifier, PriorityClassifierTelemetrySnapshot, PriorityConfig,
    PrioritySignal,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_classifier() -> PriorityClassifier {
    PriorityClassifier::new(PriorityConfig::default())
}

fn make_signal() -> PrioritySignal {
    PrioritySignal {
        event_type: "error".to_string(),
        severity: 2,
        observed_at: Instant::now(),
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let c = test_classifier();
    let snap = c.telemetry();

    assert_eq!(snap.panes_registered, 0);
    assert_eq!(snap.panes_unregistered, 0);
    assert_eq!(snap.classifications, 0);
    assert_eq!(snap.signals_observed, 0);
    assert_eq!(snap.overrides_set, 0);
    assert_eq!(snap.overrides_cleared, 0);
}

#[test]
fn panes_registered_tracked() {
    let c = test_classifier();
    c.register_pane(1);
    c.register_pane(2);
    c.register_pane(3);

    let snap = c.telemetry();
    assert_eq!(snap.panes_registered, 3);
}

#[test]
fn panes_unregistered_tracked() {
    let c = test_classifier();
    c.register_pane(1);
    c.register_pane(2);
    c.unregister_pane(1);

    let snap = c.telemetry();
    assert_eq!(snap.panes_unregistered, 1);
}

#[test]
fn unregister_missing_pane_still_counts() {
    let c = test_classifier();
    c.unregister_pane(999);

    let snap = c.telemetry();
    assert_eq!(snap.panes_unregistered, 1);
}

#[test]
fn classifications_tracked() {
    let c = test_classifier();
    c.register_pane(1);
    c.register_pane(2);

    c.classify(1);
    c.classify(2);
    c.classify(1);

    let snap = c.telemetry();
    assert_eq!(snap.classifications, 3);
}

#[test]
fn classify_all_counts_per_pane() {
    let c = test_classifier();
    c.register_pane(1);
    c.register_pane(2);
    c.register_pane(3);

    c.classify_all();

    let snap = c.telemetry();
    assert_eq!(snap.classifications, 3); // one per pane
}

#[test]
fn signals_observed_tracked() {
    let c = test_classifier();
    c.register_pane(1);

    c.observe_signal(1, &make_signal());
    c.observe_signal(1, &make_signal());

    let snap = c.telemetry();
    assert_eq!(snap.signals_observed, 2);
}

#[test]
fn overrides_tracked() {
    let c = test_classifier();
    c.register_pane(1);

    c.set_override(1, PanePriority::Critical);
    c.set_override(1, PanePriority::High);
    c.clear_override(1);

    let snap = c.telemetry();
    assert_eq!(snap.overrides_set, 2);
    assert_eq!(snap.overrides_cleared, 1);
}

#[test]
fn mixed_operations() {
    let c = test_classifier();
    c.register_pane(1);
    c.register_pane(2);
    c.observe_signal(1, &make_signal());
    c.classify(1);
    c.set_override(2, PanePriority::Low);
    c.unregister_pane(1);

    let snap = c.telemetry();
    assert_eq!(snap.panes_registered, 2);
    assert_eq!(snap.panes_unregistered, 1);
    assert_eq!(snap.classifications, 1);
    assert_eq!(snap.signals_observed, 1);
    assert_eq!(snap.overrides_set, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = PriorityClassifierTelemetrySnapshot {
        panes_registered: 100,
        panes_unregistered: 50,
        classifications: 5000,
        signals_observed: 200,
        overrides_set: 30,
        overrides_cleared: 20,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: PriorityClassifierTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn panes_registered_equals_call_count(
        count in 1usize..30,
    ) {
        let c = test_classifier();
        for i in 0..count {
            c.register_pane(i as u64);
        }
        let snap = c.telemetry();
        prop_assert_eq!(snap.panes_registered, count as u64);
    }

    #[test]
    fn classifications_equals_classify_calls(
        n_panes in 1usize..10,
        n_rounds in 1usize..5,
    ) {
        let c = test_classifier();
        for i in 0..n_panes {
            c.register_pane(i as u64);
        }
        for _ in 0..n_rounds {
            for i in 0..n_panes {
                c.classify(i as u64);
            }
        }
        let snap = c.telemetry();
        prop_assert_eq!(snap.classifications, (n_panes * n_rounds) as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..6, 1..30),
    ) {
        let c = test_classifier();
        let mut prev = c.telemetry();
        let mut next_id = 0u64;

        for op in &ops {
            match op {
                0 => { c.register_pane(next_id); next_id += 1; }
                1 => { c.unregister_pane(next_id.saturating_sub(1)); }
                2 => { c.classify(0); }
                3 => { c.observe_signal(0, &make_signal()); }
                4 => { c.set_override(0, PanePriority::High); }
                5 => { c.clear_override(0); }
                _ => unreachable!(),
            }

            let snap = c.telemetry();
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased: {} -> {}",
                prev.panes_registered, snap.panes_registered);
            prop_assert!(snap.panes_unregistered >= prev.panes_unregistered,
                "panes_unregistered decreased: {} -> {}",
                prev.panes_unregistered, snap.panes_unregistered);
            prop_assert!(snap.classifications >= prev.classifications,
                "classifications decreased: {} -> {}",
                prev.classifications, snap.classifications);
            prop_assert!(snap.signals_observed >= prev.signals_observed,
                "signals_observed decreased: {} -> {}",
                prev.signals_observed, snap.signals_observed);
            prop_assert!(snap.overrides_set >= prev.overrides_set,
                "overrides_set decreased: {} -> {}",
                prev.overrides_set, snap.overrides_set);
            prop_assert!(snap.overrides_cleared >= prev.overrides_cleared,
                "overrides_cleared decreased: {} -> {}",
                prev.overrides_cleared, snap.overrides_cleared);

            prev = snap;
        }
    }

    #[test]
    fn classify_all_counts_all_panes(
        n_panes in 1usize..15,
    ) {
        let c = test_classifier();
        for i in 0..n_panes {
            c.register_pane(i as u64);
        }
        c.classify_all();
        let snap = c.telemetry();
        prop_assert_eq!(snap.classifications, n_panes as u64);
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        panes_registered in 0u64..100000,
        panes_unregistered in 0u64..50000,
        classifications in 0u64..100000,
        signals_observed in 0u64..50000,
        overrides_set in 0u64..10000,
        overrides_cleared in 0u64..10000,
    ) {
        let snap = PriorityClassifierTelemetrySnapshot {
            panes_registered,
            panes_unregistered,
            classifications,
            signals_observed,
            overrides_set,
            overrides_cleared,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PriorityClassifierTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
