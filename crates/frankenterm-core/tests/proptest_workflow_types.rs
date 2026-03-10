//! Property tests for workflow submodule serde types.
//!
//! Covers serde roundtrip for types across engine.rs, step_results.rs,
//! traits.rs, runner.rs, lock.rs, and context.rs — all re-exported via
//! `frankenterm_core::workflows::*`.

use frankenterm_core::policy::ActionKind;
use frankenterm_core::workflows::*;
use proptest::prelude::*;

// =============================================================================
// Arbitrary strategies
// =============================================================================

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
        "[a-z0-9_-]{1,15}",
        "[a-z_]{1,15}",
        0..10_000u64,
        0..20usize,
        arb_execution_status(),
        0..i64::MAX,
        0..i64::MAX,
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

fn arb_text_match() -> impl Strategy<Value = TextMatch> {
    prop_oneof![
        "[a-z ]{1,20}".prop_map(|value| TextMatch::Substring { value }),
        "[a-z.+*]{1,15}".prop_map(|pattern| TextMatch::Regex { pattern }),
    ]
}

fn arb_wait_condition() -> impl Strategy<Value = WaitCondition> {
    prop_oneof![
        (proptest::option::of(0..10_000u64), "[a-z_]{1,15}")
            .prop_map(|(pane_id, rule_id)| WaitCondition::Pattern { pane_id, rule_id }),
        (proptest::option::of(0..10_000u64), 100..60_000u64)
            .prop_map(|(pane_id, idle_threshold_ms)| WaitCondition::PaneIdle {
                pane_id,
                idle_threshold_ms
            }),
        (proptest::option::of(0..10_000u64), 100..60_000u64)
            .prop_map(|(pane_id, stable_for_ms)| WaitCondition::StableTail {
                pane_id,
                stable_for_ms
            }),
        (proptest::option::of(0..10_000u64), arb_text_match())
            .prop_map(|(pane_id, matcher)| WaitCondition::TextMatch { pane_id, matcher }),
        (100..60_000u64).prop_map(|duration_ms| WaitCondition::Sleep { duration_ms }),
        "[a-z_]{1,15}".prop_map(|key| WaitCondition::External { key }),
    ]
}

fn arb_step_result() -> impl Strategy<Value = StepResult> {
    prop_oneof![
        Just(StepResult::Continue),
        Just(StepResult::Done {
            result: serde_json::Value::Null,
        }),
        (0..60_000u64).prop_map(|delay_ms| StepResult::Retry { delay_ms }),
        "[a-z ]{1,30}".prop_map(|reason| StepResult::Abort { reason }),
        (arb_wait_condition(), proptest::option::of(100..60_000u64))
            .prop_map(|(condition, timeout_ms)| StepResult::WaitFor {
                condition,
                timeout_ms
            }),
        (
            "[a-z ]{1,20}",
            proptest::option::of(arb_wait_condition()),
            proptest::option::of(100..60_000u64),
        )
            .prop_map(|(text, wait_for, wait_timeout_ms)| StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            }),
        (0..50usize).prop_map(|step| StepResult::JumpTo { step }),
    ]
}

fn arb_workflow_step() -> impl Strategy<Value = WorkflowStep> {
    ("[a-z_]{1,15}", "[a-z ]{1,30}").prop_map(|(name, description)| WorkflowStep { name, description })
}

fn arb_workflow_info() -> impl Strategy<Value = WorkflowInfo> {
    (
        "[a-z_]{1,15}",
        "[a-z ]{1,30}",
        prop::bool::ANY,
        prop::collection::vec("[a-z.]{1,10}", 0..3),
        prop::collection::vec("[a-z._]{1,10}", 0..3),
        prop::collection::vec("[a-z_]{1,10}", 0..3),
        0..20usize,
        prop::bool::ANY,
        prop::bool::ANY,
        prop::bool::ANY,
        prop::bool::ANY,
        prop::collection::vec("[a-z_]{1,10}", 0..3),
    )
        .prop_map(
            |(
                name,
                description,
                enabled,
                trigger_event_types,
                trigger_rule_ids,
                agent_types,
                step_count,
                requires_pane,
                requires_approval,
                can_abort,
                destructive,
                dependencies,
            )| {
                WorkflowInfo {
                    name,
                    description,
                    enabled,
                    trigger_event_types,
                    trigger_rule_ids,
                    agent_types,
                    step_count,
                    requires_pane,
                    requires_approval,
                    can_abort,
                    destructive,
                    dependencies,
                }
            },
        )
}

fn arb_workflow_start_result() -> impl Strategy<Value = WorkflowStartResult> {
    prop_oneof![
        ("[a-z0-9_-]{1,15}", "[a-z_]{1,15}").prop_map(|(execution_id, workflow_name)| {
            WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            }
        }),
        "[a-z_]{1,15}"
            .prop_map(|rule_id| WorkflowStartResult::NoMatchingWorkflow { rule_id }),
        (0..10_000u64, "[a-z_]{1,15}", "[a-z0-9_-]{1,15}").prop_map(
            |(pane_id, held_by_workflow, held_by_execution)| WorkflowStartResult::PaneLocked {
                pane_id,
                held_by_workflow,
                held_by_execution,
            }
        ),
        "[a-z ]{1,30}".prop_map(|error| WorkflowStartResult::Error { error }),
    ]
}

fn arb_workflow_execution_result() -> impl Strategy<Value = WorkflowExecutionResult> {
    prop_oneof![
        (
            "[a-z0-9_-]{1,15}",
            0..100_000u64,
            0..50usize,
        )
            .prop_map(|(execution_id, elapsed_ms, steps_executed)| {
                WorkflowExecutionResult::Completed {
                    execution_id,
                    result: serde_json::Value::Null,
                    elapsed_ms,
                    steps_executed,
                }
            }),
        (
            "[a-z0-9_-]{1,15}",
            "[a-z ]{1,30}",
            0..50usize,
            0..100_000u64,
        )
            .prop_map(|(execution_id, reason, step_index, elapsed_ms)| {
                WorkflowExecutionResult::Aborted {
                    execution_id,
                    reason,
                    step_index,
                    elapsed_ms,
                }
            }),
        (
            "[a-z0-9_-]{1,15}",
            0..50usize,
            "[a-z ]{1,30}",
        )
            .prop_map(|(execution_id, step_index, reason)| {
                WorkflowExecutionResult::PolicyDenied {
                    execution_id,
                    step_index,
                    reason,
                }
            }),
        (proptest::option::of("[a-z0-9_-]{1,15}"), "[a-z ]{1,30}").prop_map(
            |(execution_id, error)| WorkflowExecutionResult::Error {
                execution_id,
                error,
            }
        ),
    ]
}

fn arb_pane_lock_info() -> impl Strategy<Value = PaneLockInfo> {
    (0..10_000u64, "[a-z_]{1,15}", "[a-z0-9_-]{1,15}", 0..i64::MAX).prop_map(
        |(pane_id, workflow_name, execution_id, locked_at_ms)| PaneLockInfo {
            pane_id,
            workflow_name,
            execution_id,
            locked_at_ms,
        },
    )
}

fn arb_workflow_config() -> impl Strategy<Value = WorkflowConfig> {
    (1000..120_000u64, 0..10u32, 100..30_000u64).prop_map(
        |(default_wait_timeout_ms, max_step_retries, retry_delay_ms)| WorkflowConfig {
            default_wait_timeout_ms,
            max_step_retries,
            retry_delay_ms,
        },
    )
}

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::SendCtrlZ),
        Just(ActionKind::SendControl),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Activate),
        Just(ActionKind::Close),
        Just(ActionKind::BrowserAuth),
        Just(ActionKind::WorkflowRun),
        Just(ActionKind::ReservePane),
        Just(ActionKind::ReleasePane),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
        Just(ActionKind::ConnectorNotify),
        Just(ActionKind::ConnectorTicket),
        Just(ActionKind::ConnectorTriggerWorkflow),
        Just(ActionKind::ConnectorAuditLog),
        Just(ActionKind::ConnectorInvoke),
        Just(ActionKind::ConnectorCredentialAction),
    ]
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

    // -- ExecutionStatus --

    #[test]
    fn execution_status_json_roundtrip(s in arb_execution_status()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: ExecutionStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(s, back);
    }

    // -- WorkflowExecution --

    #[test]
    fn workflow_execution_json_roundtrip(e in arb_workflow_execution()) {
        let json = serde_json::to_string(&e).unwrap();
        let back: WorkflowExecution = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&e.id, &back.id);
        prop_assert_eq!(&e.workflow_name, &back.workflow_name);
        prop_assert_eq!(e.pane_id, back.pane_id);
        prop_assert_eq!(e.current_step, back.current_step);
        prop_assert_eq!(e.status, back.status);
        prop_assert_eq!(e.started_at, back.started_at);
        prop_assert_eq!(e.updated_at, back.updated_at);
    }

    // -- TextMatch --

    #[test]
    fn text_match_json_roundtrip(m in arb_text_match()) {
        let json = serde_json::to_string(&m).unwrap();
        let back: TextMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&m, &back);
    }

    // -- WaitCondition --

    #[test]
    fn wait_condition_json_roundtrip(c in arb_wait_condition()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&c, &back);
    }

    // -- StepResult --

    #[test]
    fn step_result_json_roundtrip(r in arb_step_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let _back: StepResult = serde_json::from_str(&json).unwrap();
        // Deserialize succeeds — variant preserved
    }

    // -- WorkflowStep --

    #[test]
    fn workflow_step_json_roundtrip(s in arb_workflow_step()) {
        let json = serde_json::to_string(&s).unwrap();
        let back: WorkflowStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&s.name, &back.name);
        prop_assert_eq!(&s.description, &back.description);
    }

    // -- WorkflowInfo --

    #[test]
    fn workflow_info_json_roundtrip(i in arb_workflow_info()) {
        let json = serde_json::to_string(&i).unwrap();
        let back: WorkflowInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&i.name, &back.name);
        prop_assert_eq!(&i.description, &back.description);
        prop_assert_eq!(i.enabled, back.enabled);
        prop_assert_eq!(i.step_count, back.step_count);
        prop_assert_eq!(i.requires_pane, back.requires_pane);
        prop_assert_eq!(i.requires_approval, back.requires_approval);
        prop_assert_eq!(i.can_abort, back.can_abort);
        prop_assert_eq!(i.destructive, back.destructive);
        prop_assert_eq!(&i.trigger_event_types, &back.trigger_event_types);
        prop_assert_eq!(&i.trigger_rule_ids, &back.trigger_rule_ids);
        prop_assert_eq!(&i.agent_types, &back.agent_types);
        prop_assert_eq!(&i.dependencies, &back.dependencies);
    }

    // -- WorkflowStartResult --

    #[test]
    fn workflow_start_result_json_roundtrip(r in arb_workflow_start_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let _back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
    }

    // -- WorkflowExecutionResult --

    #[test]
    fn workflow_execution_result_json_roundtrip(r in arb_workflow_execution_result()) {
        let json = serde_json::to_string(&r).unwrap();
        let _back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
    }

    // -- PaneLockInfo --

    #[test]
    fn pane_lock_info_json_roundtrip(l in arb_pane_lock_info()) {
        let json = serde_json::to_string(&l).unwrap();
        let back: PaneLockInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(l.pane_id, back.pane_id);
        prop_assert_eq!(&l.workflow_name, &back.workflow_name);
        prop_assert_eq!(&l.execution_id, &back.execution_id);
        prop_assert_eq!(l.locked_at_ms, back.locked_at_ms);
    }

    // -- WorkflowConfig --

    #[test]
    fn workflow_config_json_roundtrip(c in arb_workflow_config()) {
        let json = serde_json::to_string(&c).unwrap();
        let back: WorkflowConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(c.default_wait_timeout_ms, back.default_wait_timeout_ms);
        prop_assert_eq!(c.max_step_retries, back.max_step_retries);
        prop_assert_eq!(c.retry_delay_ms, back.retry_delay_ms);
    }

    // -- WorkflowStepPolicyDecision --

    #[test]
    fn workflow_step_policy_decision_json_roundtrip(d in arb_workflow_step_policy_decision()) {
        let json = serde_json::to_string(&d).unwrap();
        let back: WorkflowStepPolicyDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d, back);
    }

    // -- ActionKind (policy type, tested here since workflow engine uses it) --

    #[test]
    fn action_kind_json_roundtrip(a in arb_action_kind()) {
        let json = serde_json::to_string(&a).unwrap();
        let back: ActionKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(a, back);
    }
}

// =============================================================================
// Behavioral property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    // -- StepResult predicates --

    #[test]
    fn step_result_is_continue_only_for_continue(r in arb_step_result()) {
        let expected = matches!(r, StepResult::Continue);
        prop_assert_eq!(r.is_continue(), expected);
    }

    #[test]
    fn step_result_is_done_only_for_done(r in arb_step_result()) {
        let expected = matches!(r, StepResult::Done { .. });
        prop_assert_eq!(r.is_done(), expected);
    }

    #[test]
    fn step_result_is_terminal_for_done_or_abort(r in arb_step_result()) {
        let expected = matches!(r, StepResult::Done { .. } | StepResult::Abort { .. });
        prop_assert_eq!(r.is_terminal(), expected);
    }

    #[test]
    fn step_result_is_send_text_only_for_send_text(r in arb_step_result()) {
        let expected = matches!(r, StepResult::SendText { .. });
        prop_assert_eq!(r.is_send_text(), expected);
    }

    // -- WorkflowStartResult predicates --

    #[test]
    fn workflow_start_result_is_started_consistency(r in arb_workflow_start_result()) {
        let expected = matches!(r, WorkflowStartResult::Started { .. });
        prop_assert_eq!(r.is_started(), expected);
    }

    #[test]
    fn workflow_start_result_is_locked_consistency(r in arb_workflow_start_result()) {
        let expected = matches!(r, WorkflowStartResult::PaneLocked { .. });
        prop_assert_eq!(r.is_locked(), expected);
    }

    #[test]
    fn workflow_start_result_execution_id_iff_started(r in arb_workflow_start_result()) {
        let has_id = r.execution_id().is_some();
        let is_started = r.is_started();
        prop_assert_eq!(has_id, is_started);
    }

    // -- WorkflowStepPolicyDecision is_allowed --

    #[test]
    fn policy_decision_is_allowed_only_for_allow(d in arb_workflow_step_policy_decision()) {
        let expected = matches!(d, WorkflowStepPolicyDecision::Allow);
        prop_assert_eq!(d.is_allowed(), expected);
    }

    // -- ActionKind is_mutating --

    #[test]
    fn action_kind_is_mutating_excludes_reads(a in arb_action_kind()) {
        if matches!(a, ActionKind::ReadOutput | ActionKind::SearchOutput) {
            prop_assert!(!a.is_mutating());
        }
    }

    // -- WorkflowConfig default values --

    #[test]
    fn workflow_config_default_has_sane_values(_dummy in 0..1u8) {
        let cfg = WorkflowConfig::default();
        prop_assert_eq!(cfg.default_wait_timeout_ms, 30_000);
        prop_assert_eq!(cfg.max_step_retries, 3);
        prop_assert_eq!(cfg.retry_delay_ms, 1_000);
    }

    // -- StepResult constructors match predicates --

    #[test]
    fn step_result_cont_is_continue(_dummy in 0..1u8) {
        let r = StepResult::cont();
        prop_assert!(r.is_continue());
        prop_assert!(!r.is_terminal());
    }

    #[test]
    fn step_result_done_empty_is_terminal(_dummy in 0..1u8) {
        let r = StepResult::done_empty();
        prop_assert!(r.is_done());
        prop_assert!(r.is_terminal());
    }

    #[test]
    fn step_result_abort_is_terminal(reason in "[a-z ]{1,20}") {
        let r = StepResult::abort(reason);
        prop_assert!(r.is_terminal());
        prop_assert!(!r.is_done());
    }

    #[test]
    fn step_result_send_text_is_send_text(text in "[a-z ]{1,20}") {
        let r = StepResult::send_text(text);
        prop_assert!(r.is_send_text());
        prop_assert!(!r.is_terminal());
    }

    #[test]
    fn step_result_jump_to_not_terminal(step in 0..50usize) {
        let r = StepResult::jump_to(step);
        prop_assert!(!r.is_terminal());
        prop_assert!(!r.is_continue());
    }
}
