//! Property-based tests for backpressure manager telemetry counters (ft-3kxe.30).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. evaluations tracks evaluate() calls
//! 3. classifications tracks classify() calls (including from evaluate)
//! 4. transitions tracks tier changes
//! 5. panes_paused tracks pause_pane() calls
//! 6. panes_resumed tracks resume_pane() calls
//! 7. resume_alls tracks resume_all_panes() calls
//! 8. Serde roundtrip for snapshot
//! 9. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::backpressure::{
    BackpressureConfig, BackpressureManager, BackpressureTelemetrySnapshot, QueueDepths,
};

// =============================================================================
// Helpers
// =============================================================================

fn default_manager() -> BackpressureManager {
    BackpressureManager::new(BackpressureConfig {
        hysteresis_ms: 0, // disable hysteresis for deterministic tests
        ..BackpressureConfig::default()
    })
}

fn green_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 10,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 1000,
    }
}

fn red_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 900,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 1000,
    }
}

fn black_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 998,
        capture_capacity: 1000,
        write_depth: 10,
        write_capacity: 1000,
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let mgr = default_manager();
    let snap = mgr.telemetry().snapshot();

    assert_eq!(snap.evaluations, 0);
    assert_eq!(snap.classifications, 0);
    assert_eq!(snap.transitions, 0);
    assert_eq!(snap.panes_paused, 0);
    assert_eq!(snap.panes_resumed, 0);
    assert_eq!(snap.resume_alls, 0);
}

#[test]
fn evaluations_tracked() {
    let mgr = default_manager();
    mgr.evaluate(&green_depths());
    mgr.evaluate(&green_depths());
    mgr.evaluate(&green_depths());

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.evaluations, 3);
}

#[test]
fn classifications_tracked() {
    let mgr = default_manager();
    mgr.classify(&green_depths());
    mgr.classify(&red_depths());

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.classifications, 2);
}

#[test]
fn evaluate_also_counts_classification() {
    let mgr = default_manager();
    mgr.evaluate(&green_depths());

    let snap = mgr.telemetry().snapshot();
    // evaluate() calls classify() internally
    assert_eq!(snap.evaluations, 1);
    assert!(snap.classifications >= 1);
}

#[test]
fn transitions_tracked() {
    let mgr = default_manager();
    // Start at Green, move to Red
    let result = mgr.evaluate(&red_depths());
    assert!(result.is_some());

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.transitions, 1);

    // No transition if same tier
    let result = mgr.evaluate(&red_depths());
    assert!(result.is_none());

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.transitions, 1); // still 1
}

#[test]
fn panes_paused_tracked() {
    let mgr = default_manager();
    mgr.pause_pane(1);
    mgr.pause_pane(2);
    mgr.pause_pane(3);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_paused, 3);
}

#[test]
fn panes_resumed_tracked() {
    let mgr = default_manager();
    mgr.pause_pane(1);
    mgr.pause_pane(2);
    mgr.resume_pane(1);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_paused, 2);
    assert_eq!(snap.panes_resumed, 1);
}

#[test]
fn resume_alls_tracked() {
    let mgr = default_manager();
    mgr.pause_pane(1);
    mgr.pause_pane(2);
    mgr.resume_all_panes();
    mgr.resume_all_panes();

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.resume_alls, 2);
}

#[test]
fn mixed_operations() {
    let mgr = default_manager();
    mgr.evaluate(&red_depths()); // eval+classify+transition
    mgr.classify(&green_depths()); // classify only
    mgr.pause_pane(1);
    mgr.resume_pane(1);
    mgr.resume_all_panes();

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.evaluations, 1);
    assert!(snap.classifications >= 2); // at least 1 from evaluate + 1 direct
    assert_eq!(snap.transitions, 1);
    assert_eq!(snap.panes_paused, 1);
    assert_eq!(snap.panes_resumed, 1);
    assert_eq!(snap.resume_alls, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = BackpressureTelemetrySnapshot {
        evaluations: 5000,
        classifications: 6000,
        transitions: 100,
        panes_paused: 50,
        panes_resumed: 45,
        resume_alls: 10,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: BackpressureTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn evaluations_equals_call_count(
        count in 1usize..30,
    ) {
        let mgr = default_manager();
        for _ in 0..count {
            mgr.evaluate(&green_depths());
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert_eq!(snap.evaluations, count as u64);
    }

    #[test]
    fn classifications_ge_evaluations(
        n_eval in 0usize..10,
        n_classify in 0usize..10,
    ) {
        let mgr = default_manager();
        for _ in 0..n_eval {
            mgr.evaluate(&green_depths());
        }
        for _ in 0..n_classify {
            mgr.classify(&green_depths());
        }
        let snap = mgr.telemetry().snapshot();
        // Each evaluate calls classify internally
        prop_assert!(
            snap.classifications >= snap.evaluations,
            "classifications ({}) < evaluations ({})",
            snap.classifications, snap.evaluations,
        );
        prop_assert_eq!(snap.classifications, (n_eval + n_classify) as u64);
    }

    #[test]
    fn transitions_bounded_by_evaluations(
        count in 1usize..30,
    ) {
        let mgr = default_manager();
        for _ in 0..count {
            mgr.evaluate(&red_depths());
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert!(
            snap.transitions <= snap.evaluations,
            "transitions ({}) > evaluations ({})",
            snap.transitions, snap.evaluations,
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..6, 1..30),
    ) {
        let mgr = default_manager();
        let mut prev = mgr.telemetry().snapshot();

        let depths_list = [green_depths(), red_depths(), black_depths()];

        for op in &ops {
            match op {
                0 => { mgr.evaluate(&depths_list[0]); }
                1 => { mgr.evaluate(&depths_list[1]); }
                2 => { mgr.classify(&depths_list[2]); }
                3 => { mgr.pause_pane(1); }
                4 => { mgr.resume_pane(1); }
                5 => { mgr.resume_all_panes(); }
                _ => unreachable!(),
            }

            let snap = mgr.telemetry().snapshot();
            prop_assert!(snap.evaluations >= prev.evaluations,
                "evaluations decreased: {} -> {}",
                prev.evaluations, snap.evaluations);
            prop_assert!(snap.classifications >= prev.classifications,
                "classifications decreased: {} -> {}",
                prev.classifications, snap.classifications);
            prop_assert!(snap.transitions >= prev.transitions,
                "transitions decreased: {} -> {}",
                prev.transitions, snap.transitions);
            prop_assert!(snap.panes_paused >= prev.panes_paused,
                "panes_paused decreased: {} -> {}",
                prev.panes_paused, snap.panes_paused);
            prop_assert!(snap.panes_resumed >= prev.panes_resumed,
                "panes_resumed decreased: {} -> {}",
                prev.panes_resumed, snap.panes_resumed);
            prop_assert!(snap.resume_alls >= prev.resume_alls,
                "resume_alls decreased: {} -> {}",
                prev.resume_alls, snap.resume_alls);

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        evaluations in 0u64..100000,
        classifications in 0u64..100000,
        transitions in 0u64..50000,
        panes_paused in 0u64..50000,
        panes_resumed in 0u64..50000,
        resume_alls in 0u64..10000,
    ) {
        let snap = BackpressureTelemetrySnapshot {
            evaluations,
            classifications,
            transitions,
            panes_paused,
            panes_resumed,
            resume_alls,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: BackpressureTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
