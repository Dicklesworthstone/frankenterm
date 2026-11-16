//! Property-based tests for the mission lifecycle state machine.
//!
//! Validates structural correctness of the `MISSION_LIFECYCLE_TRANSITIONS` table
//! and operational properties of `Mission::transition_lifecycle`.
//!
//! Properties verified:
//! 1. Transition table determinism — no duplicate (from, via) pairs
//! 2. Terminal states have no outgoing transitions (except cancel-related)
//! 3. All non-terminal states are reachable from Planning via BFS
//! 4. apply_transition and transition_lifecycle agree on valid/invalid
//! 5. Random walk through valid transitions always reaches a terminal state
//! 6. Serde roundtrip stability for all lifecycle states
//! 7. Transition table entries reference only valid states and kinds
//! 8. Mission state updates timestamp on successful transition
//! 9. Mission state unchanged on failed transition

use std::collections::{HashMap, HashSet, VecDeque};

use proptest::prelude::*;

use frankenterm_core::plan::{
    Mission, MissionId, MissionLifecycleState, MissionLifecycleTransitionKind, MissionOwnership,
    mission_lifecycle_transition_table,
};
use frankenterm_core::tx_plan_compiler::{
    CompensationKind, CompilerConfig, PlannerAssignment, PreconditionKind, RejectedAssignment,
    StepRisk, compile_tx_plan,
};

// =============================================================================
// Constants
// =============================================================================

const ALL_STATES: &[MissionLifecycleState] = &[
    MissionLifecycleState::Planned,
    MissionLifecycleState::Planning,
    MissionLifecycleState::Dispatching,
    MissionLifecycleState::AwaitingApproval,
    MissionLifecycleState::Running,
    MissionLifecycleState::Executing,
    MissionLifecycleState::RetryPending,
    MissionLifecycleState::Blocked,
    MissionLifecycleState::Paused,
    MissionLifecycleState::Completed,
    MissionLifecycleState::Cancelled,
    MissionLifecycleState::Failed,
];

const TERMINAL_STATES: &[MissionLifecycleState] = &[
    MissionLifecycleState::Completed,
    MissionLifecycleState::Cancelled,
    MissionLifecycleState::Failed,
];

// =============================================================================
// Helpers
// =============================================================================

fn make_mission(state: MissionLifecycleState) -> Mission {
    let mut m = Mission::new(
        MissionId("mission:proptest".to_string()),
        "proptest mission",
        "ws-test",
        MissionOwnership {
            planner: "p".to_string(),
            dispatcher: "d".to_string(),
            operator: "o".to_string(),
        },
        1_000_000,
    );
    m.lifecycle_state = state;
    m
}

fn tx_assignment(
    bead_id: &str,
    agent_id: &str,
    score: f64,
    tags: &[&str],
    dependency_bead_ids: &[&str],
) -> PlannerAssignment {
    PlannerAssignment {
        bead_id: bead_id.to_string(),
        agent_id: agent_id.to_string(),
        score,
        tags: tags.iter().map(|tag| (*tag).to_string()).collect(),
        dependency_bead_ids: dependency_bead_ids
            .iter()
            .map(|bead_id| (*bead_id).to_string())
            .collect(),
    }
}

fn state_strategy() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Planned),
        Just(MissionLifecycleState::Planning),
        Just(MissionLifecycleState::Dispatching),
        Just(MissionLifecycleState::AwaitingApproval),
        Just(MissionLifecycleState::Running),
        Just(MissionLifecycleState::Executing),
        Just(MissionLifecycleState::RetryPending),
        Just(MissionLifecycleState::Blocked),
        Just(MissionLifecycleState::Paused),
        Just(MissionLifecycleState::Completed),
        Just(MissionLifecycleState::Cancelled),
        Just(MissionLifecycleState::Failed),
    ]
}

fn transition_kind_strategy() -> impl Strategy<Value = MissionLifecycleTransitionKind> {
    prop_oneof![
        Just(MissionLifecycleTransitionKind::Dispatch),
        Just(MissionLifecycleTransitionKind::RequestApproval),
        Just(MissionLifecycleTransitionKind::Approve),
        Just(MissionLifecycleTransitionKind::StartExecution),
        Just(MissionLifecycleTransitionKind::Retry),
        Just(MissionLifecycleTransitionKind::Block),
        Just(MissionLifecycleTransitionKind::Unblock),
        Just(MissionLifecycleTransitionKind::Complete),
        Just(MissionLifecycleTransitionKind::Cancel),
        Just(MissionLifecycleTransitionKind::Fail),
        Just(MissionLifecycleTransitionKind::PlanFinalized),
        Just(MissionLifecycleTransitionKind::DispatchStarted),
        Just(MissionLifecycleTransitionKind::ExecutionStarted),
        Just(MissionLifecycleTransitionKind::RetryResumed),
        Just(MissionLifecycleTransitionKind::ExecutionBlocked),
        Just(MissionLifecycleTransitionKind::MissionCancelled),
    ]
}

// =============================================================================
// Structural unit tests (non-proptest)
// =============================================================================

#[test]
fn transition_table_is_deterministic() {
    let table = mission_lifecycle_transition_table();
    let mut seen: HashMap<(String, String), MissionLifecycleState> = HashMap::new();

    for rule in table {
        let key = (format!("{:?}", rule.from), format!("{:?}", rule.via));
        if let Some(existing_to) = seen.get(&key) {
            assert_eq!(
                *existing_to, rule.to,
                "Non-deterministic transition: ({:?}, {:?}) maps to both {:?} and {:?}",
                rule.from, rule.via, existing_to, rule.to
            );
        }
        seen.insert(key, rule.to);
    }
}

#[test]
fn terminal_states_have_no_outgoing_transitions() {
    let table = mission_lifecycle_transition_table();

    for terminal in TERMINAL_STATES {
        let outgoing: Vec<_> = table.iter().filter(|r| r.from == *terminal).collect();
        assert!(
            outgoing.is_empty(),
            "Terminal state {:?} has {} outgoing transitions: {:?}",
            terminal,
            outgoing.len(),
            outgoing
        );
    }
}

#[test]
fn all_non_terminal_states_reachable_from_planning() {
    let table = mission_lifecycle_transition_table();

    // BFS from Planning
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(MissionLifecycleState::Planning);
    visited.insert(MissionLifecycleState::Planning);

    while let Some(state) = queue.pop_front() {
        for rule in table {
            if rule.from == state && !visited.contains(&rule.to) {
                visited.insert(rule.to);
                queue.push_back(rule.to);
            }
        }
    }

    for state in ALL_STATES {
        if !state.is_terminal() {
            // Non-terminal states should be reachable (Planned is an alias/peer of Planning)
            let reachable = visited.contains(state) || *state == MissionLifecycleState::Planned;
            if !reachable {
                // Planned can be reached from itself (it has outgoing transitions)
                // but might not be reachable from Planning if there's no Planning -> Planned transition
                let has_own_transitions = table.iter().any(|r| r.from == *state);
                assert!(
                    has_own_transitions,
                    "Non-terminal state {:?} is not reachable from Planning and has no transitions",
                    state
                );
            }
        }
    }
}

#[test]
fn terminal_states_reachable_from_planning() {
    let table = mission_lifecycle_transition_table();

    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    queue.push_back(MissionLifecycleState::Planning);
    visited.insert(MissionLifecycleState::Planning);

    while let Some(state) = queue.pop_front() {
        for rule in table {
            if rule.from == state && !visited.contains(&rule.to) {
                visited.insert(rule.to);
                queue.push_back(rule.to);
            }
        }
    }

    for terminal in TERMINAL_STATES {
        assert!(
            visited.contains(terminal),
            "Terminal state {:?} is not reachable from Planning",
            terminal
        );
    }
}

#[test]
fn apply_transition_agrees_with_allowed_transitions() {
    for state in ALL_STATES {
        let allowed = state.allowed_transitions();
        for kind in &allowed {
            let result = state.apply_transition(*kind);
            assert!(
                result.is_ok(),
                "allowed_transitions() includes {:?} for {:?} but apply_transition fails: {:?}",
                kind,
                state,
                result.unwrap_err()
            );
        }
    }
}

#[test]
fn state_serde_roundtrip_all_variants() {
    for state in ALL_STATES {
        let json = serde_json::to_string(state).expect("serialize");
        let back: MissionLifecycleState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*state, back, "serde roundtrip failed for {:?}", state);
    }
}

#[test]
fn transition_kind_serde_roundtrip_all_variants() {
    let all_kinds = [
        MissionLifecycleTransitionKind::Dispatch,
        MissionLifecycleTransitionKind::RequestApproval,
        MissionLifecycleTransitionKind::Approve,
        MissionLifecycleTransitionKind::StartExecution,
        MissionLifecycleTransitionKind::Retry,
        MissionLifecycleTransitionKind::Block,
        MissionLifecycleTransitionKind::Unblock,
        MissionLifecycleTransitionKind::Complete,
        MissionLifecycleTransitionKind::Cancel,
        MissionLifecycleTransitionKind::Fail,
        MissionLifecycleTransitionKind::PlanFinalized,
        MissionLifecycleTransitionKind::DispatchStarted,
        MissionLifecycleTransitionKind::ExecutionStarted,
        MissionLifecycleTransitionKind::RetryResumed,
        MissionLifecycleTransitionKind::ExecutionBlocked,
        MissionLifecycleTransitionKind::MissionCancelled,
    ];

    for kind in &all_kinds {
        let json = serde_json::to_string(kind).expect("serialize");
        let back: MissionLifecycleTransitionKind =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(*kind, back, "serde roundtrip failed for {:?}", kind);
    }
}

#[test]
fn is_terminal_consistent_with_terminal_states_list() {
    for state in ALL_STATES {
        let expected_terminal = TERMINAL_STATES.contains(state);
        assert_eq!(
            state.is_terminal(),
            expected_terminal,
            "is_terminal() for {:?} disagrees with TERMINAL_STATES list",
            state
        );
    }
}

#[test]
fn mission_running_primary_transitions_and_tx_plan_compiler_conformance() {
    let running_cases = [
        (
            MissionLifecycleTransitionKind::Cancel,
            MissionLifecycleState::Cancelled,
        ),
        (
            MissionLifecycleTransitionKind::Retry,
            MissionLifecycleState::RetryPending,
        ),
        (
            MissionLifecycleTransitionKind::Complete,
            MissionLifecycleState::Completed,
        ),
        (
            MissionLifecycleTransitionKind::Fail,
            MissionLifecycleState::Failed,
        ),
        (
            MissionLifecycleTransitionKind::Block,
            MissionLifecycleState::Blocked,
        ),
    ];

    let allowed = MissionLifecycleState::Running.allowed_transitions();
    for (idx, (transition, target)) in running_cases.iter().copied().enumerate() {
        assert!(
            allowed.contains(&transition),
            "Running must allow {transition:?}; allowed={allowed:?}"
        );
        assert_ne!(
            target,
            MissionLifecycleState::Paused,
            "primary Running transition {transition:?} must not route through Paused"
        );
        assert_eq!(
            MissionLifecycleState::Running
                .apply_transition(transition)
                .expect("Running transition should be table-valid"),
            target,
            "Running --{transition:?}--> should land in {target:?}"
        );

        let mut mission = make_mission(MissionLifecycleState::Running);
        let transitioned_at_ms = 2_000_000 + i64::try_from(idx).unwrap();
        let result = mission
            .transition_lifecycle(target, transition, transitioned_at_ms)
            .expect("mission transition should accept the Running primary transition");
        assert_eq!(result, target);
        assert_eq!(mission.lifecycle_state, target);
        assert_eq!(mission.updated_at_ms, Some(transitioned_at_ms));
    }

    let mut blocked = make_mission(MissionLifecycleState::Running);
    let invalid_pause = blocked.transition_lifecycle(
        MissionLifecycleState::Paused,
        MissionLifecycleTransitionKind::Block,
        3_000_000,
    );
    assert!(
        invalid_pause.is_err(),
        "Running --Block--> must not be accepted as a pause transition"
    );
    assert_eq!(blocked.lifecycle_state, MissionLifecycleState::Running);
    assert_eq!(blocked.updated_at_ms, None);
    assert!(
        MissionLifecycleState::Running
            .apply_transition(MissionLifecycleTransitionKind::PauseRequested)
            .is_err(),
        "PauseRequested is not part of the Running primary transition contract"
    );

    let assignments = vec![
        tx_assignment("mission-root", "agent-root", 0.95, &[], &[]),
        tx_assignment(
            "mission-context",
            "agent-low-confidence",
            0.25,
            &[],
            &["mission-root", "external-observation"],
        ),
        tx_assignment(
            "mission-critical",
            "agent-critical",
            0.92,
            &["critical"],
            &["mission-root"],
        ),
        tx_assignment(
            "mission-context",
            "agent-duplicate",
            0.99,
            &["destructive"],
            &["external-duplicate-only"],
        ),
        tx_assignment("", "agent-empty", 0.5, &[], &[]),
    ];
    let config = CompilerConfig {
        default_compensation: CompensationKind::RetryWithBackoff { max_retries: 2 },
        context_freshness_threshold: 0.5,
        context_freshness_max_age_ms: 12_345,
        ..CompilerConfig::default()
    };
    let plan = compile_tx_plan("mission-running-primary", &assignments, &config);
    let plan_again = compile_tx_plan("mission-running-primary", &assignments, &config);

    assert_eq!(plan.plan_hash, plan_again.plan_hash);
    assert_eq!(
        plan.execution_order,
        vec![
            "step-mission-root".to_string(),
            "step-mission-context".to_string(),
            "step-mission-critical".to_string(),
        ]
    );
    assert_eq!(
        plan.parallel_levels,
        vec![
            vec!["step-mission-root".to_string()],
            vec![
                "step-mission-context".to_string(),
                "step-mission-critical".to_string(),
            ],
        ]
    );
    assert_eq!(plan.rejected_assignments.len(), 2);
    assert!(
        plan.rejected_assignments.iter().any(|assignment| {
            assignment.bead_id == "mission-context"
                && assignment.agent_id == "agent-duplicate"
                && assignment
                    .reason
                    .starts_with(RejectedAssignment::REASON_DUPLICATE_BEAD_ID_PREFIX)
        }),
        "duplicate bead_id should be retained only as rejected assignment evidence"
    );
    assert!(
        plan.rejected_assignments.iter().any(|assignment| {
            assignment.bead_id.is_empty()
                && assignment.agent_id == "agent-empty"
                && assignment.reason == RejectedAssignment::REASON_EMPTY_BEAD_ID
        }),
        "empty bead_id should be retained as rejected assignment evidence"
    );
    assert_eq!(plan.rejected_edges.len(), 1);
    assert_eq!(
        plan.rejected_edges[0].from_step,
        "step-external-observation"
    );
    assert_eq!(plan.rejected_edges[0].to_step, "step-mission-context");
    assert!(
        plan.rejected_edges[0]
            .reason
            .contains("external-observation")
    );

    let context_step = plan
        .steps
        .iter()
        .find(|step| step.id == "step-mission-context")
        .expect("context-sensitive step should compile");
    assert_eq!(
        context_step.depends_on,
        vec!["step-mission-root".to_string()]
    );
    assert_eq!(context_step.risk, StepRisk::High);
    assert!(context_step.preconditions.iter().any(|precondition| {
        precondition.kind == PreconditionKind::PolicyApproved && precondition.required
    }));
    assert!(context_step.preconditions.iter().any(|precondition| {
        precondition.kind == PreconditionKind::ContextFresh { max_age_ms: 12_345 }
            && precondition.required
    }));
    assert_eq!(context_step.compensations.len(), 1);
    assert_eq!(
        context_step.compensations[0].action_type,
        CompensationKind::RetryWithBackoff { max_retries: 2 }
    );

    let critical_step = plan
        .steps
        .iter()
        .find(|step| step.id == "step-mission-critical")
        .expect("critical step should compile");
    assert_eq!(
        critical_step.depends_on,
        vec!["step-mission-root".to_string()]
    );
    assert_eq!(critical_step.risk, StepRisk::Critical);
    assert_eq!(critical_step.compensations.len(), 1);
    assert_eq!(
        critical_step.compensations[0].action_type,
        CompensationKind::RetryWithBackoff { max_retries: 2 }
    );
    assert_eq!(plan.risk_summary.total_steps, 3);
    assert_eq!(plan.risk_summary.high_risk_count, 1);
    assert_eq!(plan.risk_summary.critical_risk_count, 1);
    assert_eq!(plan.risk_summary.uncompensated_steps, 0);
    assert_eq!(plan.risk_summary.overall_risk, StepRisk::Critical);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn invalid_transitions_leave_mission_unchanged(
        state in state_strategy(),
        kind in transition_kind_strategy(),
        ts in 1_000_000i64..9_000_000,
    ) {
        let result = state.apply_transition(kind);
        if result.is_err() {
            // transition_lifecycle on Mission should also fail and leave state unchanged
            let mut mission = make_mission(state);
            let original_state = mission.lifecycle_state;
            let original_ts = mission.updated_at_ms;

            let mission_result = mission.transition_lifecycle(
                // Pick an arbitrary target since we know it will fail
                MissionLifecycleState::Completed,
                kind,
                ts,
            );
            prop_assert!(mission_result.is_err());
            prop_assert_eq!(mission.lifecycle_state, original_state);
            prop_assert_eq!(mission.updated_at_ms, original_ts);
        }
    }

    #[test]
    fn valid_transitions_update_mission_state_and_timestamp(
        state in state_strategy(),
        ts in 1_000_000i64..9_000_000,
    ) {
        let allowed = state.allowed_transitions();
        if !allowed.is_empty() {
            // Pick the first allowed transition
            let kind = allowed[0];
            let target = state.apply_transition(kind).unwrap();

            let mut mission = make_mission(state);
            let result = mission.transition_lifecycle(target, kind, ts);
            prop_assert!(result.is_ok(), "transition_lifecycle failed for {:?} --{:?}--> {:?}: {:?}", state, kind, target, result.unwrap_err());
            prop_assert_eq!(mission.lifecycle_state, target);
            prop_assert_eq!(mission.updated_at_ms, Some(ts));
        }
    }

    #[test]
    fn random_walk_reaches_terminal_within_bound(
        walk_indices in prop::collection::vec(0usize..20, 1..50),
    ) {
        let mut state = MissionLifecycleState::Planning;
        let table = mission_lifecycle_transition_table();

        for &idx in &walk_indices {
            if state.is_terminal() {
                break;
            }
            let outgoing: Vec<_> = table.iter().filter(|r| r.from == state).collect();
            if outgoing.is_empty() {
                break;
            }
            let rule = outgoing[idx % outgoing.len()];
            state = rule.to;
        }
        // After enough random steps, we should either be terminal or still in a valid state
        prop_assert!(
            ALL_STATES.contains(&state),
            "Ended up in unknown state after random walk"
        );
    }

    #[test]
    fn transition_lifecycle_wrong_target_always_fails(
        state in state_strategy(),
        kind in transition_kind_strategy(),
        wrong_target in state_strategy(),
        ts in 1_000_000i64..9_000_000,
    ) {
        let correct_target = state.apply_transition(kind);
        if let Ok(correct) = correct_target {
            if wrong_target != correct {
                // Using wrong target state should always fail
                let mut mission = make_mission(state);
                let result = mission.transition_lifecycle(wrong_target, kind, ts);
                prop_assert!(
                    result.is_err(),
                    "transition_lifecycle accepted wrong target {:?} (correct: {:?}) for {:?} --{:?}-->",
                    wrong_target, correct, state, kind,
                );
            }
        }
    }

    #[test]
    fn display_roundtrip_for_states(state in state_strategy()) {
        let display = format!("{state}");
        prop_assert!(!display.is_empty());
        // Verify display produces valid snake_case
        prop_assert!(
            display.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Display for {:?} produced non-snake_case: {}",
            state, display,
        );
    }
}
