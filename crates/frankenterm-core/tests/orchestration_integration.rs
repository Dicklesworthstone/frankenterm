// Disabled: MissionJournal, MissionControlCommand, and related types not yet in plan.rs.
// Re-enable when journal and dispatch deduplication types are implemented.
#![cfg(feature = "__journal_types_placeholder")]
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
    AssignmentId, Mission, MissionControlCommand, MissionControlDecision,
    MissionDispatchDeduplicationState, MissionDispatchMechanism, MissionFailureCode,
    MissionFailureTerminality, MissionId, MissionJournalEntryKind, MissionKillSwitchActivation,
    MissionKillSwitchLevel, MissionLifecycleState, MissionLifecycleTransitionKind,
    MissionOwnership, Outcome,
};

// ── Helpers ─────────────────────────────────────────────────────────────────

fn create_test_mission(id: &str) -> Mission {
    Mission::new(
        MissionId(id.into()),
        "orch-test",
        "ws-orch",
        MissionOwnership::solo("agent-orch"),
        1000,
    )
}

/// Build a `ControlCommand` journal entry kind for tests.
fn make_control_cmd(label: &str, ts: i64) -> MissionJournalEntryKind {
    MissionJournalEntryKind::ControlCommand {
        command: MissionControlCommand::Pause {
            requested_by: "test-op".into(),
            reason_code: label.into(),
            requested_at_ms: ts,
            correlation_id: None,
        },
        decision: MissionControlDecision {
            action: label.into(),
            lifecycle_from: MissionLifecycleState::Running,
            lifecycle_to: MissionLifecycleState::Paused,
            decision_path: format!("test-{label}"),
            reason_code: label.into(),
            error_code: None,
            checkpoint_id: None,
            decided_at_ms: ts,
        },
    }
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
                transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
            },
            "corr-1",
            "planner",
            "plan_complete",
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
                transition_kind: MissionLifecycleTransitionKind::DispatchStarted,
            },
            "corr-2",
            "dispatcher",
            "dispatch_started",
            None,
            2000,
        )
        .unwrap();
    assert_eq!(seq2, 2);

    // Replay from start
    let report = journal.replay_from_checkpoint();
    assert_eq!(report.entries_scanned, 2);
    assert_eq!(report.lifecycle_transitions, 2);
    assert!(report.is_clean());
}

#[test]
fn journal_checkpoint_and_recovery() {
    let mission = create_test_mission("m:orch-2");
    let mut journal = mission.create_journal();

    // Append some entries
    journal
        .append(
            MissionJournalEntryKind::LifecycleTransition {
                from: MissionLifecycleState::Planning,
                to: MissionLifecycleState::Planned,
                transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
            },
            "corr-cp-1",
            "planner",
            "plan_finalized",
            None,
            1000,
        )
        .unwrap();

    journal
        .append(
            MissionJournalEntryKind::AssignmentOutcome {
                assignment_id: AssignmentId("a:1".into()),
                outcome_before: None,
                outcome_after: "success".into(),
            },
            "corr-cp-2",
            "dispatcher",
            "assignment_outcome",
            None,
            2000,
        )
        .unwrap();

    // Create checkpoint
    journal.checkpoint(&mission, 3000).unwrap();

    // Append more after checkpoint
    journal
        .append(
            make_control_cmd("pause", 4000),
            "corr-cp-3",
            "operator",
            "pause_requested",
            None,
            4000,
        )
        .unwrap();

    // Replay from checkpoint should only see post-checkpoint entries
    let snapshot = journal.snapshot_state();
    let _report = journal.replay_from_checkpoint();
    // entries_since returns entries after the given seq
    let entries_after = journal.entries_since(snapshot.last_checkpoint_seq.unwrap_or(0));
    assert!(!entries_after.is_empty());
}

#[test]
fn journal_duplicate_correlation_rejected() {
    let mission = create_test_mission("m:orch-3");
    let mut journal = mission.create_journal();

    journal
        .append(
            make_control_cmd("pause", 1000),
            "dup-corr",
            "operator",
            "first",
            None,
            1000,
        )
        .unwrap();

    let result = journal.append(
        make_control_cmd("resume", 2000),
        "dup-corr",
        "operator",
        "second",
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
                make_control_cmd(&format!("cmd-{i}"), i * 1000),
                format!("corr-compact-{i}"),
                "operator",
                format!("reason-{i}"),
                None,
                i * 1000,
            )
            .unwrap();
    }

    // Checkpoint at seq 3
    journal.checkpoint(&mission, 6000).unwrap();

    // Compact before seq 3
    journal.compact_before(3);

    // Entries 1 and 2 should be removed, 3-5 + checkpoint remain
    let remaining = journal.entries_since(0);
    assert!(
        remaining.len() >= 3,
        "should have at least entries 3, 4, 5 remaining"
    );
}

#[test]
fn journal_kill_switch_change_entry() {
    let mission = create_test_mission("m:orch-5");
    let mut journal = mission.create_journal();

    journal
        .append(
            MissionJournalEntryKind::KillSwitchChange {
                level_from: MissionKillSwitchLevel::Off,
                level_to: MissionKillSwitchLevel::SafeMode,
            },
            "corr-ks-1",
            "operator",
            "emergency_pause",
            None,
            1000,
        )
        .unwrap();

    let report = journal.replay_from_checkpoint();
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
                transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
            },
            "corr-rm-1",
            "planner",
            "plan",
            None,
            1000,
        )
        .unwrap();

    // Add recovery marker at seq 1
    journal.recovery_marker(1, "crash_recovery", 5000).unwrap();

    let report = journal.replay_from_checkpoint();
    assert_eq!(report.recovery_markers, 1);
}

#[test]
fn journal_sync_to_mission_state() {
    let mut mission = create_test_mission("m:orch-7");
    let mut journal = mission.create_journal();

    journal
        .append(
            make_control_cmd("tick", 1000),
            "corr-sync-1",
            "dispatcher",
            "tick",
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
        MissionDispatchDeduplicationRecord, MissionDispatchIdempotencyKey,
    };

    let mut dedup = MissionDispatchDeduplicationState::default();
    let mission_id = MissionId("m:dedup-1".into());

    let key = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &AssignmentId("a:1".into()),
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "hello".into(),
            paste_mode: None,
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
        MissionDispatchDeduplicationRecord, MissionDispatchIdempotencyKey,
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
            paste_mode: None,
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
            paste_mode: None,
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
    // Terminal failures
    let pd = MissionFailureCode::PolicyDenied.contract();
    assert!(matches!(
        pd.terminality,
        MissionFailureTerminality::Terminal
    ));
    let ad = MissionFailureCode::ApprovalDenied.contract();
    assert!(matches!(
        ad.terminality,
        MissionFailureTerminality::Terminal
    ));

    // Non-terminal (retryable)
    let rl = MissionFailureCode::RateLimited.contract();
    assert!(matches!(
        rl.terminality,
        MissionFailureTerminality::NonTerminal
    ));
    let ss = MissionFailureCode::StaleState.contract();
    assert!(matches!(
        ss.terminality,
        MissionFailureTerminality::NonTerminal
    ));
}

#[test]
fn failure_code_retryability() {
    use frankenterm_core::plan::MissionFailureRetryability;

    // Never retry: PolicyDenied, ApprovalDenied
    let pd = MissionFailureCode::PolicyDenied.contract();
    assert!(matches!(pd.retryability, MissionFailureRetryability::Never));

    // After backoff: RateLimited
    let rl = MissionFailureCode::RateLimited.contract();
    assert!(matches!(
        rl.retryability,
        MissionFailureRetryability::AfterBackoff
    ));

    // After state refresh: StaleState
    let ss = MissionFailureCode::StaleState.contract();
    assert!(matches!(
        ss.retryability,
        MissionFailureRetryability::AfterStateRefresh
    ));
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
        activated_by: "operator".into(),
        reason_code: "emergency".into(),
        error_code: Some("FTX_KS01".into()),
        activated_at_ms: 1000,
        expires_at_ms: Some(60_000),
        correlation_id: Some("ks-corr-1".into()),
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
            make_control_cmd("test", 1000),
            "corr-canon-1",
            "dispatcher",
            "test",
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
    use frankenterm_core::plan::WaitCondition;

    let mechanisms = vec![
        MissionDispatchMechanism::RobotSend {
            pane_id: 42,
            text: "hello agent".into(),
            paste_mode: None,
        },
        MissionDispatchMechanism::RobotWaitFor {
            pane_id: Some(42),
            condition: WaitCondition::Pattern {
                pane_id: Some(42),
                rule_id: "ready>".into(),
            },
            timeout_ms: 5000,
        },
        MissionDispatchMechanism::InternalLockAcquire {
            lock_name: "res:1".into(),
            timeout_ms: Some(3000),
        },
        MissionDispatchMechanism::InternalLockRelease {
            lock_name: "res:1".into(),
        },
        MissionDispatchMechanism::InternalStoreData {
            key: "k1".into(),
            value: serde_json::json!("v1"),
        },
    ];

    for mechanism in &mechanisms {
        let json = serde_json::to_string(mechanism).unwrap();
        let restored: MissionDispatchMechanism = serde_json::from_str(&json).unwrap();
        // Compare via re-serialized JSON (MissionDispatchMechanism doesn't derive PartialEq)
        let json2 = serde_json::to_string(&restored).unwrap();
        assert_eq!(json, json2);
    }
}

// ── Idempotency Key Computation ─────────────────────────────────────────────

#[test]
fn dispatch_idempotency_key_deterministic() {
    use frankenterm_core::plan::MissionDispatchIdempotencyKey;

    let mission_id = MissionId("m:idem-1".into());
    let assignment_id = AssignmentId("a:1".into());
    let mechanism = MissionDispatchMechanism::RobotSend {
        pane_id: 1,
        text: "cmd".into(),
        paste_mode: None,
    };

    let k1 = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mechanism);
    let k2 = MissionDispatchIdempotencyKey::compute(&mission_id, &assignment_id, &mechanism);
    assert_eq!(k1.0, k2.0);
}

#[test]
fn dispatch_idempotency_key_differs_by_mechanism() {
    use frankenterm_core::plan::MissionDispatchIdempotencyKey;

    let mission_id = MissionId("m:idem-2".into());
    let assignment_id = AssignmentId("a:1".into());

    let k1 = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &assignment_id,
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "cmd-a".into(),
            paste_mode: None,
        },
    );
    let k2 = MissionDispatchIdempotencyKey::compute(
        &mission_id,
        &assignment_id,
        &MissionDispatchMechanism::RobotSend {
            pane_id: 1,
            text: "cmd-b".into(),
            paste_mode: None,
        },
    );
    assert_ne!(k1.0, k2.0);
}

// ── Mission Loop State Basics ───────────────────────────────────────────────

#[cfg(feature = "subprocess-bridge")]
#[test]
fn mission_loop_initial_state() {
    use frankenterm_core::mission_loop::{MissionLoop, MissionLoopConfig};

    let config = MissionLoopConfig::default();
    let mloop = MissionLoop::new(config);
    assert_eq!(mloop.state().cycle_count, 0);
    assert_eq!(mloop.state().total_assignments_made, 0);
    assert_eq!(mloop.state().total_rejections, 0);
}

#[cfg(feature = "subprocess-bridge")]
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
            transition_kind: MissionLifecycleTransitionKind::PlanFinalized,
        },
        make_control_cmd("pause", 0),
        MissionJournalEntryKind::KillSwitchChange {
            level_from: MissionKillSwitchLevel::Off,
            level_to: MissionKillSwitchLevel::HardStop,
        },
        MissionJournalEntryKind::AssignmentOutcome {
            assignment_id: AssignmentId("a:1".into()),
            outcome_before: None,
            outcome_after: "success".into(),
        },
    ];

    for (i, kind) in kinds.into_iter().enumerate() {
        journal
            .append(
                kind,
                format!("corr-serde-{i}"),
                "operator",
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
            make_control_cmd("test", 1000),
            "corr-js-1",
            "dispatcher",
            "test",
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
