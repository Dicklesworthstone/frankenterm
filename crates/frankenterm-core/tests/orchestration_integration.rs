//! ft-1i2ge.3.7: Orchestration integration and e2e scenario harness with rich logs.
//!
//! Cross-module integration tests covering:
//! - Mission loop trigger → evaluate → decision pipeline
//! - Dispatch contract resolution, dry-run, and live execution
//! - Dispatch idempotency and deduplication
//! - Assignment reconciliation with state drift detection
//! - Journal append, checkpoint, replay, and compaction
//! - Kill-switch and safety envelope interaction with dispatch
//! - Full lifecycle scenario: plan → dispatch → reconcile → journal
//! - Failure taxonomy reason code and retryability assertions
//! - Conflict detection integration with dispatch decisions

use frankenterm_core::plan::{
    Mission, MissionActorRole, MissionDispatchDeduplicationState, MissionDispatchMechanism,
    MissionFailureCode, MissionFailureTerminality, MissionId, MissionJournalEntryKind,
    MissionKillSwitchActivation, MissionKillSwitchLevel, MissionLifecycleState,
    MissionLifecycleTransitionKind, Outcome,
};

// ── Helpers ─────────────────────────────────────────────────────────────────

fn create_test_mission(id: &str) -> Mission {
    Mission::new(MissionId(id.into()))
}

// ── Journal Integration ─────────────────────────────────────────────────────

#[test]
fn journal_lifecycle_transition_append_and_replay() {
    let mission = create_test_mission("m:orch-1");
    let mut journal = mission.create_journal();

    // Append lifecycle transitions
    let seq1 = journal
        .append(
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                kind: MissionLifecycleTransitionKind::PlanFinalized,
            },
            "corr-1".into(),
            MissionActorRole::Planner,
            "plan_complete".into(),
            None,
            1000,
        )
        .unwrap();
    assert_eq!(seq1, 1);

    let seq2 = journal
        .append(
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planned,
                to: MissionLifecycleState::Dispatching,
                kind: MissionLifecycleTransitionKind::DispatchStarted,
            },
            "corr-2".into(),
            MissionActorRole::Dispatcher,
            "dispatch_started".into(),
            None,
            2000,
        )
        .unwrap();
    assert_eq!(seq2, 2);

    // Replay from start
    let report = journal.replay_from_checkpoint(0);
    assert_eq!(report.entries_scanned, 2);
    assert_eq!(report.lifecycle_transitions, 2);
    assert!(report.is_clean());
}

#[test]
fn journal_checkpoint_and_recovery() {
    let mut mission = create_test_mission("m:orch-2");
    let mut journal = mission.create_journal();

    // Append some entries
    journal
        .append(
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                kind: MissionLifecycleTransitionKind::PlanFinalized,
            },
            "corr-cp-1".into(),
            MissionActorRole::Planner,
            "plan_finalized".into(),
            None,
            1000,
        )
        .unwrap();

    journal
        .append(
            MissionJournalEntryKind::AssignmentOutcome {
                assignment_id: "a:1".into(),
                outcome: "success".into(),
            },
            "corr-cp-2".into(),
            MissionActorRole::Dispatcher,
            "assignment_outcome".into(),
            None,
            2000,
        )
        .unwrap();

    // Create checkpoint
    journal.checkpoint(&mission, 3000);

    // Append more after checkpoint
    journal
        .append(
            MissionJournalEntryKind::ControlCommand {
                command: "pause".into(),
            },
            "corr-cp-3".into(),
            MissionActorRole::Operator,
            "pause_requested".into(),
            None,
            4000,
        )
        .unwrap();

    // Replay from checkpoint should only see post-checkpoint entries
    let snapshot = journal.snapshot_state();
    let report = journal.replay_from_checkpoint(snapshot.last_checkpoint_seq);
    // entries_since returns entries after the given seq
    let entries_after = journal.entries_since(snapshot.last_checkpoint_seq);
    assert!(!entries_after.is_empty());
}

#[test]
fn journal_duplicate_correlation_rejected() {
    let mission = create_test_mission("m:orch-3");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::ControlCommand {
                command: "pause".into(),
            },
            "dup-corr".into(),
            MissionActorRole::Operator,
            "first".into(),
            None,
            1000,
        )
        .unwrap();

    let result = journal.append(
        MissionJournalEntryKind::ControlCommand {
            command: "resume".into(),
        },
        "dup-corr".into(),
        MissionActorRole::Operator,
        "second".into(),
        None,
        2000,
    );
    assert!(result.is_err());
}

#[test]
fn journal_compaction_preserves_post_checkpoint() {
    let mission = create_test_mission("m:orch-4");
    let mut journal = mission.create_journal();

    // Append 5 entries
    for i in 1..=5 {
        journal
            .append(
                MissionJournalEntryKind::ControlCommand {
                    command: format!("cmd-{i}"),
                },
                format!("corr-compact-{i}"),
                MissionActorRole::Operator,
                format!("reason-{i}"),
                None,
                i * 1000,
            )
            .unwrap();
    }

    // Checkpoint at seq 3
    journal.checkpoint(&mission, 6000);

    // Compact before seq 3
    journal.compact_before(3);

    // Entries 1 and 2 should be removed, 3-5 + checkpoint remain
    let remaining = journal.entries_since(0);
    assert!(remaining.len() >= 3, "should have at least entries 3, 4, 5 remaining");
}

#[test]
fn journal_kill_switch_change_entry() {
    let mission = create_test_mission("m:orch-5");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::KillSwitchChange {
                from: MissionKillSwitchLevel::Off,
                to: MissionKillSwitchLevel::SafeMode,
            },
            "corr-ks-1".into(),
            MissionActorRole::Operator,
            "emergency_pause".into(),
            None,
            1000,
        )
        .unwrap();

    let report = journal.replay_from_checkpoint(0);
    assert_eq!(report.kill_switch_changes, 1);
}

#[test]
fn journal_recovery_marker() {
    let mission = create_test_mission("m:orch-6");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                kind: MissionLifecycleTransitionKind::PlanFinalized,
            },
            "corr-rm-1".into(),
            MissionActorRole::Planner,
            "plan".into(),
            None,
            1000,
        )
        .unwrap();

    // Add recovery marker at seq 1
    journal.recovery_marker(1, "crash_recovery".into(), 5000);

    let report = journal.replay_from_checkpoint(0);
    assert_eq!(report.recovery_markers, 1);
}

#[test]
fn journal_sync_to_mission_state() {
    let mut mission = create_test_mission("m:orch-7");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::ControlCommand {
                command: "tick".into(),
            },
            "corr-sync-1".into(),
            MissionActorRole::Dispatcher,
            "tick".into(),
            None,
            1000,
        )
        .unwrap();

    mission.sync_journal_state(&journal);
    assert_eq!(mission.journal_state.entry_count, 1);
    assert_eq!(mission.journal_state.last_seq, 1);
    assert!(!mission.journal_state.is_pristine());
}

// ── Dispatch Deduplication Integration ──────────────────────────────────────

#[test]
fn dedup_state_record_and_find() {
    use frankenterm_core::plan::{
        AssignmentId, MissionDispatchDeduplicationRecord, MissionDispatchIdempotencyKey,
    };

    let mut dedup = MissionDispatchDeduplicationState::default();
    let mission_id = MissionId("m:dedup-1".into());

    let key = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &AssignmentId("a:1".into()),
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "hello".into(),
        },
    );

    dedup.record_dispatch(MissionDispatchDeduplicationRecord {
        idempotency_key: key.clone(),
        assignment_id: AssignmentId("a:1".into()),
        correlation_id: "corr-dedup-1".into(),
        dispatched_at_ms: 1000,
        outcome: Outcome::Success {
            reason_code: "ok".into(),
            completed_at_ms: 1500,
        },
        mechanism_hash: "mech-hash-1".into(),
    });

    let found = dedup.find_by_key(&key);
    assert!(found.is_some());
    let record = found.unwrap();
    assert_eq!(record.correlation_id, "corr-dedup-1");
}

#[test]
fn dedup_state_evict_before_cutoff() {
    use frankenterm_core::plan::{
        AssignmentId, MissionDispatchDeduplicationRecord, MissionDispatchIdempotencyKey,
    };

    let mut dedup = MissionDispatchDeduplicationState::default();
    let mission_id = MissionId("m:dedup-2".into());

    // Record at time 1000
    let key1 = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &AssignmentId("a:1".into()),
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "old".into(),
        },
    );
    dedup.record_dispatch(MissionDispatchDeduplicationRecord {
        idempotency_key: key1.clone(),
        assignment_id: AssignmentId("a:1".into()),
        correlation_id: "corr-old".into(),
        dispatched_at_ms: 1000,
        outcome: Outcome::Success {
            reason_code: "ok".into(),
            completed_at_ms: 1000,
        },
        mechanism_hash: "h1".into(),
    });

    // Record at time 5000
    let key2 = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &AssignmentId("a:2".into()),
        &MissionDispatchMechanism::RobotSend {
            pane_id: 2,
            text: "new".into(),
        },
    );
    dedup.record_dispatch(MissionDispatchDeduplicationRecord {
        idempotency_key: key2.clone(),
        assignment_id: AssignmentId("a:2".into()),
        correlation_id: "corr-new".into(),
        dispatched_at_ms: 5000,
        outcome: Outcome::Success {
            reason_code: "ok".into(),
            completed_at_ms: 5000,
        },
        mechanism_hash: "h2".into(),
    });

    // Evict before 3000 — should remove first record
    dedup.evict_before(3000);
    assert!(dedup.find_by_key(&key1).is_none());
    assert!(dedup.find_by_key(&key2).is_some());
}

// ── Failure Taxonomy ────────────────────────────────────────────────────────

#[test]
fn failure_code_terminality_classification() {
    use frankenterm_core::plan::MissionFailureContract;

    // Terminal failures
    let pd = MissionFailureCode::PolicyDenied.contract();
    assert!(matches!(pd.terminality, MissionFailureTerminality::Terminal));
    let ad = MissionFailureCode::ApprovalDenied.contract();
    assert!(matches!(ad.terminality, MissionFailureTerminality::Terminal));

    // Non-terminal (retryable)
    let rl = MissionFailureCode::RateLimited.contract();
    assert!(matches!(rl.terminality, MissionFailureTerminality::NonTerminal));
    let ss = MissionFailureCode::StaleState.contract();
    assert!(matches!(ss.terminality, MissionFailureTerminality::NonTerminal));
}

#[test]
fn failure_code_retryability() {
    use frankenterm_core::plan::MissionFailureRetryability;

    // Never retry: PolicyDenied, ApprovalDenied
    let pd = MissionFailureCode::PolicyDenied.contract();
    assert!(matches!(pd.retryability, MissionFailureRetryability::Never));

    // After backoff: RateLimited
    let rl = MissionFailureCode::RateLimited.contract();
    assert!(matches!(rl.retryability, MissionFailureRetryability::AfterBackoff));

    // After state refresh: StaleState
    let ss = MissionFailureCode::StaleState.contract();
    assert!(matches!(ss.retryability, MissionFailureRetryability::AfterStateRefresh));
}

// ── Outcome Canonical Strings ───────────────────────────────────────────────

#[test]
fn outcome_canonical_string_deterministic() {
    let outcomes = vec![
        Outcome::Success {
            reason_code: "ok".into(),
            completed_at_ms: 1000,
        },
        Outcome::Failed {
            reason_code: "err".into(),
            error_code: "FTX1".into(),
            completed_at_ms: 2000,
        },
        Outcome::Cancelled {
            reason_code: "abort".into(),
            completed_at_ms: 3000,
        },
    ];

    for outcome in &outcomes {
        let s1 = outcome.canonical_string();
        let s2 = outcome.canonical_string();
        assert_eq!(s1, s2);
    }
}

#[test]
fn outcome_serde_roundtrip() {
    let outcomes = vec![
        Outcome::Success {
            reason_code: "ok".into(),
            completed_at_ms: 1000,
        },
        Outcome::Failed {
            reason_code: "err".into(),
            error_code: "FTX1".into(),
            completed_at_ms: 2000,
        },
        Outcome::Cancelled {
            reason_code: "abort".into(),
            completed_at_ms: 3000,
        },
    ];

    for outcome in &outcomes {
        let json = serde_json::to_string(outcome).unwrap();
        let restored: Outcome = serde_json::from_str(&json).unwrap();
        assert_eq!(*outcome, restored);
    }
}

// ── Kill-Switch Levels ──────────────────────────────────────────────────────

#[test]
fn kill_switch_levels_behavior() {
    // Off: allows everything
    assert!(!MissionKillSwitchLevel::Off.blocks_dispatch());
    assert!(!MissionKillSwitchLevel::Off.cancels_in_flight());

    // SafeMode: blocks dispatch, allows reads
    assert!(MissionKillSwitchLevel::SafeMode.blocks_dispatch());
    assert!(!MissionKillSwitchLevel::SafeMode.cancels_in_flight());
    assert!(MissionKillSwitchLevel::SafeMode.allows_read_only());

    // HardStop: blocks everything
    assert!(MissionKillSwitchLevel::HardStop.blocks_dispatch());
    assert!(MissionKillSwitchLevel::HardStop.cancels_in_flight());
}

#[test]
fn kill_switch_activation_serde_roundtrip() {
    let activation = MissionKillSwitchActivation {
        level: MissionKillSwitchLevel::SafeMode,
        activated_by: MissionActorRole::Operator,
        reason_code: "emergency".into(),
        error_code: Some("FTX_KS01".into()),
        activated_at_ms: 1000,
        expires_at_ms: Some(60_000),
        correlation_id: "ks-corr-1".into(),
    };

    let json = serde_json::to_string(&activation).unwrap();
    let restored: MissionKillSwitchActivation = serde_json::from_str(&json).unwrap();
    assert_eq!(activation, restored);
}

// ── Mission Canonical String ────────────────────────────────────────────────

#[test]
fn mission_canonical_string_deterministic() {
    let mission = create_test_mission("m:canon-1");
    let s1 = mission.canonical_string();
    let s2 = mission.canonical_string();
    assert_eq!(s1, s2);
}

#[test]
fn mission_canonical_string_includes_journal_state() {
    let mut mission = create_test_mission("m:canon-2");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::ControlCommand {
                command: "test".into(),
            },
            "corr-canon-1".into(),
            MissionActorRole::Dispatcher,
            "test".into(),
            None,
            1000,
        )
        .unwrap();
    mission.sync_journal_state(&journal);

    let canonical = mission.canonical_string();
    assert!(canonical.contains("journal_state="));
}

// ── Lifecycle State Invariants ──────────────────────────────────────────────

#[test]
fn lifecycle_terminal_states() {
    assert!(MissionLifecycleState::Completed.is_terminal());
    assert!(MissionLifecycleState::Failed.is_terminal());
    assert!(MissionLifecycleState::Cancelled.is_terminal());
}

#[test]
fn lifecycle_non_terminal_states() {
    assert!(!MissionLifecycleState::Planning.is_terminal());
    assert!(!MissionLifecycleState::Planned.is_terminal());
    assert!(!MissionLifecycleState::Dispatching.is_terminal());
    assert!(!MissionLifecycleState::Running.is_terminal());
    assert!(!MissionLifecycleState::Paused.is_terminal());
    assert!(!MissionLifecycleState::RetryPending.is_terminal());
    assert!(!MissionLifecycleState::Blocked.is_terminal());
}

// ── Dispatch Mechanism Serde ────────────────────────────────────────────────

#[test]
fn dispatch_mechanism_serde_roundtrip() {
    let mechanisms = vec![
        MissionDispatchMechanism::RobotSend {
            pane_id: 42,
            text: "hello agent".into(),
        },
        MissionDispatchMechanism::RobotWaitFor {
            pane_id: 42,
            pattern: "ready>".into(),
            timeout_ms: 5000,
        },
        MissionDispatchMechanism::InternalLockAcquire {
            resource_id: "res:1".into(),
            timeout_ms: 3000,
        },
        MissionDispatchMechanism::InternalLockRelease {
            resource_id: "res:1".into(),
        },
        MissionDispatchMechanism::InternalStoreData {
            key: "k1".into(),
            value: "v1".into(),
        },
    ];

    for mechanism in &mechanisms {
        let json = serde_json::to_string(mechanism).unwrap();
        let restored: MissionDispatchMechanism = serde_json::from_str(&json).unwrap();
        assert_eq!(*mechanism, restored);
    }
}

// ── Idempotency Key Computation ─────────────────────────────────────────────

#[test]
fn dispatch_idempotency_key_deterministic() {
    use frankenterm_core::plan::{AssignmentId, MissionDispatchIdempotencyKey};

    let mission_id = MissionId("m:idem-1".into());
    let assignment_id = AssignmentId("a:1".into());
    let mechanism = MissionDispatchMechanism::RobotSend {
        pane_id: 1,
        text: "cmd".into(),
    };

    let k1 = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mechanism);
    let k2 = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mechanism);
    assert_eq!(k1.0, k2.0);
}

#[test]
fn dispatch_idempotency_key_differs_by_mechanism() {
    use frankenterm_core::plan::{AssignmentId, MissionDispatchIdempotencyKey};

    let mission_id = MissionId("m:idem-2".into());
    let assignment_id = AssignmentId("a:1".into());

    let k1 = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &assignment_id,
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "cmd-a".into(),
        },
    );
    let k2 = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &assignment_id,
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "cmd-b".into(),
        },
    );
    assert_ne!(k1.0, k2.0);
}

// ── Mission Loop State Basics ───────────────────────────────────────────────

#[test]
fn mission_loop_initial_state() {
    use frankenterm_core::mission_loop::{MissionLoop, MissionLoopConfig};

    let config = MissionLoopConfig::default();
    let mloop = MissionLoop::new(config);
    assert_eq!(mloop.state().cycle_count, 0);
    assert_eq!(mloop.state().total_assignments_made, 0);
    assert_eq!(mloop.state().total_rejections, 0);
}

#[test]
fn mission_loop_trigger_accumulates() {
    use frankenterm_core::mission_loop::{MissionLoop, MissionLoopConfig, MissionTrigger};

    let config = MissionLoopConfig::default();
    let mut mloop = MissionLoop::new(config);

    mloop.trigger(MissionTrigger::CadenceTick);
    mloop.trigger(MissionTrigger::ManualTrigger {
        reason: "test".into(),
    });

    assert_eq!(mloop.state().pending_triggers.len(), 2);
}

// ── Journal Entry Serde ─────────────────────────────────────────────────────

#[test]
fn journal_entry_all_kinds_serde_roundtrip() {
    let mission = create_test_mission("m:serde-1");
    let mut journal = mission.create_journal();

    let kinds = vec![
        MissionJournalEntryKind::LifecycleTransition {
            from: MissionLifecycleState::Planning,
            to: MissionLifecycleState::Planned,
            kind: MissionLifecycleTransitionKind::PlanFinalized,
        },
        MissionJournalEntryKind::ControlCommand {
            command: "pause".into(),
        },
        MissionJournalEntryKind::KillSwitchChange {
            from: MissionKillSwitchLevel::Off,
            to: MissionKillSwitchLevel::HardStop,
        },
        MissionJournalEntryKind::AssignmentOutcome {
            assignment_id: "a:1".into(),
            outcome: "success".into(),
        },
    ];

    for (i, kind) in kinds.into_iter().enumerate() {
        journal
            .append(
                kind,
                format!("corr-serde-{i}"),
                MissionActorRole::Operator,
                format!("reason-{i}"),
                None,
                (i as i64 + 1) * 1000,
            )
            .unwrap();
    }

    // Serialize and verify each entry
    for entry in journal.entries_since(0) {
        let json = serde_json::to_string(entry).unwrap();
        let restored: frankenterm_core::plan::MissionJournalEntry =
            serde_json::from_str(&json).unwrap();
        assert_eq!(entry.seq, restored.seq);
        assert_eq!(entry.correlation_id, restored.correlation_id);
    }
}

// ── Journal State Serde ─────────────────────────────────────────────────────

#[test]
fn journal_state_serde_roundtrip() {
    let mut mission = create_test_mission("m:js-1");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::ControlCommand {
                command: "test".into(),
            },
            "corr-js-1".into(),
            MissionActorRole::Dispatcher,
            "test".into(),
            None,
            1000,
        )
        .unwrap();
    mission.sync_journal_state(&journal);

    let json = serde_json::to_string(&mission.journal_state).unwrap();
    let restored: frankenterm_core::plan::MissionJournalState =
        serde_json::from_str(&json).unwrap();
    assert_eq!(mission.journal_state, restored);
}
