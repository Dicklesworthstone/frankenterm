// Property-based tests for workflows/step_results and workflows/engine modules.
//
// Covers: serde roundtrips for StepResult, TextMatch, WaitCondition,
// WorkflowExecution, ExecutionStatus, WorkflowStepPolicyDecision.
// Also covers structural invariants and helper method contracts.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::workflows::{
    ExecutionStatus, StepResult, TextMatch, WaitCondition, WorkflowExecution,
    WorkflowStepPolicyDecision,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_text_match() -> impl Strategy<Value = TextMatch> {
    prop_oneof![
        "[a-z ]{3,20}".prop_map(|value| TextMatch::Substring { value }),
        "[a-z]+".prop_map(|pattern| TextMatch::Regex { pattern }),
    ]
}

fn arb_wait_condition() -> impl Strategy<Value = WaitCondition> {
    prop_oneof![
        (prop::option::of(0u64..10_000), "[a-z_.]{3,15}")
            .prop_map(|(pane_id, rule_id)| WaitCondition::Pattern { pane_id, rule_id }),
        (prop::option::of(0u64..10_000), 100u64..30_000).prop_map(
            |(pane_id, idle_threshold_ms)| WaitCondition::PaneIdle {
                pane_id,
                idle_threshold_ms,
            }
        ),
        (prop::option::of(0u64..10_000), 100u64..30_000).prop_map(|(pane_id, stable_for_ms)| {
            WaitCondition::StableTail {
                pane_id,
                stable_for_ms,
            }
        }),
        (prop::option::of(0u64..10_000), arb_text_match())
            .prop_map(|(pane_id, matcher)| WaitCondition::TextMatch { pane_id, matcher }),
        (100u64..30_000).prop_map(|duration_ms| WaitCondition::Sleep { duration_ms }),
        "[a-z_]{3,15}".prop_map(|key| WaitCondition::External { key }),
    ]
}

fn arb_step_result() -> impl Strategy<Value = StepResult> {
    prop_oneof![
        Just(StepResult::Continue),
        Just(StepResult::done_empty()),
        (100u64..30_000).prop_map(|delay_ms| StepResult::Retry { delay_ms }),
        "[a-z ]{5,30}".prop_map(|reason| StepResult::Abort { reason }),
        (arb_wait_condition(), prop::option::of(1000u64..120_000)).prop_map(
            |(condition, timeout_ms)| StepResult::WaitFor {
                condition,
                timeout_ms,
            }
        ),
        (
            "[a-z ]{3,30}",
            prop::option::of(arb_wait_condition()),
            prop::option::of(1000u64..120_000),
        )
            .prop_map(|(text, wait_for, wait_timeout_ms)| StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            }),
        (0usize..100).prop_map(|step| StepResult::JumpTo { step }),
    ]
}

fn arb_execution_status() -> impl Strategy<Value = ExecutionStatus> {
    prop_oneof![
        Just(ExecutionStatus::Running),
        Just(ExecutionStatus::Waiting),
        Just(ExecutionStatus::Completed),
        Just(ExecutionStatus::Aborted),
    ]
}

fn arb_workflow_execution() -> impl Strategy<Value = WorkflowExecution> {
    (
        "[a-z0-9]{8,16}",
        "[a-z_]{3,20}",
        0u64..10_000,
        0usize..100,
        arb_execution_status(),
        0i64..9_999_999_999_999i64,
        0i64..9_999_999_999_999i64,
    )
        .prop_map(
            |(id, workflow_name, pane_id, current_step, status, started_at, updated_at)| {
                WorkflowExecution {
                    id,
                    workflow_name,
                    pane_id,
                    current_step,
                    status,
                    started_at,
                    updated_at,
                }
            },
        )
}

fn arb_workflow_step_policy_decision() -> impl Strategy<Value = WorkflowStepPolicyDecision> {
    prop_oneof![
        Just(WorkflowStepPolicyDecision::Allow),
        Just(WorkflowStepPolicyDecision::Deny),
        Just(WorkflowStepPolicyDecision::RequireApproval),
        Just(WorkflowStepPolicyDecision::Error),
    ]
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn text_match_serde_roundtrip(val in arb_text_match()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: TextMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn wait_condition_serde_roundtrip(val in arb_wait_condition()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn step_result_serde_roundtrip(val in arb_step_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: StepResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn execution_status_serde_roundtrip(val in arb_execution_status()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: ExecutionStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }

    #[test]
    fn workflow_execution_serde_roundtrip(val in arb_workflow_execution()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowExecution = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.id, &back.id);
        prop_assert_eq!(&val.workflow_name, &back.workflow_name);
        prop_assert_eq!(val.pane_id, back.pane_id);
        prop_assert_eq!(val.current_step, back.current_step);
        prop_assert_eq!(val.status, back.status);
        prop_assert_eq!(val.started_at, back.started_at);
    }

    #[test]
    fn workflow_step_policy_decision_serde_roundtrip(val in arb_workflow_step_policy_decision()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowStepPolicyDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val, back);
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn step_result_cont_is_continue(_dummy in 0u8..1) {
        let r = StepResult::cont();
        prop_assert!(r.is_continue());
        prop_assert!(!r.is_done());
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_send_text());
    }

    #[test]
    fn step_result_done_is_terminal(_dummy in 0u8..1) {
        let r = StepResult::done_empty();
        prop_assert!(r.is_done());
        prop_assert!(r.is_terminal());
        prop_assert!(!r.is_continue());
    }

    #[test]
    fn step_result_abort_is_terminal(reason in "[a-z ]{5,30}") {
        let r = StepResult::abort(reason);
        prop_assert!(r.is_terminal());
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_done());
    }

    #[test]
    fn step_result_send_text_detected(text in "[a-z ]{3,30}") {
        let r = StepResult::send_text(text);
        prop_assert!(r.is_send_text());
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_terminal());
    }

    #[test]
    fn step_result_retry_not_terminal(delay_ms in 100u64..30_000) {
        let r = StepResult::retry(delay_ms);
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_continue());
    }

    #[test]
    fn step_result_jump_to_not_terminal(step in 0usize..100) {
        let r = StepResult::jump_to(step);
        prop_assert!(!r.is_terminal());
    }

    #[test]
    fn step_result_wait_for_not_terminal(cond in arb_wait_condition()) {
        let r = StepResult::wait_for(cond);
        prop_assert!(!r.is_terminal());
    }

    #[test]
    fn text_match_substring_constructor(value in "[a-z ]{3,20}") {
        let m = TextMatch::substring(value.clone());
        let check = matches!(m, TextMatch::Substring { .. });
        prop_assert!(check);
    }

    #[test]
    fn text_match_regex_constructor(pattern in "[a-z]+") {
        let m = TextMatch::regex(pattern.clone());
        let check = matches!(m, TextMatch::Regex { .. });
        prop_assert!(check);
    }

    #[test]
    fn wait_condition_pattern_constructor(rule_id in "[a-z_.]{3,15}") {
        let c = WaitCondition::pattern(rule_id.clone());
        match &c {
            WaitCondition::Pattern { pane_id, rule_id: rid } => {
                prop_assert!(pane_id.is_none());
                prop_assert_eq!(rid, &rule_id);
            }
            _ => prop_assert!(false, "Expected Pattern variant"),
        }
    }

    #[test]
    fn wait_condition_pattern_on_pane(pane_id in 0u64..10_000, rule_id in "[a-z_.]{3,15}") {
        let c = WaitCondition::pattern_on_pane(pane_id, rule_id.clone());
        match &c {
            WaitCondition::Pattern { pane_id: pid, rule_id: rid } => {
                prop_assert_eq!(*pid, Some(pane_id));
                prop_assert_eq!(rid, &rule_id);
            }
            _ => prop_assert!(false, "Expected Pattern variant"),
        }
    }

    #[test]
    fn policy_decision_allow_is_allowed(_dummy in 0u8..1) {
        prop_assert!(WorkflowStepPolicyDecision::Allow.is_allowed());
        prop_assert!(!WorkflowStepPolicyDecision::Deny.is_allowed());
        prop_assert!(!WorkflowStepPolicyDecision::RequireApproval.is_allowed());
        prop_assert!(!WorkflowStepPolicyDecision::Error.is_allowed());
    }

    #[test]
    fn step_result_serializes_with_type_tag(val in arb_step_result()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"type\":"));
    }

    #[test]
    fn wait_condition_serializes_with_type_tag(val in arb_wait_condition()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"type\":"));
    }
}
