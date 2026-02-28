//! Property-based tests for VOI scheduler telemetry counters (ft-3kxe.36).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. panes_registered tracks register_pane() calls (deduped)
//! 3. panes_unregistered tracks unregister_pane() calls
//! 4. belief_updates tracks update_belief() calls
//! 5. drift_applications tracks apply_drift() calls
//! 6. backpressure_changes tracks actual tier changes
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::bayesian_ledger::PaneState;
use frankenterm_core::voi::{BackpressureTierInput, VoiConfig, VoiScheduler, VoiTelemetrySnapshot};

// =============================================================================
// Helpers
// =============================================================================

fn test_scheduler() -> VoiScheduler {
    VoiScheduler::new(VoiConfig::default())
}

fn uniform_likelihoods() -> [f64; PaneState::COUNT] {
    [0.0; PaneState::COUNT]
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let sched = test_scheduler();
    let snap = sched.telemetry().snapshot();

    assert_eq!(snap.panes_registered, 0);
    assert_eq!(snap.panes_unregistered, 0);
    assert_eq!(snap.belief_updates, 0);
    assert_eq!(snap.drift_applications, 0);
    assert_eq!(snap.schedules_computed, 0);
    assert_eq!(snap.backpressure_changes, 0);
}

#[test]
fn panes_registered_tracked() {
    let mut sched = test_scheduler();
    sched.register_pane(1, 0);
    sched.register_pane(2, 0);
    sched.register_pane(3, 0);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 3);
}

#[test]
fn duplicate_registration_not_counted() {
    let mut sched = test_scheduler();
    sched.register_pane(1, 0);
    sched.register_pane(1, 100);
    sched.register_pane(1, 200);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 1);
}

#[test]
fn panes_unregistered_tracked() {
    let mut sched = test_scheduler();
    sched.register_pane(1, 0);
    sched.register_pane(2, 0);
    sched.unregister_pane(1);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_unregistered, 1);
}

#[test]
fn unregister_nonexistent_not_counted() {
    let mut sched = test_scheduler();
    sched.unregister_pane(999);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.panes_unregistered, 0);
}

#[test]
fn belief_updates_tracked() {
    let mut sched = test_scheduler();
    sched.register_pane(1, 0);
    sched.update_belief(1, &uniform_likelihoods(), 100);
    sched.update_belief(1, &uniform_likelihoods(), 200);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.belief_updates, 2);
}

#[test]
fn belief_update_on_unknown_pane_not_counted() {
    let mut sched = test_scheduler();
    sched.update_belief(999, &uniform_likelihoods(), 100);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.belief_updates, 0);
}

#[test]
fn drift_applications_tracked() {
    let mut sched = test_scheduler();
    sched.apply_drift(100);
    sched.apply_drift(200);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.drift_applications, 2);
}

#[test]
fn backpressure_changes_tracked() {
    let mut sched = test_scheduler();
    // Initial is Green, set to Yellow = change
    sched.set_backpressure(BackpressureTierInput::Yellow);
    // Yellow → Red = change
    sched.set_backpressure(BackpressureTierInput::Red);
    // Red → Red = no change
    sched.set_backpressure(BackpressureTierInput::Red);

    let snap = sched.telemetry().snapshot();
    assert_eq!(snap.backpressure_changes, 2);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = VoiTelemetrySnapshot {
        panes_registered: 100,
        panes_unregistered: 30,
        belief_updates: 50000,
        drift_applications: 1000,
        schedules_computed: 5000,
        backpressure_changes: 15,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: VoiTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn panes_registered_equals_unique_ids(
        ids in prop::collection::vec(0u64..20, 1..15),
    ) {
        let mut sched = test_scheduler();
        for &id in &ids {
            sched.register_pane(id, 0);
        }
        let snap = sched.telemetry().snapshot();
        let unique_count = ids.iter().collect::<std::collections::HashSet<_>>().len();
        prop_assert_eq!(snap.panes_registered, unique_count as u64);
    }

    #[test]
    fn belief_updates_equals_valid_calls(
        n_panes in 1usize..5,
        n_updates in 0usize..10,
    ) {
        let mut sched = test_scheduler();
        for i in 0..n_panes {
            sched.register_pane(i as u64, 0);
        }
        let mut total = 0u64;
        for i in 0..n_updates {
            let pane = (i % n_panes) as u64;
            sched.update_belief(pane, &uniform_likelihoods(), (i * 100) as u64);
            total += 1;
        }
        let snap = sched.telemetry().snapshot();
        prop_assert_eq!(snap.belief_updates, total);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..6, 1..30),
    ) {
        let mut sched = test_scheduler();
        let mut prev = sched.telemetry().snapshot();
        let mut next_id = 0u64;
        let mut time_ms = 0u64;

        for op in &ops {
            match op {
                0 => { sched.register_pane(next_id, time_ms); next_id += 1; }
                1 => { sched.unregister_pane(next_id.saturating_sub(1)); }
                2 => {
                    if next_id > 0 {
                        sched.update_belief(0, &uniform_likelihoods(), time_ms);
                    }
                }
                3 => { sched.apply_drift(time_ms); }
                4 => { sched.set_backpressure(BackpressureTierInput::Yellow); }
                5 => { sched.set_backpressure(BackpressureTierInput::Green); }
                _ => unreachable!(),
            }
            time_ms += 50;

            let snap = sched.telemetry().snapshot();
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased");
            prop_assert!(snap.panes_unregistered >= prev.panes_unregistered,
                "panes_unregistered decreased");
            prop_assert!(snap.belief_updates >= prev.belief_updates,
                "belief_updates decreased");
            prop_assert!(snap.drift_applications >= prev.drift_applications,
                "drift_applications decreased");
            prop_assert!(snap.backpressure_changes >= prev.backpressure_changes,
                "backpressure_changes decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        panes_registered in 0u64..10000,
        panes_unregistered in 0u64..5000,
        belief_updates in 0u64..100000,
        drift_applications in 0u64..50000,
        schedules_computed in 0u64..100000,
        backpressure_changes in 0u64..1000,
    ) {
        let snap = VoiTelemetrySnapshot {
            panes_registered,
            panes_unregistered,
            belief_updates,
            drift_applications,
            schedules_computed,
            backpressure_changes,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: VoiTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
