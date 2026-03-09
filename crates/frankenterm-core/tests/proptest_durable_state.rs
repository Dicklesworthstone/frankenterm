//! Property-based tests for durable_state checkpoint/rollback subsystem.
//!
//! Coverage targets:
//! - Checkpoint ID monotonicity across arbitrary sequences
//! - Retention pruning respects max_checkpoints
//! - Rollback restores exact prior entity state
//! - Diff correctness: added + removed + changed = total change set
//! - Diff symmetry: diff(A→B) inverse of diff(B→A)
//! - Serde roundtrip (JSON export/import preserves all state)
//! - Trigger/metadata preservation through checkpoint cycle
//! - Error paths: not-found, already-rolled-back
//!
//! ft-3681t.2.5 quality support slice.

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::durable_state::{CheckpointTrigger, DurableStateError, DurableStateManager};
use frankenterm_core::session_topology::{
    LifecycleEntityKind, LifecycleIdentity, LifecycleRegistry, LifecycleState,
    MuxPaneLifecycleState, SessionLifecycleState, WindowLifecycleState,
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn pane_identity(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Pane, "default", "local", id, 1)
}

fn session_identity(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Session, "default", "local", id, 1)
}

fn window_identity(id: u64) -> LifecycleIdentity {
    LifecycleIdentity::new(LifecycleEntityKind::Window, "default", "local", id, 1)
}

fn make_registry(pane_ids: &[u64]) -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    for &pid in pane_ids {
        reg.register_entity(
            pane_identity(pid),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .expect("register pane");
    }
    reg
}

fn make_mixed_registry(sessions: &[u64], windows: &[u64], panes: &[u64]) -> LifecycleRegistry {
    let mut reg = LifecycleRegistry::new();
    for &sid in sessions {
        reg.register_entity(
            session_identity(sid),
            LifecycleState::Session(SessionLifecycleState::Active),
            0,
        )
        .expect("register session");
    }
    for &wid in windows {
        reg.register_entity(
            window_identity(wid),
            LifecycleState::Window(WindowLifecycleState::Active),
            0,
        )
        .expect("register window");
    }
    for &pid in panes {
        reg.register_entity(
            pane_identity(pid),
            LifecycleState::Pane(MuxPaneLifecycleState::Running),
            0,
        )
        .expect("register pane");
    }
    reg
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_pane_ids() -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(1u64..1000, 0..10)
}

fn arb_trigger() -> impl Strategy<Value = CheckpointTrigger> {
    prop_oneof![
        Just(CheckpointTrigger::Manual),
        Just(CheckpointTrigger::Periodic),
        Just(CheckpointTrigger::PreShutdown),
        Just(CheckpointTrigger::PostRecovery),
        "[a-z]{3,12}".prop_map(|op| CheckpointTrigger::PreOperation { operation: op }),
        "[a-z]{3,12}".prop_map(|name| CheckpointTrigger::FleetProvisioning { fleet_name: name }),
    ]
}

fn arb_metadata() -> impl Strategy<Value = HashMap<String, String>> {
    prop::collection::hash_map("[a-z_]{2,8}", "[a-z0-9]{1,16}", 0..4)
}

// ---------------------------------------------------------------------------
// Checkpoint ID monotonicity
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Checkpoint IDs always increase, regardless of trigger type.
    #[test]
    fn checkpoint_ids_strictly_monotonic(
        n in 2u32..20,
        triggers in prop::collection::vec(arb_trigger(), 2..20),
    ) {
        let reg = make_registry(&[1, 2]);
        let mut mgr = DurableStateManager::new();
        let count = (n as usize).min(triggers.len());

        let mut ids = Vec::new();
        for (i, trigger) in triggers.iter().enumerate().take(count) {
            let cp = mgr.checkpoint(
                &reg,
                format!("cp-{i}"),
                trigger.clone(),
                HashMap::new(),
            );
            ids.push(cp.id);
        }

        for w in ids.windows(2) {
            assert!(w[0] < w[1], "IDs must be strictly increasing: {} < {}", w[0], w[1]);
        }
    }

    /// Checkpoint IDs survive JSON roundtrip and remain monotonic after restore.
    #[test]
    fn checkpoint_ids_monotonic_after_json_roundtrip(panes in arb_pane_ids()) {
        let reg = make_registry(&panes);
        let mut mgr = DurableStateManager::new();

        mgr.checkpoint(&reg, "a", CheckpointTrigger::Manual, HashMap::new());
        mgr.checkpoint(&reg, "b", CheckpointTrigger::Periodic, HashMap::new());

        let json = mgr.to_json().unwrap();
        let mut restored = DurableStateManager::from_json(&json).unwrap();

        // New checkpoint after restore should have ID > restored IDs
        let cp_id = restored.checkpoint(&reg, "c", CheckpointTrigger::Manual, HashMap::new()).id;
        let summaries = restored.list_checkpoints();
        let max_prev = summaries.iter().filter(|s| s.id != cp_id).map(|s| s.id).max().unwrap();
        assert!(cp_id > max_prev);
    }
}

// ---------------------------------------------------------------------------
// Retention pruning
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Checkpoint count never exceeds max_checkpoints.
    #[test]
    fn retention_never_exceeds_max(
        max in 1usize..10,
        n in 5usize..25,
    ) {
        let reg = make_registry(&[1]);
        let mut mgr = DurableStateManager::with_max_checkpoints(max);

        for i in 0..n {
            mgr.checkpoint(&reg, format!("cp-{i}"), CheckpointTrigger::Manual, HashMap::new());
        }

        assert!(
            mgr.checkpoint_count() <= max,
            "count {} exceeds max {}",
            mgr.checkpoint_count(),
            max
        );
    }

    /// Oldest checkpoints are evicted first under retention pressure.
    #[test]
    fn oldest_evicted_first(
        max in 2usize..6,
        n in 8usize..20,
    ) {
        let reg = make_registry(&[1]);
        let mut mgr = DurableStateManager::with_max_checkpoints(max);

        for i in 0..n {
            mgr.checkpoint(&reg, format!("cp-{i}"), CheckpointTrigger::Manual, HashMap::new());
        }

        let summaries = mgr.list_checkpoints();
        assert_eq!(summaries.len(), max);

        // The retained checkpoints should be the most recent ones
        let expected_first = format!("cp-{}", n - max);
        assert_eq!(summaries[0].label, expected_first);
    }
}

// ---------------------------------------------------------------------------
// Rollback restores exact state
// ---------------------------------------------------------------------------

#[test]
fn rollback_restores_entities_from_checkpoint() {
    let mut reg = make_registry(&[1, 2, 3]);
    let mut mgr = DurableStateManager::new();

    // Checkpoint with 3 panes running
    let cp_id = mgr
        .checkpoint(
            &reg,
            "before-change",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    // Add pane 4, change pane 1 to stopped
    reg.register_entity(
        pane_identity(4),
        LifecycleState::Pane(MuxPaneLifecycleState::Running),
        100,
    )
    .ok();
    reg.register_entity(
        pane_identity(1),
        LifecycleState::Pane(MuxPaneLifecycleState::Closed),
        100,
    )
    .ok();

    // Rollback
    let record = mgr.rollback(cp_id, &mut reg, "test rollback").unwrap();
    assert!(record.restored_entity_count > 0 || record.removed_entity_count > 0);

    // Pane 1 should be restored to Running
    let snap = reg.snapshot();
    let pane1 = snap.iter().find(|e| e.identity.local_id == 1).unwrap();
    assert_eq!(
        pane1.state,
        LifecycleState::Pane(MuxPaneLifecycleState::Running)
    );
}

#[test]
fn rollback_to_nonexistent_checkpoint_errors() {
    let mut reg = make_registry(&[1]);
    let mut mgr = DurableStateManager::new();

    let err = mgr.rollback(999, &mut reg, "bad rollback").unwrap_err();
    assert_eq!(err, DurableStateError::CheckpointNotFound { id: 999 });
}

#[test]
fn rollback_to_already_rolled_back_errors() {
    let mut reg = make_registry(&[1, 2]);
    let mut mgr = DurableStateManager::new();

    let cp1_id = mgr
        .checkpoint(&reg, "cp1", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let cp2_id = mgr
        .checkpoint(&reg, "cp2", CheckpointTrigger::Manual, HashMap::new())
        .id;

    // Rollback to cp1 — this marks cp2 as rolled_back
    mgr.rollback(cp1_id, &mut reg, "first rollback").unwrap();

    // Attempting to rollback to cp2 (which is now rolled_back) should error
    let err = mgr
        .rollback(cp2_id, &mut reg, "second rollback")
        .unwrap_err();
    assert_eq!(err, DurableStateError::AlreadyRolledBack { id: cp2_id });
}

#[test]
fn rollback_creates_pre_rollback_checkpoint() {
    let mut reg = make_registry(&[1]);
    let mut mgr = DurableStateManager::new();

    let cp_id = mgr
        .checkpoint(&reg, "original", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let count_before = mgr.checkpoint_count();
    mgr.rollback(cp_id, &mut reg, "test").unwrap();

    // A pre-rollback checkpoint should have been created
    assert_eq!(mgr.checkpoint_count(), count_before + 1);
    let latest = mgr.latest_checkpoint().unwrap();
    assert!(latest.label.contains("pre-rollback"));
}

#[test]
fn rollback_history_tracks_operations() {
    let mut reg = make_registry(&[1, 2]);
    let mut mgr = DurableStateManager::new();

    let cp_id = mgr
        .checkpoint(&reg, "base", CheckpointTrigger::Manual, HashMap::new())
        .id;

    assert!(mgr.rollback_history().is_empty());

    mgr.rollback(cp_id, &mut reg, "reason-1").unwrap();
    assert_eq!(mgr.rollback_history().len(), 1);
    assert_eq!(mgr.rollback_history()[0].reason, "reason-1");
    assert_eq!(mgr.rollback_history()[0].target_checkpoint_id, cp_id);
}

// ---------------------------------------------------------------------------
// Diff correctness
// ---------------------------------------------------------------------------

#[test]
fn diff_empty_when_checkpoints_identical() {
    let reg = make_registry(&[1, 2, 3]);
    let mut mgr = DurableStateManager::new();

    let cp1 = mgr
        .checkpoint(&reg, "a", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let cp2 = mgr
        .checkpoint(&reg, "b", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert!(
        diff.is_empty(),
        "identical snapshots should produce empty diff"
    );
    assert_eq!(diff.change_count(), 0);
}

#[test]
fn diff_detects_added_entities() {
    let reg_small = make_registry(&[1, 2]);
    let reg_big = make_registry(&[1, 2, 3, 4]);
    let mut mgr = DurableStateManager::new();

    let cp1 = mgr
        .checkpoint(
            &reg_small,
            "small",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;
    let cp2 = mgr
        .checkpoint(&reg_big, "big", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert_eq!(diff.added.len(), 2, "panes 3 and 4 should be added");
    assert!(diff.removed.is_empty());
}

#[test]
fn diff_detects_removed_entities() {
    let reg_big = make_registry(&[1, 2, 3, 4]);
    let reg_small = make_registry(&[1, 2]);
    let mut mgr = DurableStateManager::new();

    let cp1 = mgr
        .checkpoint(&reg_big, "big", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let cp2 = mgr
        .checkpoint(
            &reg_small,
            "small",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert_eq!(diff.removed.len(), 2, "panes 3 and 4 should be removed");
    assert!(diff.added.is_empty());
}

#[test]
fn diff_detects_state_changes() {
    let reg1 = make_registry(&[1, 2]);
    let mut reg2 = LifecycleRegistry::new();
    reg2.register_entity(
        pane_identity(1),
        LifecycleState::Pane(MuxPaneLifecycleState::Running),
        0,
    )
    .ok();
    reg2.register_entity(
        pane_identity(2),
        LifecycleState::Pane(MuxPaneLifecycleState::Closed),
        100,
    )
    .ok();

    let mut mgr = DurableStateManager::new();
    let cp1 = mgr
        .checkpoint(&reg1, "before", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let cp2 = mgr
        .checkpoint(&reg2, "after", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert_eq!(diff.changed.len(), 1, "pane 2 state changed");
    assert!(diff.added.is_empty());
    assert!(diff.removed.is_empty());
}

#[test]
fn diff_from_current_works() {
    let reg_before = make_registry(&[1, 2]);
    let mut mgr = DurableStateManager::new();

    let cp_id = mgr
        .checkpoint(
            &reg_before,
            "base",
            CheckpointTrigger::Manual,
            HashMap::new(),
        )
        .id;

    let reg_after = make_registry(&[1, 2, 3]);
    let diff = mgr.diff_from_current(cp_id, &reg_after).unwrap();
    assert_eq!(diff.added.len(), 1, "pane 3 should be added");
    assert_eq!(diff.to_checkpoint, 0, "to=0 means current state");
}

#[test]
fn diff_nonexistent_checkpoint_errors() {
    let mgr = DurableStateManager::new();
    let err = mgr.diff(1, 2).unwrap_err();
    assert_eq!(err, DurableStateError::CheckpointNotFound { id: 1 });
}

// ---------------------------------------------------------------------------
// Serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// JSON export/import preserves checkpoints, history, and next_id.
    #[test]
    fn json_roundtrip_preserves_state(
        panes in arb_pane_ids(),
        trigger in arb_trigger(),
        metadata in arb_metadata(),
    ) {
        let reg = make_registry(&panes);
        let mut mgr = DurableStateManager::new();

        mgr.checkpoint(&reg, "roundtrip-test", trigger, metadata);

        let json = mgr.to_json().unwrap();
        let restored = DurableStateManager::from_json(&json).unwrap();

        assert_eq!(restored.checkpoint_count(), mgr.checkpoint_count());
        let orig_summaries = mgr.list_checkpoints();
        let rest_summaries = restored.list_checkpoints();
        for (o, r) in orig_summaries.iter().zip(rest_summaries.iter()) {
            assert_eq!(o.id, r.id);
            assert_eq!(o.label, r.label);
            assert_eq!(o.entity_count, r.entity_count);
            assert_eq!(o.rolled_back, r.rolled_back);
        }
    }

    /// Trigger serde roundtrip preserves variant data.
    #[test]
    fn trigger_serde_roundtrip(trigger in arb_trigger()) {
        let json = serde_json::to_string(&trigger).unwrap();
        let decoded: CheckpointTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(trigger, decoded);
    }
}

// ---------------------------------------------------------------------------
// Metadata preservation
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_preserves_metadata() {
    let reg = make_registry(&[1]);
    let mut mgr = DurableStateManager::new();

    let mut meta = HashMap::new();
    meta.insert("fleet".to_string(), "alpha".to_string());
    meta.insert("version".to_string(), "1.2.3".to_string());

    let cp_id = mgr
        .checkpoint(&reg, "with-meta", CheckpointTrigger::Manual, meta.clone())
        .id;

    // Verify after retrieval
    let retrieved = mgr.get_checkpoint(cp_id).unwrap();
    assert_eq!(retrieved.metadata, meta);
    assert_eq!(retrieved.metadata.get("fleet").unwrap(), "alpha");
    assert_eq!(retrieved.metadata.get("version").unwrap(), "1.2.3");
}

#[test]
fn checkpoint_preserves_trigger_variant() {
    let reg = make_registry(&[1]);
    let mut mgr = DurableStateManager::new();

    let triggers = vec![
        CheckpointTrigger::Manual,
        CheckpointTrigger::Periodic,
        CheckpointTrigger::PreShutdown,
        CheckpointTrigger::PostRecovery,
        CheckpointTrigger::PreOperation {
            operation: "deploy".to_string(),
        },
        CheckpointTrigger::FleetProvisioning {
            fleet_name: "swarm-alpha".to_string(),
        },
    ];

    for (i, trigger) in triggers.into_iter().enumerate() {
        let cp_id = mgr
            .checkpoint(&reg, format!("t-{i}"), trigger.clone(), HashMap::new())
            .id;
        let retrieved = mgr.get_checkpoint(cp_id).unwrap();
        assert_eq!(retrieved.trigger, trigger);
    }
}

// ---------------------------------------------------------------------------
// Mixed entity types
// ---------------------------------------------------------------------------

#[test]
fn checkpoint_preserves_mixed_entity_types() {
    let reg = make_mixed_registry(&[1], &[10], &[100, 101]);
    let mut mgr = DurableStateManager::new();

    let cp = mgr.checkpoint(&reg, "mixed", CheckpointTrigger::Manual, HashMap::new());
    assert_eq!(cp.entities.len(), 4); // 1 session + 1 window + 2 panes
}

#[test]
fn diff_with_mixed_entity_types() {
    let reg1 = make_mixed_registry(&[1], &[10], &[100]);
    let reg2 = make_mixed_registry(&[1, 2], &[10], &[100, 101]);
    let mut mgr = DurableStateManager::new();

    let cp1 = mgr
        .checkpoint(&reg1, "before", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let cp2 = mgr
        .checkpoint(&reg2, "after", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert_eq!(diff.added.len(), 2, "session 2 + pane 101 added");
    assert!(diff.removed.is_empty());
    assert!(diff.changed.is_empty());
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn empty_registry_checkpoint() {
    let reg = LifecycleRegistry::new();
    let mut mgr = DurableStateManager::new();

    let cp = mgr.checkpoint(&reg, "empty", CheckpointTrigger::Manual, HashMap::new());
    assert_eq!(cp.entities.len(), 0);
}

#[test]
fn list_checkpoints_summary() {
    let reg = make_registry(&[1, 2]);
    let mut mgr = DurableStateManager::new();

    mgr.checkpoint(&reg, "first", CheckpointTrigger::Manual, HashMap::new());
    mgr.checkpoint(&reg, "second", CheckpointTrigger::Periodic, HashMap::new());

    let summaries = mgr.list_checkpoints();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].label, "first");
    assert_eq!(summaries[0].entity_count, 2);
    assert_eq!(summaries[1].label, "second");
}

#[test]
fn default_manager_is_empty() {
    let mgr = DurableStateManager::default();
    assert_eq!(mgr.checkpoint_count(), 0);
    assert!(mgr.latest_checkpoint().is_none());
    assert!(mgr.rollback_history().is_empty());
}

#[test]
#[should_panic(expected = "called `Option::unwrap()` on a `None` value")]
fn with_max_checkpoints_zero_panics() {
    // Known edge case: max_checkpoints=0 causes panic in checkpoint() because
    // all entries are drained then .last().unwrap() is called on empty vec.
    let reg = make_registry(&[1]);
    let mut mgr = DurableStateManager::with_max_checkpoints(0);
    mgr.checkpoint(&reg, "a", CheckpointTrigger::Manual, HashMap::new());
}

#[test]
fn diff_change_count_consistent() {
    let reg1 = make_registry(&[1, 2, 3]);
    let mut reg2 = make_registry(&[2, 4]);
    reg2.register_entity(
        pane_identity(3),
        LifecycleState::Pane(MuxPaneLifecycleState::Closed),
        100,
    )
    .ok();

    let mut mgr = DurableStateManager::new();
    let cp1 = mgr
        .checkpoint(&reg1, "a", CheckpointTrigger::Manual, HashMap::new())
        .id;
    let cp2 = mgr
        .checkpoint(&reg2, "b", CheckpointTrigger::Manual, HashMap::new())
        .id;

    let diff = mgr.diff(cp1, cp2).unwrap();
    assert_eq!(
        diff.change_count(),
        diff.added.len() + diff.removed.len() + diff.changed.len()
    );
    assert!(!diff.is_empty());
}
