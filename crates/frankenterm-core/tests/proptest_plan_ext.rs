//! Extended property-based tests for plan module.
//!
//! Supplements proptest_plan.rs with coverage for:
//! - MissionLifecycleState transition table completeness
//! - Terminal states have no outgoing transitions
//! - apply_transition matches transition table
//! - MissionFailureCode reason_code/error_code prefix conventions
//! - terminality/retryability consistency
//! - ApprovalState canonical_string determinism
//! - Outcome serde roundtrip
//! - MissionLifecycleState serde roundtrip
//! - MissionLifecycleTransitionKind serde roundtrip
//! - MissionFailureCode serde roundtrip

use proptest::prelude::*;

use frankenterm_core::plan::{
    ApprovalState, MissionFailureCode, MissionFailureRetryability, MissionFailureTerminality,
    MissionLifecycleState, MissionLifecycleTransitionKind, Outcome,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_lifecycle_state() -> impl Strategy<Value = MissionLifecycleState> {
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

fn arb_transition_kind() -> impl Strategy<Value = MissionLifecycleTransitionKind> {
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

fn arb_failure_code() -> impl Strategy<Value = MissionFailureCode> {
    prop_oneof![
        Just(MissionFailureCode::PolicyDenied),
        Just(MissionFailureCode::ReservationConflict),
        Just(MissionFailureCode::RateLimited),
        Just(MissionFailureCode::StaleState),
        Just(MissionFailureCode::DispatchError),
        Just(MissionFailureCode::ApprovalRequired),
        Just(MissionFailureCode::ApprovalDenied),
        Just(MissionFailureCode::ApprovalExpired),
        Just(MissionFailureCode::KillSwitchActivated),
    ]
}

// =============================================================================
// MissionLifecycleState transition properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Terminal states (Completed, Cancelled, Failed) have no allowed transitions
    #[test]
    fn terminal_states_have_no_transitions(state in arb_lifecycle_state()) {
        if state.is_terminal() {
            let transitions = state.allowed_transitions();
            prop_assert!(transitions.is_empty(),
                "{:?} is terminal but has transitions: {:?}", state, transitions);
        }
    }

    /// Non-terminal states have at least one allowed transition
    #[test]
    fn non_terminal_states_have_transitions(state in arb_lifecycle_state()) {
        if !state.is_terminal() {
            let transitions = state.allowed_transitions();
            prop_assert!(!transitions.is_empty(),
                "{:?} is non-terminal but has no transitions", state);
        }
    }

    /// apply_transition succeeds iff the transition is in allowed_transitions
    #[test]
    fn apply_transition_consistent_with_allowed(
        state in arb_lifecycle_state(),
        transition in arb_transition_kind(),
    ) {
        let allowed = state.allowed_transitions();
        let result = state.apply_transition(transition);

        if allowed.contains(&transition) {
            prop_assert!(result.is_ok(),
                "{:?} --{:?}--> should succeed since it's in allowed_transitions", state, transition);
        } else {
            prop_assert!(result.is_err(),
                "{:?} --{:?}--> should fail since it's not in allowed_transitions", state, transition);
        }
    }

    /// apply_transition target matches the transition table
    #[test]
    fn apply_transition_reaches_correct_target(
        state in arb_lifecycle_state(),
        transition in arb_transition_kind(),
    ) {
        if let Ok(next_state) = state.apply_transition(transition) {
            // Verify against the table directly
            let table = MissionLifecycleState::transition_table();
            let table_entry = table.iter().find(|r| r.from == state && r.via == transition);
            prop_assert!(table_entry.is_some(),
                "apply_transition succeeded but no table entry for {:?} --{:?}-->", state, transition);
            prop_assert_eq!(table_entry.unwrap().to, next_state,
                "apply_transition target mismatch for {:?} --{:?}-->", state, transition);
        }
    }

    /// is_terminal only for Completed, Cancelled, Failed
    #[test]
    fn is_terminal_correct(state in arb_lifecycle_state()) {
        let expected = matches!(state,
            MissionLifecycleState::Completed | MissionLifecycleState::Cancelled | MissionLifecycleState::Failed);
        prop_assert_eq!(state.is_terminal(), expected,
            "{:?}.is_terminal() should be {}", state, expected);
    }
}

// =============================================================================
// MissionLifecycleState serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn lifecycle_state_serde_roundtrip(state in arb_lifecycle_state()) {
        let json = serde_json::to_string(&state).unwrap();
        let restored: MissionLifecycleState = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(state, restored);
    }

    #[test]
    fn transition_kind_serde_roundtrip(kind in arb_transition_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let restored: MissionLifecycleTransitionKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, restored);
    }

    /// Display is non-empty for all lifecycle states
    #[test]
    fn lifecycle_state_display_non_empty(state in arb_lifecycle_state()) {
        let s = state.to_string();
        prop_assert!(!s.is_empty());
    }

    /// Display is non-empty for all transition kinds
    #[test]
    fn transition_kind_display_non_empty(kind in arb_transition_kind()) {
        let s = kind.to_string();
        prop_assert!(!s.is_empty());
    }
}

// =============================================================================
// MissionFailureCode properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// reason_code always starts with "mission."
    #[test]
    fn failure_code_reason_prefix(code in arb_failure_code()) {
        let reason = code.reason_code();
        prop_assert!(reason.starts_with("mission."),
            "{:?}.reason_code() = '{}' should start with 'mission.'", code, reason);
    }

    /// error_code always starts with "robot.mission_"
    #[test]
    fn failure_code_error_prefix(code in arb_failure_code()) {
        let error = code.error_code();
        prop_assert!(error.starts_with("robot.mission_"),
            "{:?}.error_code() = '{}' should start with 'robot.mission_'", code, error);
    }

    /// Terminal failures are not retryable
    #[test]
    fn terminal_failure_not_retryable(code in arb_failure_code()) {
        if code.terminality() == MissionFailureTerminality::Terminal {
            prop_assert_eq!(code.retryability(), MissionFailureRetryability::NotRetryable,
                "Terminal failure {:?} should not be retryable", code);
        }
    }

    /// human_hint is non-empty
    #[test]
    fn failure_code_human_hint_non_empty(code in arb_failure_code()) {
        let hint = code.human_hint();
        prop_assert!(!hint.is_empty());
    }

    /// serde roundtrip
    #[test]
    fn failure_code_serde_roundtrip(code in arb_failure_code()) {
        let json = serde_json::to_string(&code).unwrap();
        let restored: MissionFailureCode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(code, restored);
    }
}

// =============================================================================
// ApprovalState properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// canonical_string is deterministic (same input → same output)
    #[test]
    fn approval_not_required_canonical_deterministic(_dummy in 0u8..1) {
        let state = ApprovalState::NotRequired;
        prop_assert_eq!(state.canonical_string(), state.canonical_string());
        prop_assert_eq!(&state.canonical_string(), "not_required");
    }

    #[test]
    fn approval_pending_canonical_contains_requester(
        requester in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
    ) {
        let state = ApprovalState::Pending {
            requested_by: requester.clone(),
            requested_at_ms: ts,
        };
        let canonical = state.canonical_string();
        prop_assert!(canonical.starts_with("pending:"));
        prop_assert!(canonical.contains(&requester));
    }

    #[test]
    fn approval_approved_canonical_contains_approver(
        approver in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
    ) {
        let state = ApprovalState::Approved {
            approved_by: approver.clone(),
            approved_at_ms: ts,
            approval_code_hash: "abc123".to_string(),
        };
        let canonical = state.canonical_string();
        prop_assert!(canonical.starts_with("approved:"));
        prop_assert!(canonical.contains(&approver));
    }

    #[test]
    fn approval_denied_canonical_contains_denier(
        denier in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
        reason in "[a-z]+",
    ) {
        let state = ApprovalState::Denied {
            denied_by: denier.clone(),
            denied_at_ms: ts,
            reason_code: reason.clone(),
        };
        let canonical = state.canonical_string();
        prop_assert!(canonical.starts_with("denied:"));
        prop_assert!(canonical.contains(&denier));
        prop_assert!(canonical.contains(&reason));
    }

    #[test]
    fn approval_expired_canonical(
        ts in 1000i64..i64::MAX / 2,
        reason in "[a-z]+",
    ) {
        let state = ApprovalState::Expired {
            expired_at_ms: ts,
            reason_code: reason.clone(),
        };
        let canonical = state.canonical_string();
        prop_assert!(canonical.starts_with("expired:"));
        prop_assert!(canonical.contains(&reason));
    }
}

// =============================================================================
// Outcome serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn outcome_success_serde_roundtrip(
        reason in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
    ) {
        let outcome = Outcome::Success {
            reason_code: reason,
            completed_at_ms: ts,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: Outcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, restored);
    }

    #[test]
    fn outcome_failed_serde_roundtrip(
        reason in "[a-z]+",
        error in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
    ) {
        let outcome = Outcome::Failed {
            reason_code: reason,
            error_code: error,
            completed_at_ms: ts,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: Outcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, restored);
    }

    #[test]
    fn outcome_cancelled_serde_roundtrip(
        reason in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
    ) {
        let outcome = Outcome::Cancelled {
            reason_code: reason,
            completed_at_ms: ts,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: Outcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(outcome, restored);
    }

    /// Outcome canonical_string is deterministic
    #[test]
    fn outcome_canonical_deterministic(
        reason in "[a-z]+",
        ts in 1000i64..i64::MAX / 2,
    ) {
        let outcome = Outcome::Success {
            reason_code: reason,
            completed_at_ms: ts,
        };
        prop_assert_eq!(outcome.canonical_string(), outcome.canonical_string());
    }
}

// =============================================================================
// Transition table structural properties
// =============================================================================

#[test]
fn transition_table_has_no_transitions_from_terminal() {
    let table = MissionLifecycleState::transition_table();
    let terminal_sources: Vec<_> = table
        .iter()
        .filter(|r| r.from.is_terminal())
        .collect();
    assert!(
        terminal_sources.is_empty(),
        "Transition table should have no rows from terminal states, found: {:?}",
        terminal_sources
    );
}

#[test]
fn transition_table_every_from_state_appears() {
    let table = MissionLifecycleState::transition_table();
    let all_states = [
        MissionLifecycleState::Planned,
        MissionLifecycleState::Planning,
        MissionLifecycleState::Dispatching,
        MissionLifecycleState::AwaitingApproval,
        MissionLifecycleState::Running,
        MissionLifecycleState::Executing,
        MissionLifecycleState::RetryPending,
        MissionLifecycleState::Blocked,
        MissionLifecycleState::Paused,
    ];

    for state in &all_states {
        let has_transition = table.iter().any(|r| r.from == *state);
        assert!(
            has_transition,
            "Non-terminal state {:?} should appear as 'from' in transition table",
            state
        );
    }
}

#[test]
fn default_lifecycle_state_is_planning() {
    assert_eq!(MissionLifecycleState::default(), MissionLifecycleState::Planning);
}
