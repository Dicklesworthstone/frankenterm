//! Property-based tests for mission robot types (ft-1i2ge.5.2).

use proptest::prelude::*;

use frankenterm_core::plan::{ApprovalState, MissionActorRole, MissionLifecycleState, Outcome};
use frankenterm_core::robot_types::{
    MissionActionState, MissionAgentState, MissionAssignmentCounters, MissionAssignmentData,
    MissionDecisionData, MissionDecisionsData, MissionFailureCatalogEntry, MissionRunState,
    MissionStateData, MissionStateFilters, MissionTransitionInfo, RobotResponse,
};

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_run_state() -> impl Strategy<Value = MissionRunState> {
    prop_oneof![
        Just(MissionRunState::Pending),
        Just(MissionRunState::Succeeded),
        Just(MissionRunState::Failed),
        Just(MissionRunState::Cancelled),
    ]
}

fn arb_agent_state() -> impl Strategy<Value = MissionAgentState> {
    prop_oneof![
        Just(MissionAgentState::NotRequired),
        Just(MissionAgentState::Pending),
        Just(MissionAgentState::Approved),
        Just(MissionAgentState::Denied),
        Just(MissionAgentState::Expired),
    ]
}

fn arb_action_state() -> impl Strategy<Value = MissionActionState> {
    prop_oneof![
        Just(MissionActionState::Ready),
        Just(MissionActionState::Blocked),
        Just(MissionActionState::Completed),
    ]
}

fn arb_lifecycle_state() -> impl Strategy<Value = MissionLifecycleState> {
    prop_oneof![
        Just(MissionLifecycleState::Planning),
        Just(MissionLifecycleState::Planned),
        Just(MissionLifecycleState::Dispatching),
        Just(MissionLifecycleState::AwaitingApproval),
        Just(MissionLifecycleState::Running),
        Just(MissionLifecycleState::Paused),
        Just(MissionLifecycleState::RetryPending),
        Just(MissionLifecycleState::Blocked),
        Just(MissionLifecycleState::Completed),
        Just(MissionLifecycleState::Failed),
        Just(MissionLifecycleState::Cancelled),
    ]
}

fn arb_actor_role() -> impl Strategy<Value = MissionActorRole> {
    prop_oneof![
        Just(MissionActorRole::Planner),
        Just(MissionActorRole::Dispatcher),
        Just(MissionActorRole::Operator),
    ]
}

fn arb_approval_state() -> impl Strategy<Value = ApprovalState> {
    prop_oneof![
        Just(ApprovalState::NotRequired),
        (".*", any::<i64>()).prop_map(|(by, at)| ApprovalState::Pending {
            requested_by: by,
            requested_at_ms: at,
        }),
        (".*", any::<i64>(), ".*").prop_map(|(by, at, hash)| ApprovalState::Approved {
            approved_by: by,
            approved_at_ms: at,
            approval_code_hash: hash,
        }),
        (".*", any::<i64>(), ".*").prop_map(|(by, at, rc)| ApprovalState::Denied {
            denied_by: by,
            denied_at_ms: at,
            reason_code: rc,
        }),
        (any::<i64>(), ".*").prop_map(|(at, rc)| ApprovalState::Expired {
            expired_at_ms: at,
            reason_code: rc,
        }),
    ]
}

fn arb_outcome() -> impl Strategy<Value = Outcome> {
    prop_oneof![
        ("[a-z_]{3,12}", any::<i64>()).prop_map(|(rc, at)| Outcome::Success {
            reason_code: rc,
            completed_at_ms: at,
        }),
        ("[a-z_]{3,12}", "[A-Z]{2}-[0-9]{4}", any::<i64>()).prop_map(|(rc, ec, at)| {
            Outcome::Failed {
                reason_code: rc,
                error_code: ec,
                completed_at_ms: at,
            }
        }),
        ("[a-z_]{3,12}", any::<i64>()).prop_map(|(rc, at)| Outcome::Cancelled {
            reason_code: rc,
            completed_at_ms: at,
        }),
    ]
}

fn arb_filters() -> impl Strategy<Value = MissionStateFilters> {
    (
        proptest::option::of(arb_lifecycle_state()),
        proptest::option::of(arb_run_state()),
        proptest::option::of(arb_agent_state()),
        proptest::option::of(arb_action_state()),
        proptest::option::of("[a-z0-9-]{4,16}"),
        proptest::option::of("[a-z]{3,10}"),
        1..200usize,
    )
        .prop_map(
            |(mission_state, run_state, agent_state, action_state, assignment_id, assignee, limit)| {
                MissionStateFilters {
                    mission_state,
                    run_state,
                    agent_state,
                    action_state,
                    assignment_id,
                    assignee,
                    limit,
                }
            },
        )
}

fn arb_counters() -> impl Strategy<Value = MissionAssignmentCounters> {
    (
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
        0..100usize,
    )
        .prop_map(
            |(pending_approval, approved, denied, expired, succeeded, failed, cancelled, unresolved)| {
                MissionAssignmentCounters {
                    pending_approval,
                    approved,
                    denied,
                    expired,
                    succeeded,
                    failed,
                    cancelled,
                    unresolved,
                }
            },
        )
}

fn arb_assignment() -> impl Strategy<Value = MissionAssignmentData> {
    (
        "[a-z0-9-]{4,12}",
        "[a-z0-9-]{4,12}",
        "[a-z]{3,10}",
        arb_actor_role(),
        "[a-z_]{3,12}",
        arb_run_state(),
        arb_agent_state(),
        arb_action_state(),
        arb_approval_state(),
        proptest::option::of(arb_outcome()),
        proptest::option::of("[a-z_]{3,12}"),
        proptest::option::of("[A-Z]{2}-[0-9]{4}"),
    )
        .prop_map(
            |(
                assignment_id,
                candidate_id,
                assignee,
                assigned_by,
                action_type,
                run_state,
                agent_state,
                action_state,
                approval_state,
                outcome,
                reason_code,
                error_code,
            )| {
                MissionAssignmentData {
                    assignment_id,
                    candidate_id,
                    assignee,
                    assigned_by,
                    action_type,
                    run_state,
                    agent_state,
                    action_state,
                    approval_state,
                    outcome,
                    reason_code,
                    error_code,
                }
            },
        )
}

fn arb_transition() -> impl Strategy<Value = MissionTransitionInfo> {
    ("[a-z_]{3,12}", "[a-z_]{3,12}").prop_map(|(kind, to)| MissionTransitionInfo { kind, to })
}

fn arb_failure_entry() -> impl Strategy<Value = MissionFailureCatalogEntry> {
    (
        "[a-z_]{4,15}",
        "[A-Z]{2}-[0-9]{4}",
        prop_oneof![Just("terminal".to_string()), Just("transient".to_string())],
        prop_oneof![Just("retryable".to_string()), Just("non_retryable".to_string())],
        "[a-z ]{5,25}",
        "[a-z_]{5,25}",
    )
        .prop_map(
            |(reason_code, error_code, terminality, retryability, human_hint, machine_hint)| {
                MissionFailureCatalogEntry {
                    reason_code,
                    error_code,
                    terminality,
                    retryability,
                    human_hint,
                    machine_hint,
                }
            },
        )
}

// ---------------------------------------------------------------------------
// MRA-1: MissionRunState serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_1_run_state_serde(state in arb_run_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionRunState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// ---------------------------------------------------------------------------
// MRA-2: MissionAgentState serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_2_agent_state_serde(state in arb_agent_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionAgentState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// ---------------------------------------------------------------------------
// MRA-3: MissionActionState serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_3_action_state_serde(state in arb_action_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let back: MissionActionState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, state);
    }
}

// ---------------------------------------------------------------------------
// MRA-4: MissionStateFilters with None fields omits them in JSON
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_4_filters_none_omitted(limit in 1..100usize) {
        let filters = MissionStateFilters {
            mission_state: None,
            run_state: None,
            agent_state: None,
            action_state: None,
            assignment_id: None,
            assignee: None,
            limit,
        };
        let json = serde_json::to_string(&filters).unwrap();
        prop_assert!(!json.contains("mission_state"));
        prop_assert!(!json.contains("run_state"));
        prop_assert!(!json.contains("agent_state"));
        prop_assert!(!json.contains("action_state"));
        prop_assert!(!json.contains("assignment_id"));
        prop_assert!(!json.contains("assignee"));
        let expected = format!("\"limit\":{}", limit);
        prop_assert!(json.contains(&expected));
    }
}

// ---------------------------------------------------------------------------
// MRA-5: MissionStateFilters serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn mra_5_filters_serde(filters in arb_filters()) {
        let json = serde_json::to_string(&filters).unwrap();
        let back: MissionStateFilters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.limit, filters.limit);
        prop_assert_eq!(back.run_state, filters.run_state);
        prop_assert_eq!(back.agent_state, filters.agent_state);
        prop_assert_eq!(back.action_state, filters.action_state);
        prop_assert_eq!(back.assignment_id, filters.assignment_id);
        prop_assert_eq!(back.assignee, filters.assignee);
    }
}

// ---------------------------------------------------------------------------
// MRA-6: MissionAssignmentCounters default is all zeroes
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn mra_6_counters_default_zero(_x in Just(0)) {
        let c = MissionAssignmentCounters::default();
        prop_assert_eq!(c.pending_approval, 0);
        prop_assert_eq!(c.approved, 0);
        prop_assert_eq!(c.denied, 0);
        prop_assert_eq!(c.expired, 0);
        prop_assert_eq!(c.succeeded, 0);
        prop_assert_eq!(c.failed, 0);
        prop_assert_eq!(c.cancelled, 0);
        prop_assert_eq!(c.unresolved, 0);
    }
}

// ---------------------------------------------------------------------------
// MRA-7: MissionAssignmentCounters serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_7_counters_serde(counters in arb_counters()) {
        let json = serde_json::to_string(&counters).unwrap();
        let back: MissionAssignmentCounters = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pending_approval, counters.pending_approval);
        prop_assert_eq!(back.approved, counters.approved);
        prop_assert_eq!(back.denied, counters.denied);
        prop_assert_eq!(back.expired, counters.expired);
        prop_assert_eq!(back.succeeded, counters.succeeded);
        prop_assert_eq!(back.failed, counters.failed);
        prop_assert_eq!(back.cancelled, counters.cancelled);
        prop_assert_eq!(back.unresolved, counters.unresolved);
    }
}

// ---------------------------------------------------------------------------
// MRA-8: MissionTransitionInfo serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_8_transition_serde(info in arb_transition()) {
        let json = serde_json::to_string(&info).unwrap();
        let back: MissionTransitionInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.kind, &info.kind);
        prop_assert_eq!(&back.to, &info.to);
    }
}

// ---------------------------------------------------------------------------
// MRA-9: MissionAssignmentData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_9_assignment_serde(assignment in arb_assignment()) {
        let json = serde_json::to_string(&assignment).unwrap();
        let back: MissionAssignmentData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.assignment_id, &assignment.assignment_id);
        prop_assert_eq!(&back.candidate_id, &assignment.candidate_id);
        prop_assert_eq!(&back.assignee, &assignment.assignee);
        prop_assert_eq!(back.run_state, assignment.run_state);
        prop_assert_eq!(back.agent_state, assignment.agent_state);
        prop_assert_eq!(back.action_state, assignment.action_state);
    }
}

// ---------------------------------------------------------------------------
// MRA-10: MissionFailureCatalogEntry serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_10_failure_entry_serde(entry in arb_failure_entry()) {
        let json = serde_json::to_string(&entry).unwrap();
        let back: MissionFailureCatalogEntry = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.reason_code, &entry.reason_code);
        prop_assert_eq!(&back.error_code, &entry.error_code);
        prop_assert_eq!(&back.terminality, &entry.terminality);
        prop_assert_eq!(&back.retryability, &entry.retryability);
    }
}

// ---------------------------------------------------------------------------
// MRA-11: MissionStateData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_11_state_data_serde(
        lifecycle in arb_lifecycle_state(),
        filters in arb_filters(),
        counters in arb_counters(),
        transitions in proptest::collection::vec(arb_transition(), 0..4),
        assignments in proptest::collection::vec(arb_assignment(), 0..4),
    ) {
        let data = MissionStateData {
            mission_file: "active.json".to_string(),
            mission_id: "m-test".to_string(),
            title: "Test Mission".to_string(),
            mission_hash: "abc123".to_string(),
            lifecycle_state: lifecycle,
            mission_matches_filter: true,
            candidate_count: assignments.len(),
            assignment_count: assignments.len(),
            matched_assignment_count: assignments.len(),
            returned_assignment_count: assignments.len(),
            filters,
            assignment_counters: counters,
            available_transitions: transitions,
            assignments,
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionStateData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.mission_id, &data.mission_id);
        prop_assert_eq!(back.lifecycle_state, data.lifecycle_state);
        prop_assert_eq!(back.assignments.len(), data.assignments.len());
        prop_assert_eq!(back.available_transitions.len(), data.available_transitions.len());
    }
}

// ---------------------------------------------------------------------------
// MRA-12: MissionDecisionsData serde roundtrip
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_12_decisions_data_serde(
        lifecycle in arb_lifecycle_state(),
        filters in arb_filters(),
        transitions in proptest::collection::vec(arb_transition(), 0..3),
        catalog in proptest::collection::vec(arb_failure_entry(), 0..3),
    ) {
        let data = MissionDecisionsData {
            mission_file: "active.json".to_string(),
            mission_id: "m-dec".to_string(),
            title: "Decision Test".to_string(),
            mission_hash: "dec456".to_string(),
            lifecycle_state: lifecycle,
            mission_matches_filter: true,
            candidate_count: 0,
            assignment_count: 0,
            matched_assignment_count: 0,
            returned_assignment_count: 0,
            filters,
            available_transitions: transitions,
            failure_catalog: catalog,
            decisions: vec![],
        };
        let json = serde_json::to_string(&data).unwrap();
        let back: MissionDecisionsData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.mission_id, &data.mission_id);
        prop_assert_eq!(back.lifecycle_state, data.lifecycle_state);
        prop_assert_eq!(back.failure_catalog.len(), data.failure_catalog.len());
    }
}

// ---------------------------------------------------------------------------
// MRA-13: MissionStateData wraps in RobotResponse envelope
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_13_state_in_envelope(lifecycle in arb_lifecycle_state()) {
        let data = MissionStateData {
            mission_file: "active.json".to_string(),
            mission_id: "m-env".to_string(),
            title: "Envelope".to_string(),
            mission_hash: "env789".to_string(),
            lifecycle_state: lifecycle,
            mission_matches_filter: true,
            candidate_count: 0,
            assignment_count: 0,
            matched_assignment_count: 0,
            returned_assignment_count: 0,
            filters: MissionStateFilters {
                mission_state: None,
                run_state: None,
                agent_state: None,
                action_state: None,
                assignment_id: None,
                assignee: None,
                limit: 50,
            },
            assignment_counters: MissionAssignmentCounters::default(),
            available_transitions: vec![],
            assignments: vec![],
        };
        let resp = RobotResponse::success(data, 1);
        let json = serde_json::to_string(&resp).unwrap();
        prop_assert!(json.contains("\"ok\":true"));
        prop_assert!(json.contains("m-env"));
        let back: RobotResponse<MissionStateData> = serde_json::from_str(&json).unwrap();
        prop_assert!(back.ok);
        let inner = back.data.unwrap();
        prop_assert_eq!(&inner.mission_id, "m-env");
        prop_assert_eq!(inner.lifecycle_state, lifecycle);
    }
}

// ---------------------------------------------------------------------------
// MRA-14: MissionDecisionData with optional dispatch fields
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_14_decision_optional_fields(
        assignment in arb_assignment(),
        has_error in any::<bool>(),
    ) {
        let decision = MissionDecisionData {
            assignment,
            candidate_action: None,
            dispatch_contract: None,
            dispatch_target: None,
            dry_run_execution: None,
            decision_error: if has_error {
                Some("test_error".to_string())
            } else {
                None
            },
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: MissionDecisionData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.decision_error, decision.decision_error);
        // Optional fields should be omitted when None
        if !has_error {
            prop_assert!(!json.contains("decision_error"));
        }
    }
}

// ---------------------------------------------------------------------------
// MRA-15: All MissionRunState variants use snake_case
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_15_run_state_snake_case(state in arb_run_state()) {
        let json = serde_json::to_string(&state).unwrap();
        // Should be lowercase, no CamelCase
        let unquoted = json.trim_matches('"');
        prop_assert_eq!(unquoted, unquoted.to_lowercase(), "should be snake_case");
    }
}

// ---------------------------------------------------------------------------
// MRA-16: All MissionAgentState variants use snake_case
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn mra_16_agent_state_snake_case(state in arb_agent_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let unquoted = json.trim_matches('"');
        prop_assert_eq!(unquoted, unquoted.to_lowercase(), "should be snake_case");
    }
}
