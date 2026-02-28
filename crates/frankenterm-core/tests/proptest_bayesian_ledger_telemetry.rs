//! Property-based tests for Bayesian ledger telemetry counters (ft-3kxe.40).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. updates tracks update() calls
//! 3. feedbacks tracks record_feedback() calls
//! 4. panes_reset tracks reset_pane() calls
//! 5. panes_removed tracks remove_pane() calls
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::bayesian_ledger::{
    BayesianClassifier, Evidence, LedgerConfig, LedgerTelemetrySnapshot, PaneState,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_classifier() -> BayesianClassifier {
    BayesianClassifier::new(LedgerConfig::default())
}

fn test_evidence() -> Evidence {
    Evidence::OutputRate(15.0)
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let cls = test_classifier();
    let snap = cls.telemetry().snapshot();

    assert_eq!(snap.updates, 0);
    assert_eq!(snap.feedbacks, 0);
    assert_eq!(snap.panes_reset, 0);
    assert_eq!(snap.panes_removed, 0);
}

#[test]
fn updates_tracked() {
    let mut cls = test_classifier();
    cls.update(1, test_evidence());
    cls.update(2, test_evidence());
    cls.update(1, Evidence::Entropy(5.0));

    let snap = cls.telemetry().snapshot();
    assert_eq!(snap.updates, 3);
}

#[test]
fn feedbacks_tracked() {
    let mut cls = test_classifier();
    cls.record_feedback(1, PaneState::Active);
    cls.record_feedback(1, PaneState::Idle);

    let snap = cls.telemetry().snapshot();
    assert_eq!(snap.feedbacks, 2);
}

#[test]
fn panes_reset_tracked() {
    let mut cls = test_classifier();
    cls.update(1, test_evidence());
    cls.reset_pane(1);
    cls.reset_pane(999); // resetting non-existent pane still counts

    let snap = cls.telemetry().snapshot();
    assert_eq!(snap.panes_reset, 2);
}

#[test]
fn panes_removed_tracked() {
    let mut cls = test_classifier();
    cls.update(1, test_evidence());
    cls.remove_pane(1);
    cls.remove_pane(999); // removing non-existent pane still counts

    let snap = cls.telemetry().snapshot();
    assert_eq!(snap.panes_removed, 2);
}

#[test]
fn mixed_operations() {
    let mut cls = test_classifier();
    cls.update(1, test_evidence());
    cls.update(2, test_evidence());
    cls.record_feedback(1, PaneState::Active);
    cls.reset_pane(1);
    cls.remove_pane(2);

    let snap = cls.telemetry().snapshot();
    assert_eq!(snap.updates, 2);
    assert_eq!(snap.feedbacks, 1);
    assert_eq!(snap.panes_reset, 1);
    assert_eq!(snap.panes_removed, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = LedgerTelemetrySnapshot {
        updates: 5000,
        feedbacks: 100,
        panes_reset: 50,
        panes_removed: 25,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: LedgerTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn updates_equal_call_count(
        count in 1usize..20,
    ) {
        let mut cls = test_classifier();
        for i in 0..count {
            cls.update(i as u64, test_evidence());
        }
        let snap = cls.telemetry().snapshot();
        prop_assert_eq!(snap.updates, count as u64);
    }

    #[test]
    fn feedbacks_equal_call_count(
        count in 1usize..20,
    ) {
        let mut cls = test_classifier();
        for _ in 0..count {
            cls.record_feedback(1, PaneState::Active);
        }
        let snap = cls.telemetry().snapshot();
        prop_assert_eq!(snap.feedbacks, count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..20),
    ) {
        let mut cls = test_classifier();
        let mut prev = cls.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { cls.update(i as u64, test_evidence()); }
                1 => { cls.record_feedback(1, PaneState::Idle); }
                2 => { cls.reset_pane(i as u64); }
                3 => { cls.remove_pane(i as u64); }
                _ => unreachable!(),
            }

            let snap = cls.telemetry().snapshot();
            prop_assert!(snap.updates >= prev.updates, "updates decreased");
            prop_assert!(snap.feedbacks >= prev.feedbacks, "feedbacks decreased");
            prop_assert!(snap.panes_reset >= prev.panes_reset, "panes_reset decreased");
            prop_assert!(snap.panes_removed >= prev.panes_removed, "panes_removed decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        updates in 0u64..100000,
        feedbacks in 0u64..10000,
        panes_reset in 0u64..1000,
        panes_removed in 0u64..1000,
    ) {
        let snap = LedgerTelemetrySnapshot {
            updates,
            feedbacks,
            panes_reset,
            panes_removed,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: LedgerTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
