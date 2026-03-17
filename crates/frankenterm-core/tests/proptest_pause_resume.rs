#![cfg(feature = "subprocess-bridge")]

//! Property-based tests for mission pause/resume/abort semantics (C5).
//!
//! Covers:
//! - Pause/resume roundtrip preserves lifecycle state
//! - Checkpoint captures correct paused_from_state
//! - Cumulative pause duration is monotonically non-decreasing
//! - Abort from any non-terminal state reaches Cancelled
//! - Abort idempotently cancels all in-flight assignments
//! - MissionControlCommand serde roundtrip
//! - MissionCheckpoint serde roundtrip
//! - MissionPauseResumeState serde roundtrip
//! - Canonical string determinism
//! - Eviction bounds history length
//! - Multiple pause/resume cycles maintain consistent counters
//! - Checkpoint assignment entries match mission assignments count

use frankenterm_core::plan::{
    ApprovalState, Assignment, AssignmentId, CandidateAction, CandidateActionId, Mission,
    MissionActorRole, MissionCheckpoint, MissionControlCommand, MissionId, MissionLifecycleState,
    MissionOwnership, MissionPauseResumeState, StepAction,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_pausable_state() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Running),
        Just(MissionLifecycleState::Dispatching),
        Just(MissionLifecycleState::AwaitingApproval),
        Just(MissionLifecycleState::Blocked),
        Just(MissionLifecycleState::RetryPending),
    ]
}

fn arb_non_terminal_state() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Planning),
        Just(MissionLifecycleState::Planned),
        Just(MissionLifecycleState::Dispatching),
        Just(MissionLifecycleState::AwaitingApproval),
        Just(MissionLifecycleState::Running),
        Just(MissionLifecycleState::Paused),
        Just(MissionLifecycleState::RetryPending),
        Just(MissionLifecycleState::Blocked),
    ]
}

fn arb_terminal_state() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Completed),
        Just(MissionLifecycleState::Failed),
        Just(MissionLifecycleState::Cancelled),
    ]
}

fn arb_non_empty_string() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,15}".prop_map(|s| s)
}

fn arb_timestamp() -> impl Strategy<Value = i64> {
    1000i64..1_000_000i64
}

fn arb_control_command() -> impl Strategy<Value = MissionControlCommand> {
    prop_oneof![
        (
            arb_non_empty_string(),
            arb_non_empty_string(),
            arb_timestamp()
        )
            .prop_map(|(by, reason, ts)| MissionControlCommand::Pause {
                requested_by: by,
                reason_code: reason,
                requested_at_ms: ts,
                correlation_id: None,
            }),
        (
            arb_non_empty_string(),
            arb_non_empty_string(),
            arb_timestamp()
        )
            .prop_map(|(by, reason, ts)| MissionControlCommand::Resume {
                requested_by: by,
                reason_code: reason,
                requested_at_ms: ts,
                correlation_id: None,
            }),
        (
            arb_non_empty_string(),
            arb_non_empty_string(),
            arb_timestamp()
        )
            .prop_map(|(by, reason, ts)| MissionControlCommand::Abort {
                requested_by: by,
                reason_code: reason,
                error_code: None,
                requested_at_ms: ts,
                correlation_id: None,
            }),
    ]
}

fn arb_checkpoint() -> impl Strategy<Value = MissionCheckpoint> {
    (
        arb_non_empty_string(),
        arb_pausable_state(),
        arb_non_empty_string(),
        arb_non_empty_string(),
        arb_timestamp(),
    )
        .prop_map(|(id, state, by, reason, ts)| MissionCheckpoint {
            checkpoint_id: id,
            paused_from_state: state,
            paused_by: by,
            reason_code: reason,
            paused_at_ms: ts,
            resumed_at_ms: None,
            resumed_by: None,
            assignment_entries: Vec::new(),
            correlation_id: None,
        })
}

fn make_mission(state: MissionLifecycleState, num_assignments: usize) -> Mission {
    let mut mission = Mission::new(
        MissionId(format!("m-prop-{}", state)),
        "proptest",
        "ws-prop",
        MissionOwnership::solo("agent-prop"),
        1000,
    );
    mission.lifecycle_state = state;

    for i in 0..num_assignments {
        let cid = CandidateActionId(format!("c{i}"));
        mission.candidates.push(CandidateAction {
            candidate_id: cid.clone(),
            requested_by: MissionActorRole::Planner,
            action: StepAction::SendText {
                pane_id: 0,
                text: format!("action-{i}"),
                paste_mode: None,
            },
            rationale: "proptest".into(),
            score: None,
            created_at_ms: 1000,
        });
        mission.assignments.push(Assignment {
            assignment_id: AssignmentId(format!("a{i}")),
            candidate_id: cid,
            assigned_by: MissionActorRole::Dispatcher,
            assignee: format!("agent-{i}"),
            created_at_ms: 1000,
            updated_at_ms: None,
            approval_state: ApprovalState::NotRequired,
            outcome: None,
            reservation_intent: None,
            escalation: None,
        });
    }

    mission
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn pause_resume_roundtrip_preserves_lifecycle(
        state in arb_pausable_state(),
        pause_ts in 2000i64..100_000i64,
        resume_ts_offset in 100i64..50_000i64,
    ) {
        let resume_ts = pause_ts + resume_ts_offset;
        let mut mission = make_mission(state, 0);

        mission.pause_mission("op", "test", pause_ts, None).unwrap();
        prop_assert_eq!(mission.lifecycle_state, MissionLifecycleState::Paused);

        mission.resume_mission("op", "test", resume_ts, None).unwrap();
        prop_assert_eq!(mission.lifecycle_state, state);
    }

    #[test]
    fn checkpoint_records_correct_paused_from_state(
        state in arb_pausable_state(),
    ) {
        let mut mission = make_mission(state, 2);

        mission.pause_mission("op", "test", 5000, None).unwrap();

        let cp = mission.pause_resume_state.current_checkpoint.as_ref().unwrap();
        prop_assert_eq!(cp.paused_from_state, state);
        prop_assert_eq!(cp.assignment_entries.len(), 2);
    }

    #[test]
    fn cumulative_duration_monotonically_increases(
        cycles in 1usize..6,
    ) {
        let mut mission = make_mission(MissionLifecycleState::Running, 0);
        let mut prev_duration = 0i64;

        for i in 0..cycles {
            let pause_ts = (i as i64) * 2000 + 2000;
            let resume_ts = pause_ts + 500;

            mission.pause_mission("op", "test", pause_ts, None).unwrap();
            mission.resume_mission("op", "test", resume_ts, None).unwrap();

            let current = mission.pause_resume_state.cumulative_pause_duration_ms;
            prop_assert!(current >= prev_duration, "duration must be monotonic: {} >= {}", current, prev_duration);
            prev_duration = current;
        }
    }

    #[test]
    fn abort_from_non_terminal_reaches_cancelled(
        state in arb_non_terminal_state(),
    ) {
        let mut mission = make_mission(MissionLifecycleState::Running, 1);

        // If state is Paused, we need to pause first
        if state == MissionLifecycleState::Paused {
            mission.pause_mission("op", "setup", 1500, None).unwrap();
        } else {
            mission.lifecycle_state = state;
        }

        let result = mission.abort_mission("op", "test_abort", None, 5000, None);
        prop_assert!(result.is_ok());
        prop_assert_eq!(mission.lifecycle_state, MissionLifecycleState::Cancelled);
    }

    #[test]
    fn abort_cancels_all_inflight_assignments(
        num_assignments in 0usize..5,
    ) {
        let mut mission = make_mission(MissionLifecycleState::Running, num_assignments);

        mission.abort_mission("op", "abort", None, 5000, None).unwrap();

        for assignment in &mission.assignments {
            let is_cancelled = matches!(assignment.outcome, Some(frankenterm_core::plan::Outcome::Cancelled { .. }));
            prop_assert!(is_cancelled, "assignment {} must be cancelled", assignment.assignment_id.0);
        }
    }

    #[test]
    fn abort_rejects_terminal_states(
        state in arb_terminal_state(),
    ) {
        let mut mission = make_mission(state, 0);
        let result = mission.abort_mission("op", "test", None, 5000, None);
        prop_assert!(result.is_err());
    }

    #[test]
    fn pause_rejects_non_pausable_states(
        state in prop_oneof![
            Just(MissionLifecycleState::Planning),
            Just(MissionLifecycleState::Planned),
            Just(MissionLifecycleState::Paused),
            Just(MissionLifecycleState::Completed),
            Just(MissionLifecycleState::Failed),
            Just(MissionLifecycleState::Cancelled),
        ],
    ) {
        let mut mission = make_mission(state, 0);
        let result = mission.pause_mission("op", "test", 5000, None);
        prop_assert!(result.is_err());
    }

    #[test]
    fn control_command_serde_roundtrip(
        cmd in arb_control_command(),
    ) {
        let json = serde_json::to_string(&cmd).unwrap();
        let restored: MissionControlCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&cmd, &restored);
    }

    #[test]
    fn checkpoint_serde_roundtrip(
        cp in arb_checkpoint(),
    ) {
        let json = serde_json::to_string(&cp).unwrap();
        let restored: MissionCheckpoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&cp, &restored);
    }

    #[test]
    fn pause_resume_state_serde_roundtrip(
        pause_count in 0u32..10,
        resume_count in 0u32..10,
        abort_count in 0u32..5,
        duration_ms in 0i64..100_000,
    ) {
        let state = MissionPauseResumeState {
            current_checkpoint: None,
            checkpoint_history: Vec::new(),
            total_pause_count: pause_count,
            total_resume_count: resume_count,
            total_abort_count: abort_count,
            cumulative_pause_duration_ms: duration_ms,
        };

        let json = serde_json::to_string(&state).unwrap();
        let restored: MissionPauseResumeState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&state, &restored);
    }

    #[test]
    fn canonical_string_is_deterministic(
        state in arb_pausable_state(),
    ) {
        let mut mission = make_mission(state, 1);
        mission.pause_mission("op", "test", 5000, None).unwrap();

        let s1 = mission.pause_resume_state.canonical_string();
        let s2 = mission.pause_resume_state.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn eviction_bounds_history_length(
        history_size in 1usize..20,
        cutoff_fraction in 0.0f64..1.0,
    ) {
        let mut state = MissionPauseResumeState::default();
        for i in 0..history_size {
            state.checkpoint_history.push(MissionCheckpoint {
                checkpoint_id: format!("cp-{i}"),
                paused_from_state: MissionLifecycleState::Running,
                paused_by: "op".into(),
                reason_code: "test".into(),
                paused_at_ms: (i as i64) * 1000,
                resumed_at_ms: Some((i as i64) * 1000 + 500),
                resumed_by: Some("op".into()),
                assignment_entries: Vec::new(),
                correlation_id: None,
            });
        }

        let cutoff_ms = ((history_size as f64) * cutoff_fraction * 1000.0) as i64;
        state.evict_history_before(cutoff_ms);

        for cp in &state.checkpoint_history {
            prop_assert!(cp.paused_at_ms >= cutoff_ms, "retained entry {} must be >= cutoff {}", cp.paused_at_ms, cutoff_ms);
        }
    }

    #[test]
    fn multiple_cycles_consistent_counters(
        cycles in 1usize..8,
    ) {
        let mut mission = make_mission(MissionLifecycleState::Running, 0);

        for i in 0..cycles {
            let pause_ts = (i as i64) * 2000 + 2000;
            let resume_ts = pause_ts + 500;
            mission.pause_mission("op", "test", pause_ts, None).unwrap();
            mission.resume_mission("op", "test", resume_ts, None).unwrap();
        }

        prop_assert_eq!(mission.pause_resume_state.total_pause_count, cycles as u32);
        prop_assert_eq!(mission.pause_resume_state.total_resume_count, cycles as u32);
        prop_assert_eq!(mission.pause_resume_state.checkpoint_history.len(), cycles);
        prop_assert_eq!(mission.pause_resume_state.total_abort_count, 0);
    }

    #[test]
    fn checkpoint_entries_match_assignment_count(
        num_assignments in 0usize..6,
    ) {
        let mut mission = make_mission(MissionLifecycleState::Running, num_assignments);
        mission.pause_mission("op", "test", 5000, None).unwrap();

        let cp = mission.pause_resume_state.current_checkpoint.as_ref().unwrap();
        prop_assert_eq!(cp.assignment_entries.len(), num_assignments);
    }

    #[test]
    fn control_command_canonical_string_deterministic(
        cmd in arb_control_command(),
    ) {
        let s1 = cmd.canonical_string();
        let s2 = cmd.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn pause_then_abort_finalizes_checkpoint(
        state in arb_pausable_state(),
        pause_ts in 2000i64..50_000i64,
        abort_offset in 100i64..50_000i64,
    ) {
        let abort_ts = pause_ts + abort_offset;
        let mut mission = make_mission(state, 1);

        mission.pause_mission("op", "test", pause_ts, None).unwrap();
        prop_assert!(mission.pause_resume_state.is_paused());

        mission.abort_mission("op", "abort", None, abort_ts, None).unwrap();
        prop_assert!(!mission.pause_resume_state.is_paused());
        prop_assert_eq!(mission.pause_resume_state.checkpoint_history.len(), 1);

        let cp = &mission.pause_resume_state.checkpoint_history[0];
        prop_assert_eq!(cp.resumed_at_ms, Some(abort_ts));
    }
}
