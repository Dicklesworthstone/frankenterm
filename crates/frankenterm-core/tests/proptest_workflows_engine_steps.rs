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
    fn step_result_done_preserves_result_payload(
        status in "[a-z_]{3,20}",
        count in 0u64..10_000,
        ok in any::<bool>(),
    ) {
        let payload = serde_json::json!({
            "status": status,
            "count": count,
            "ok": ok,
        });
        let r = StepResult::done(payload.clone());

        prop_assert!(r.is_done());
        prop_assert!(r.is_terminal());
        match r {
            StepResult::Done { result } => prop_assert_eq!(result, payload),
            other => prop_assert!(false, "expected Done, got {other:?}"),
        }
    }

    #[test]
    fn step_result_abort_is_terminal(reason in "[a-z ]{5,30}") {
        let r = StepResult::abort(reason.clone());
        prop_assert!(r.is_terminal());
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_done());
        match r {
            StepResult::Abort { reason: actual_reason } => prop_assert_eq!(actual_reason, reason),
            other => prop_assert!(false, "expected Abort, got {other:?}"),
        }
    }

    #[test]
    fn step_result_send_text_detected(text in "[a-z ]{3,30}") {
        let r = StepResult::send_text(text);
        prop_assert!(r.is_send_text());
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_terminal());
    }

    #[test]
    fn step_result_send_text_and_wait_preserves_wait_payload(
        text in "[a-z ]{3,30}",
        cond in arb_wait_condition(),
        timeout_ms in 1u64..60_000,
    ) {
        let r = StepResult::send_text_and_wait(text.clone(), cond.clone(), timeout_ms);
        prop_assert!(r.is_send_text());
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_terminal());
        match r {
            StepResult::SendText {
                text: actual_text,
                wait_for,
                wait_timeout_ms,
            } => {
                prop_assert_eq!(actual_text, text);
                prop_assert_eq!(wait_for, Some(cond));
                prop_assert_eq!(wait_timeout_ms, Some(timeout_ms));
            }
            other => prop_assert!(false, "expected SendText, got {other:?}"),
        }
    }

    #[test]
    fn step_result_retry_not_terminal(delay_ms in 100u64..30_000) {
        let r = StepResult::retry(delay_ms);
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_continue());
        match r {
            StepResult::Retry { delay_ms: actual_delay_ms } => {
                prop_assert_eq!(actual_delay_ms, delay_ms);
            }
            other => prop_assert!(false, "expected Retry, got {other:?}"),
        }
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
    fn step_result_wait_for_with_timeout_preserves_condition_and_timeout(
        cond in arb_wait_condition(),
        timeout_ms in 1u64..60_000,
    ) {
        let r = StepResult::wait_for_with_timeout(cond.clone(), timeout_ms);
        prop_assert!(!r.is_continue());
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_send_text());
        match r {
            StepResult::WaitFor {
                condition,
                timeout_ms: actual_timeout_ms,
            } => {
                prop_assert_eq!(condition, cond);
                prop_assert_eq!(actual_timeout_ms, Some(timeout_ms));
            }
            other => prop_assert!(false, "expected WaitFor, got {other:?}"),
        }
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
    fn wait_condition_pane_idle_constructors_preserve_threshold(
        pane_id in 0u64..10_000,
        idle_threshold_ms in 1u64..60_000,
    ) {
        let target_pane = WaitCondition::pane_idle(idle_threshold_ms);
        prop_assert_eq!(target_pane.pane_id(), None);
        match target_pane {
            WaitCondition::PaneIdle {
                pane_id,
                idle_threshold_ms: actual_threshold_ms,
            } => {
                prop_assert_eq!(pane_id, None);
                prop_assert_eq!(actual_threshold_ms, idle_threshold_ms);
            }
            other => prop_assert!(false, "Expected PaneIdle variant, got {other:?}"),
        }

        let explicit_pane = WaitCondition::pane_idle_on(pane_id, idle_threshold_ms);
        prop_assert_eq!(explicit_pane.pane_id(), Some(pane_id));
        match explicit_pane {
            WaitCondition::PaneIdle {
                pane_id: actual_pane_id,
                idle_threshold_ms: actual_threshold_ms,
            } => {
                prop_assert_eq!(actual_pane_id, Some(pane_id));
                prop_assert_eq!(actual_threshold_ms, idle_threshold_ms);
            }
            other => prop_assert!(false, "Expected PaneIdle variant, got {other:?}"),
        }
    }

    #[test]
    fn wait_condition_sleep_and_external_have_no_pane_id(
        duration_ms in 1u64..60_000,
        key in "[a-z_]{3,15}",
    ) {
        let sleep = WaitCondition::sleep(duration_ms);
        prop_assert_eq!(sleep.pane_id(), None);
        match sleep {
            WaitCondition::Sleep { duration_ms: actual_duration_ms } => {
                prop_assert_eq!(actual_duration_ms, duration_ms);
            }
            other => prop_assert!(false, "Expected Sleep variant, got {other:?}"),
        }

        let external = WaitCondition::external(key.clone());
        prop_assert_eq!(external.pane_id(), None);
        match external {
            WaitCondition::External { key: actual_key } => {
                prop_assert_eq!(actual_key, key);
            }
            other => prop_assert!(false, "Expected External variant, got {other:?}"),
        }
    }

    #[test]
    fn wait_condition_stable_tail_constructors_preserve_duration(
        pane_id in 0u64..10_000,
        stable_for_ms in 1u64..60_000,
    ) {
        let target_pane = WaitCondition::stable_tail(stable_for_ms);
        prop_assert_eq!(target_pane.pane_id(), None);
        match target_pane {
            WaitCondition::StableTail {
                pane_id,
                stable_for_ms: actual_stable_for_ms,
            } => {
                prop_assert_eq!(pane_id, None);
                prop_assert_eq!(actual_stable_for_ms, stable_for_ms);
            }
            other => prop_assert!(false, "Expected StableTail variant, got {other:?}"),
        }

        let explicit_pane = WaitCondition::stable_tail_on(pane_id, stable_for_ms);
        prop_assert_eq!(explicit_pane.pane_id(), Some(pane_id));
        match explicit_pane {
            WaitCondition::StableTail {
                pane_id: actual_pane_id,
                stable_for_ms: actual_stable_for_ms,
            } => {
                prop_assert_eq!(actual_pane_id, Some(pane_id));
                prop_assert_eq!(actual_stable_for_ms, stable_for_ms);
            }
            other => prop_assert!(false, "Expected StableTail variant, got {other:?}"),
        }
    }

    #[test]
    fn wait_condition_text_match_constructors_preserve_matcher_and_pane(
        pane_id in 0u64..10_000,
        matcher in arb_text_match(),
    ) {
        let target_pane = WaitCondition::text_match(matcher.clone());
        prop_assert_eq!(target_pane.pane_id(), None);
        match target_pane {
            WaitCondition::TextMatch {
                pane_id,
                matcher: actual_matcher,
            } => {
                prop_assert_eq!(pane_id, None);
                prop_assert_eq!(actual_matcher, matcher);
            }
            other => prop_assert!(false, "Expected TextMatch variant, got {other:?}"),
        }

        let explicit_pane = WaitCondition::text_match_on_pane(pane_id, matcher.clone());
        prop_assert_eq!(explicit_pane.pane_id(), Some(pane_id));
        match explicit_pane {
            WaitCondition::TextMatch {
                pane_id: actual_pane_id,
                matcher: actual_matcher,
            } => {
                prop_assert_eq!(actual_pane_id, Some(pane_id));
                prop_assert_eq!(actual_matcher, matcher);
            }
            other => prop_assert!(false, "Expected TextMatch variant, got {other:?}"),
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
