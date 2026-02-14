//! Property-based tests for the `undo` module.
//!
//! Covers `UndoOutcome` serde roundtrips, `UndoExecutionResult` serde
//! roundtrips and field preservation, and `UndoRequest` builder pattern
//! invariants.

use frankenterm_core::undo::{UndoExecutionResult, UndoOutcome, UndoRequest};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_undo_outcome() -> impl Strategy<Value = UndoOutcome> {
    prop_oneof![
        Just(UndoOutcome::Success),
        Just(UndoOutcome::NotApplicable),
        Just(UndoOutcome::Failed),
    ]
}

fn arb_undo_execution_result() -> impl Strategy<Value = UndoExecutionResult> {
    (
        0_i64..100_000, // action_id
        "[a-z_]{3,15}", // strategy
        arb_undo_outcome(),
        "[A-Za-z ]{5,30}",                           // message
        proptest::option::of("[A-Za-z ]{5,30}"),     // guidance
        proptest::option::of("[a-z0-9-]{5,15}"),     // target_workflow_id
        proptest::option::of(0_u64..10_000),         // target_pane_id
        proptest::option::of(0_i64..10_000_000_000), // undone_at
    )
        .prop_map(
            |(
                action_id,
                strategy,
                outcome,
                message,
                guidance,
                target_workflow_id,
                target_pane_id,
                undone_at,
            )| {
                UndoExecutionResult {
                    action_id,
                    strategy,
                    outcome,
                    message,
                    guidance,
                    target_workflow_id,
                    target_pane_id,
                    undone_at,
                }
            },
        )
}

// =========================================================================
// UndoOutcome — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// UndoOutcome serde roundtrip.
    #[test]
    fn prop_outcome_serde(outcome in arb_undo_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: UndoOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, outcome);
    }

    /// UndoOutcome serializes to snake_case.
    #[test]
    fn prop_outcome_snake_case(outcome in arb_undo_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let expected = match outcome {
            UndoOutcome::Success => "\"success\"",
            UndoOutcome::NotApplicable => "\"not_applicable\"",
            UndoOutcome::Failed => "\"failed\"",
        };
        prop_assert_eq!(json.as_str(), expected);
    }

    /// UndoOutcome serde is deterministic.
    #[test]
    fn prop_outcome_serde_deterministic(outcome in arb_undo_outcome()) {
        let j1 = serde_json::to_string(&outcome).unwrap();
        let j2 = serde_json::to_string(&outcome).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// UndoOutcome from invalid string produces a serde error.
    #[test]
    fn prop_outcome_rejects_invalid(bad in "[A-Z]{3,10}") {
        let json = format!("\"{}\"", bad);
        let result = serde_json::from_str::<UndoOutcome>(&json);
        prop_assert!(result.is_err(), "should reject invalid variant: {}", bad);
    }
}

// =========================================================================
// UndoExecutionResult — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// UndoExecutionResult serde roundtrip preserves all fields.
    #[test]
    fn prop_result_serde(result in arb_undo_execution_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.action_id, result.action_id);
        prop_assert_eq!(&back.strategy, &result.strategy);
        prop_assert_eq!(back.outcome, result.outcome);
        prop_assert_eq!(&back.message, &result.message);
        prop_assert_eq!(&back.guidance, &result.guidance);
        prop_assert_eq!(&back.target_workflow_id, &result.target_workflow_id);
        prop_assert_eq!(back.target_pane_id, result.target_pane_id);
        prop_assert_eq!(back.undone_at, result.undone_at);
    }

    /// UndoExecutionResult serde is deterministic.
    #[test]
    fn prop_result_serde_deterministic(result in arb_undo_execution_result()) {
        let j1 = serde_json::to_string(&result).unwrap();
        let j2 = serde_json::to_string(&result).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    /// Pretty-printed JSON also roundtrips correctly.
    #[test]
    fn prop_result_pretty_serde(result in arb_undo_execution_result()) {
        let json = serde_json::to_string_pretty(&result).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.action_id, result.action_id);
        prop_assert_eq!(&back.strategy, &result.strategy);
        prop_assert_eq!(back.outcome, result.outcome);
    }

    /// JSON always contains expected field names.
    #[test]
    fn prop_result_json_has_required_fields(result in arb_undo_execution_result()) {
        let json = serde_json::to_string(&result).unwrap();
        prop_assert!(json.contains("\"action_id\""), "missing action_id field");
        prop_assert!(json.contains("\"strategy\""), "missing strategy field");
        prop_assert!(json.contains("\"outcome\""), "missing outcome field");
        prop_assert!(json.contains("\"message\""), "missing message field");
    }

    /// UndoExecutionResult with all optional fields set roundtrips.
    #[test]
    fn prop_result_all_some(
        action_id in 0_i64..100_000,
        strategy in "[a-z_]{3,15}",
        outcome in arb_undo_outcome(),
        message in "[A-Za-z ]{5,30}",
        guidance in "[A-Za-z ]{5,30}",
        workflow_id in "[a-z0-9-]{5,15}",
        pane_id in 0_u64..10_000,
        undone_at in 0_i64..10_000_000_000,
    ) {
        let result = UndoExecutionResult {
            action_id,
            strategy,
            outcome,
            message,
            guidance: Some(guidance),
            target_workflow_id: Some(workflow_id),
            target_pane_id: Some(pane_id),
            undone_at: Some(undone_at),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        prop_assert!(back.guidance.is_some());
        prop_assert!(back.target_workflow_id.is_some());
        prop_assert!(back.target_pane_id.is_some());
        prop_assert!(back.undone_at.is_some());
    }
}

// =========================================================================
// UndoRequest — builder pattern
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// UndoRequest::new sets default actor to "user".
    #[test]
    fn prop_request_default_actor(action_id in 0_i64..100_000) {
        let req = UndoRequest::new(action_id);
        prop_assert_eq!(req.action_id, action_id);
        prop_assert_eq!(&req.actor, "user");
        prop_assert!(req.reason.is_none());
    }

    /// with_actor overrides the actor field.
    #[test]
    fn prop_request_with_actor(action_id in 0_i64..100_000, actor in "[a-z]{3,15}") {
        let req = UndoRequest::new(action_id).with_actor(actor.clone());
        prop_assert_eq!(&req.actor, &actor);
        prop_assert_eq!(req.action_id, action_id);
    }

    /// with_reason sets the reason field.
    #[test]
    fn prop_request_with_reason(action_id in 0_i64..100_000, reason in "[a-z ]{5,30}") {
        let req = UndoRequest::new(action_id).with_reason(reason.clone());
        prop_assert_eq!(req.reason, Some(reason));
    }

    /// Builder methods are chainable and independent.
    #[test]
    fn prop_request_builder_chain(
        action_id in 0_i64..100_000,
        actor in "[a-z]{3,10}",
        reason in "[a-z ]{5,20}",
    ) {
        let req = UndoRequest::new(action_id)
            .with_actor(actor.clone())
            .with_reason(reason.clone());
        prop_assert_eq!(req.action_id, action_id);
        prop_assert_eq!(&req.actor, &actor);
        prop_assert_eq!(req.reason, Some(reason));
    }

    /// Calling with_actor twice keeps the last value.
    #[test]
    fn prop_request_actor_last_wins(
        action_id in 0_i64..100_000,
        actor1 in "[a-z]{3,10}",
        actor2 in "[a-z]{3,10}",
    ) {
        let req = UndoRequest::new(action_id)
            .with_actor(actor1)
            .with_actor(actor2.clone());
        prop_assert_eq!(&req.actor, &actor2);
    }

    /// Calling with_reason twice keeps the last value.
    #[test]
    fn prop_request_reason_last_wins(
        action_id in 0_i64..100_000,
        reason1 in "[a-z ]{5,20}",
        reason2 in "[a-z ]{5,20}",
    ) {
        let req = UndoRequest::new(action_id)
            .with_reason(reason1)
            .with_reason(reason2.clone());
        prop_assert_eq!(req.reason, Some(reason2));
    }

    /// with_actor does not affect reason, and vice versa.
    #[test]
    fn prop_request_builder_independence(
        action_id in 0_i64..100_000,
        actor in "[a-z]{3,10}",
        reason in "[a-z ]{5,20}",
    ) {
        let req_actor_only = UndoRequest::new(action_id).with_actor(actor.clone());
        prop_assert!(req_actor_only.reason.is_none(), "actor should not set reason");

        let req_reason_only = UndoRequest::new(action_id).with_reason(reason.clone());
        prop_assert_eq!(&req_reason_only.actor, "user", "reason should not change actor");
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn outcome_all_variants_distinct() {
    assert_ne!(UndoOutcome::Success, UndoOutcome::NotApplicable);
    assert_ne!(UndoOutcome::Success, UndoOutcome::Failed);
    assert_ne!(UndoOutcome::NotApplicable, UndoOutcome::Failed);
}

#[test]
fn request_default_values() {
    let req = UndoRequest::new(42);
    assert_eq!(req.action_id, 42);
    assert_eq!(req.actor, "user");
    assert!(req.reason.is_none());
}

#[test]
fn result_with_none_optionals() {
    let result = UndoExecutionResult {
        action_id: 1,
        strategy: "none".to_string(),
        outcome: UndoOutcome::NotApplicable,
        message: "not found".to_string(),
        guidance: None,
        target_workflow_id: None,
        target_pane_id: None,
        undone_at: None,
    };
    let json = serde_json::to_string(&result).unwrap();
    let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
    assert!(back.guidance.is_none());
    assert!(back.target_workflow_id.is_none());
    assert!(back.target_pane_id.is_none());
    assert!(back.undone_at.is_none());
}
