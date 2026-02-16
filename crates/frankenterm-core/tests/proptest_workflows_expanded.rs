//! Expanded property-based tests for the workflows module.
//!
//! Covers DescriptorStep serde roundtrips, DescriptorMatcher serde,
//! WorkflowStartResult/WorkflowExecutionResult state invariants,
//! ExecutionStatus ordering, UnstickReport aggregation invariants,
//! WaitConditionResult state machine, GroupLockResult/Conflict,
//! and various config/metadata types.
//!
//! Complements the existing proptest_workflows.rs which covers StepResult,
//! TextMatch, WaitCondition, PaneWorkflowLockManager, BroadcastPrecondition,
//! BroadcastResult, validate_session_id, FallbackNextStepPlan,
//! WorkflowDescriptor, and FallbackReason.

use frankenterm_core::workflows::{
    DescriptorControlKey, DescriptorLimits, DescriptorMatcher, DescriptorStep, DescriptorTrigger,
    ExecutionStatus, GroupLockConflict, GroupLockResult, PaneWorkflowLockManager, UnstickConfig,
    UnstickFinding, UnstickFindingKind, UnstickReport, WaitConditionOptions, WaitConditionResult,
    WorkflowExecution, WorkflowExecutionResult, WorkflowStartResult, WorkflowStep,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_descriptor_control_key() -> impl Strategy<Value = DescriptorControlKey> {
    prop_oneof![
        Just(DescriptorControlKey::CtrlC),
        Just(DescriptorControlKey::CtrlD),
        Just(DescriptorControlKey::CtrlZ),
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

fn arb_unstick_finding_kind() -> impl Strategy<Value = UnstickFindingKind> {
    prop_oneof![
        Just(UnstickFindingKind::TodoComment),
        Just(UnstickFindingKind::PanicSite),
        Just(UnstickFindingKind::SuppressedError),
    ]
}

fn arb_step_id() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,15}"
}

fn arb_short_text() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 ]{1,50}"
}

fn arb_descriptor_matcher() -> impl Strategy<Value = DescriptorMatcher> {
    prop_oneof![
        arb_short_text().prop_map(|value| DescriptorMatcher::Substring { value }),
        // Only simple valid regex patterns to avoid regex compilation failures
        prop_oneof![
            Just("hello".to_string()),
            Just("foo.*bar".to_string()),
            Just("\\d+".to_string()),
            Just("[a-z]+".to_string()),
            Just("test|check".to_string()),
        ]
        .prop_map(|pattern| DescriptorMatcher::Regex { pattern }),
    ]
}

/// Generate a flat (non-recursive) DescriptorStep — avoids stack overflow
/// from recursive Conditional/Loop nesting.
fn arb_descriptor_step_flat() -> impl Strategy<Value = DescriptorStep> {
    prop_oneof![
        (
            arb_step_id(),
            arb_descriptor_matcher(),
            proptest::option::of(1u64..120_000)
        )
            .prop_map(|(id, matcher, timeout_ms)| DescriptorStep::WaitFor {
                id,
                description: None,
                matcher,
                timeout_ms,
            }),
        (arb_step_id(), 1u64..30_000u64).prop_map(|(id, duration_ms)| DescriptorStep::Sleep {
            id,
            description: None,
            duration_ms,
        }),
        (arb_step_id(), arb_short_text()).prop_map(|(id, text)| DescriptorStep::SendText {
            id,
            description: None,
            text,
            wait_for: None,
            wait_timeout_ms: None,
        }),
        (arb_step_id(), arb_descriptor_control_key()).prop_map(|(id, key)| {
            DescriptorStep::SendCtrl {
                id,
                description: None,
                key,
            }
        }),
        (arb_step_id(), arb_short_text()).prop_map(|(id, message)| DescriptorStep::Notify {
            id,
            description: None,
            message,
        }),
        (arb_step_id(), arb_short_text()).prop_map(|(id, message)| DescriptorStep::Log {
            id,
            description: None,
            message,
        }),
        (arb_step_id(), arb_short_text()).prop_map(|(id, reason)| DescriptorStep::Abort {
            id,
            description: None,
            reason,
        }),
    ]
}

fn arb_unstick_finding() -> impl Strategy<Value = UnstickFinding> {
    (
        arb_unstick_finding_kind(),
        "[a-z/]{1,30}\\.rs",
        1u32..10_000u32,
        "[a-zA-Z0-9_ ]{1,80}",
        arb_short_text(),
    )
        .prop_map(|(kind, file, line, snippet, suggestion)| UnstickFinding {
            kind,
            file,
            line,
            snippet,
            suggestion,
        })
}

fn arb_workflow_start_result() -> impl Strategy<Value = WorkflowStartResult> {
    prop_oneof![
        (arb_step_id(), arb_short_text()).prop_map(|(execution_id, workflow_name)| {
            WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            }
        }),
        arb_step_id().prop_map(|rule_id| WorkflowStartResult::NoMatchingWorkflow { rule_id }),
        (any::<u64>(), arb_short_text(), arb_step_id()).prop_map(
            |(pane_id, held_by_workflow, held_by_execution)| WorkflowStartResult::PaneLocked {
                pane_id,
                held_by_workflow,
                held_by_execution,
            }
        ),
        arb_short_text().prop_map(|error| WorkflowStartResult::Error { error }),
    ]
}

fn arb_workflow_execution_result() -> impl Strategy<Value = WorkflowExecutionResult> {
    prop_oneof![
        (arb_step_id(), any::<u64>(), 0usize..100).prop_map(
            |(execution_id, elapsed_ms, steps_executed)| {
                WorkflowExecutionResult::Completed {
                    execution_id,
                    result: serde_json::json!({"status": "ok"}),
                    elapsed_ms,
                    steps_executed,
                }
            }
        ),
        (arb_step_id(), arb_short_text(), 0usize..100, any::<u64>()).prop_map(
            |(execution_id, reason, step_index, elapsed_ms)| WorkflowExecutionResult::Aborted {
                execution_id,
                reason,
                step_index,
                elapsed_ms,
            }
        ),
        (arb_step_id(), 0usize..100, arb_short_text()).prop_map(
            |(execution_id, step_index, reason)| WorkflowExecutionResult::PolicyDenied {
                execution_id,
                step_index,
                reason,
            }
        ),
        (proptest::option::of(arb_step_id()), arb_short_text()).prop_map(
            |(execution_id, error)| WorkflowExecutionResult::Error {
                execution_id,
                error,
            }
        ),
    ]
}

fn arb_wait_condition_result() -> impl Strategy<Value = WaitConditionResult> {
    prop_oneof![
        (
            any::<u64>(),
            1usize..1000,
            proptest::option::of(arb_short_text())
        )
            .prop_map(
                |(elapsed_ms, polls, context)| WaitConditionResult::Satisfied {
                    elapsed_ms,
                    polls,
                    context,
                }
            ),
        (
            any::<u64>(),
            1usize..1000,
            proptest::option::of(arb_short_text())
        )
            .prop_map(
                |(elapsed_ms, polls, last_observed)| WaitConditionResult::TimedOut {
                    elapsed_ms,
                    polls,
                    last_observed,
                }
            ),
        arb_short_text().prop_map(|reason| WaitConditionResult::Unsupported { reason }),
    ]
}

// ── DescriptorControlKey serde roundtrip ────────────────────────────────────

proptest! {
    #[test]
    fn descriptor_control_key_serde_roundtrip(key in arb_descriptor_control_key()) {
        let json = serde_json::to_string(&key).unwrap();
        let back: DescriptorControlKey = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "roundtrip mismatch");
    }
}

// ── DescriptorMatcher ───────────────────────────────────────────────────────

proptest! {
    #[test]
    fn descriptor_matcher_serde_roundtrip(matcher in arb_descriptor_matcher()) {
        let json = serde_json::to_string(&matcher).unwrap();
        let back: DescriptorMatcher = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "roundtrip mismatch");
    }

    #[test]
    fn descriptor_matcher_json_has_kind_field(matcher in arb_descriptor_matcher()) {
        let json = serde_json::to_string(&matcher).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let kind = parsed.get("kind").and_then(|v| v.as_str()).unwrap();
        match matcher {
            DescriptorMatcher::Substring { .. } => prop_assert_eq!(kind, "substring"),
            DescriptorMatcher::Regex { .. } => prop_assert_eq!(kind, "regex"),
        }
    }
}

// ── DescriptorStep ──────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn descriptor_step_serde_roundtrip(step in arb_descriptor_step_flat()) {
        let json = serde_json::to_string(&step).unwrap();
        let back: DescriptorStep = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "roundtrip mismatch");
    }

    #[test]
    fn descriptor_step_json_has_type_field(step in arb_descriptor_step_flat()) {
        let json = serde_json::to_string(&step).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ty = parsed.get("type").and_then(|v| v.as_str()).unwrap();
        let expected_type = match step {
            DescriptorStep::WaitFor { .. } => "wait_for",
            DescriptorStep::Sleep { .. } => "sleep",
            DescriptorStep::SendText { .. } => "send_text",
            DescriptorStep::SendCtrl { .. } => "send_ctrl",
            DescriptorStep::Notify { .. } => "notify",
            DescriptorStep::Log { .. } => "log",
            DescriptorStep::Abort { .. } => "abort",
            DescriptorStep::Conditional { .. } => "conditional",
            DescriptorStep::Loop { .. } => "loop",
        };
        prop_assert_eq!(ty, expected_type, "serde type tag mismatch");
    }

    #[test]
    fn descriptor_step_json_has_id_field(step in arb_descriptor_step_flat()) {
        let json = serde_json::to_string(&step).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let id = parsed.get("id").and_then(|v| v.as_str());
        prop_assert!(id.is_some(), "every step must have an 'id' field in JSON");
        prop_assert!(!id.unwrap().is_empty(), "step id must not be empty");
    }

    #[test]
    fn descriptor_step_conditional_serde_roundtrip(
        id in arb_step_id(),
        test_text in arb_short_text(),
        matcher in arb_descriptor_matcher(),
    ) {
        let step = DescriptorStep::Conditional {
            id,
            description: Some("test conditional".to_string()),
            test_text,
            matcher,
            then_steps: vec![
                DescriptorStep::Log {
                    id: "then1".to_string(),
                    description: None,
                    message: "matched".to_string(),
                },
            ],
            else_steps: vec![],
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: DescriptorStep = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "conditional roundtrip mismatch");
    }

    #[test]
    fn descriptor_step_loop_serde_roundtrip(
        id in arb_step_id(),
        count in 1u32..10,
    ) {
        let step = DescriptorStep::Loop {
            id,
            description: None,
            count,
            body: vec![
                DescriptorStep::Sleep {
                    id: "body1".to_string(),
                    description: None,
                    duration_ms: 100,
                },
            ],
        };
        let json = serde_json::to_string(&step).unwrap();
        let back: DescriptorStep = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "loop roundtrip mismatch");
    }
}

// ── ExecutionStatus ─────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn execution_status_serde_roundtrip(status in arb_execution_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let back: ExecutionStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, back, "roundtrip mismatch");
    }

    #[test]
    fn execution_status_json_is_snake_case(status in arb_execution_status()) {
        let json = serde_json::to_string(&status).unwrap();
        // JSON should be a quoted string in snake_case
        let s = json.trim_matches('"');
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "expected snake_case, got: {}",
            s
        );
    }
}

// ── WorkflowStartResult ────────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_start_result_serde_roundtrip(result in arb_workflow_start_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "roundtrip mismatch");
    }

    #[test]
    fn workflow_start_result_is_started_consistent(result in arb_workflow_start_result()) {
        let is_started = result.is_started();
        match &result {
            WorkflowStartResult::Started { .. } => prop_assert!(is_started),
            _ => prop_assert!(!is_started),
        }
    }

    #[test]
    fn workflow_start_result_is_locked_consistent(result in arb_workflow_start_result()) {
        let is_locked = result.is_locked();
        match &result {
            WorkflowStartResult::PaneLocked { .. } => prop_assert!(is_locked),
            _ => prop_assert!(!is_locked),
        }
    }

    #[test]
    fn workflow_start_result_execution_id_only_on_started(result in arb_workflow_start_result()) {
        let eid = result.execution_id();
        match &result {
            WorkflowStartResult::Started { execution_id, .. } => {
                prop_assert_eq!(eid, Some(execution_id.as_str()));
            }
            _ => {
                prop_assert!(eid.is_none());
            }
        }
    }

    #[test]
    fn workflow_start_result_json_has_type_field(result in arb_workflow_start_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ty = parsed.get("type").and_then(|v| v.as_str()).unwrap();
        match result {
            WorkflowStartResult::Started { .. } => prop_assert_eq!(ty, "started"),
            WorkflowStartResult::NoMatchingWorkflow { .. } => {
                prop_assert_eq!(ty, "no_matching_workflow");
            }
            WorkflowStartResult::PaneLocked { .. } => prop_assert_eq!(ty, "pane_locked"),
            WorkflowStartResult::Error { .. } => prop_assert_eq!(ty, "error"),
        }
    }
}

// ── WorkflowExecutionResult ─────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_execution_result_serde_roundtrip(result in arb_workflow_execution_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "roundtrip mismatch");
    }

    #[test]
    fn workflow_execution_result_is_completed_consistent(result in arb_workflow_execution_result()) {
        let is_completed = result.is_completed();
        match &result {
            WorkflowExecutionResult::Completed { .. } => prop_assert!(is_completed),
            _ => prop_assert!(!is_completed),
        }
    }

    #[test]
    fn workflow_execution_result_is_aborted_consistent(result in arb_workflow_execution_result()) {
        let is_aborted = result.is_aborted();
        match &result {
            WorkflowExecutionResult::Aborted { .. } => prop_assert!(is_aborted),
            _ => prop_assert!(!is_aborted),
        }
    }

    #[test]
    fn workflow_execution_result_execution_id_consistency(result in arb_workflow_execution_result()) {
        let eid = result.execution_id();
        match &result {
            WorkflowExecutionResult::Completed { execution_id, .. }
            | WorkflowExecutionResult::Aborted { execution_id, .. }
            | WorkflowExecutionResult::PolicyDenied { execution_id, .. } => {
                prop_assert_eq!(eid, Some(execution_id.as_str()));
            }
            WorkflowExecutionResult::Error { execution_id, .. } => {
                prop_assert_eq!(eid, execution_id.as_deref());
            }
        }
    }

    #[test]
    fn workflow_execution_result_json_has_type_field(result in arb_workflow_execution_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ty = parsed.get("type").and_then(|v| v.as_str()).unwrap();
        match result {
            WorkflowExecutionResult::Completed { .. } => prop_assert_eq!(ty, "completed"),
            WorkflowExecutionResult::Aborted { .. } => prop_assert_eq!(ty, "aborted"),
            WorkflowExecutionResult::PolicyDenied { .. } => prop_assert_eq!(ty, "policy_denied"),
            WorkflowExecutionResult::Error { .. } => prop_assert_eq!(ty, "error"),
        }
    }
}

// ── WaitConditionResult ─────────────────────────────────────────────────────

proptest! {
    #[test]
    fn wait_condition_result_is_satisfied_xor_timed_out(result in arb_wait_condition_result()) {
        let satisfied = result.is_satisfied();
        let timed_out = result.is_timed_out();
        // At most one of these should be true
        prop_assert!(!(satisfied && timed_out), "cannot be both satisfied and timed out");
        // Unsupported is neither
        match &result {
            WaitConditionResult::Unsupported { .. } => {
                prop_assert!(!satisfied && !timed_out);
            }
            WaitConditionResult::Satisfied { .. } => {
                prop_assert!(satisfied && !timed_out);
            }
            WaitConditionResult::TimedOut { .. } => {
                prop_assert!(!satisfied && timed_out);
            }
        }
    }

    #[test]
    fn wait_condition_result_elapsed_ms_consistent(result in arb_wait_condition_result()) {
        let elapsed = result.elapsed_ms();
        match &result {
            WaitConditionResult::Satisfied { elapsed_ms, .. } => {
                prop_assert_eq!(elapsed, Some(*elapsed_ms));
            }
            WaitConditionResult::TimedOut { elapsed_ms, .. } => {
                prop_assert_eq!(elapsed, Some(*elapsed_ms));
            }
            WaitConditionResult::Unsupported { .. } => {
                prop_assert!(elapsed.is_none());
            }
        }
    }
}

// ── UnstickFindingKind ──────────────────────────────────────────────────────

proptest! {
    #[test]
    fn unstick_finding_kind_serde_roundtrip(kind in arb_unstick_finding_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: UnstickFindingKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(kind, back);
    }

    #[test]
    fn unstick_finding_kind_label_nonempty(kind in arb_unstick_finding_kind()) {
        let label = kind.label();
        prop_assert!(!label.is_empty(), "label must not be empty");
    }
}

// ── UnstickFinding ──────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn unstick_finding_serde_roundtrip(finding in arb_unstick_finding()) {
        let json = serde_json::to_string(&finding).unwrap();
        let back: UnstickFinding = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2, "roundtrip mismatch");
    }

    #[test]
    fn unstick_finding_line_positive(finding in arb_unstick_finding()) {
        prop_assert!(finding.line >= 1, "line must be >= 1, got {}", finding.line);
    }
}

// ── UnstickReport ───────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn unstick_report_total_findings_matches_vec_len(
        findings in proptest::collection::vec(arb_unstick_finding(), 0..20),
        files_scanned in 0usize..100,
    ) {
        let mut counts = BTreeMap::new();
        for f in &findings {
            *counts.entry(f.kind.label().to_string()).or_insert(0usize) += 1;
        }
        let report = UnstickReport {
            findings: findings.clone(),
            files_scanned,
            truncated: false,
            scanner: "text".to_string(),
            counts,
        };
        prop_assert_eq!(
            report.total_findings(),
            findings.len(),
            "total_findings() should match findings.len()"
        );
    }

    #[test]
    fn unstick_report_human_summary_nonempty(
        findings in proptest::collection::vec(arb_unstick_finding(), 0..10),
    ) {
        let report = UnstickReport {
            findings,
            files_scanned: 5,
            truncated: false,
            scanner: "text".to_string(),
            counts: BTreeMap::new(),
        };
        let summary = report.human_summary();
        prop_assert!(!summary.is_empty(), "human_summary should never be empty");
    }

    #[test]
    fn unstick_report_serde_roundtrip(
        findings in proptest::collection::vec(arb_unstick_finding(), 0..5),
        files_scanned in 0usize..50,
        truncated in any::<bool>(),
    ) {
        let mut counts = BTreeMap::new();
        for f in &findings {
            *counts.entry(f.kind.label().to_string()).or_insert(0usize) += 1;
        }
        let report = UnstickReport {
            findings,
            files_scanned,
            truncated,
            scanner: "text".to_string(),
            counts,
        };
        let json = serde_json::to_string(&report).unwrap();
        let back: UnstickReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.total_findings(), back.total_findings());
        prop_assert_eq!(report.files_scanned, back.files_scanned);
        prop_assert_eq!(report.truncated, back.truncated);
        prop_assert_eq!(&report.scanner, &back.scanner);
    }

    #[test]
    fn unstick_report_counts_sum_matches_findings(
        findings in proptest::collection::vec(arb_unstick_finding(), 1..15),
    ) {
        let mut counts = BTreeMap::new();
        for f in &findings {
            *counts.entry(f.kind.label().to_string()).or_insert(0usize) += 1;
        }
        let sum: usize = counts.values().sum();
        prop_assert_eq!(sum, findings.len(), "counts sum should equal findings count");
    }
}

// ── UnstickReport non-proptest ──────────────────────────────────────────────

#[test]
fn unstick_report_empty_has_no_findings() {
    let report = UnstickReport::empty("text");
    assert_eq!(report.total_findings(), 0);
    assert!(!report.truncated);
    assert_eq!(report.files_scanned, 0);
    assert!(report.counts.is_empty());
}

#[test]
fn unstick_report_empty_summary_says_no_findings() {
    let report = UnstickReport::empty("ast-grep");
    let summary = report.human_summary();
    assert!(
        summary.contains("No actionable findings"),
        "empty report summary should say 'No actionable findings', got: {}",
        summary
    );
}

// ── UnstickConfig ───────────────────────────────────────────────────────────

#[test]
fn unstick_config_default_values() {
    let config = UnstickConfig::default();
    assert_eq!(config.max_findings_per_kind, 10);
    assert_eq!(config.max_total_findings, 25);
    assert!(
        !config.extensions.is_empty(),
        "default extensions should not be empty"
    );
    assert!(config.extensions.contains(&"rs".to_string()));
}

proptest! {
    #[test]
    fn unstick_config_serde_roundtrip(
        max_per_kind in 1usize..50,
        max_total in 1usize..100,
    ) {
        let config = UnstickConfig {
            root: std::path::PathBuf::from("/tmp/test"),
            max_findings_per_kind: max_per_kind,
            max_total_findings: max_total,
            extensions: vec!["rs".to_string(), "py".to_string()],
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: UnstickConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.max_findings_per_kind, max_per_kind);
        prop_assert_eq!(back.max_total_findings, max_total);
        prop_assert_eq!(back.root, std::path::PathBuf::from("/tmp/test"));
    }
}

// ── GroupLockResult ─────────────────────────────────────────────────────────

proptest! {
    #[test]
    fn group_lock_result_is_acquired_consistent(
        pane_ids in proptest::collection::vec(1u64..100, 1..5),
    ) {
        let result = GroupLockResult::Acquired {
            locked_panes: pane_ids.clone(),
        };
        prop_assert!(result.is_acquired());
    }

    #[test]
    fn group_lock_partial_failure_not_acquired(
        would_have in proptest::collection::vec(1u64..100, 0..3),
    ) {
        let conflict = GroupLockConflict {
            pane_id: 999,
            held_by_workflow: "other_wf".to_string(),
            held_by_execution: "exec_123".to_string(),
        };
        let result = GroupLockResult::PartialFailure {
            would_have_locked: would_have,
            conflicts: vec![conflict],
        };
        prop_assert!(!result.is_acquired());
    }
}

// ── GroupLockConflict serde ──────────────────────────────────────────────────

proptest! {
    #[test]
    fn group_lock_conflict_serde_roundtrip(
        pane_id in any::<u64>(),
        wf_name in arb_short_text(),
        exec_id in arb_step_id(),
    ) {
        let conflict = GroupLockConflict {
            pane_id,
            held_by_workflow: wf_name.clone(),
            held_by_execution: exec_id.clone(),
        };
        let json = serde_json::to_string(&conflict).unwrap();
        let back: GroupLockConflict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(conflict.pane_id, back.pane_id);
        prop_assert_eq!(&conflict.held_by_workflow, &back.held_by_workflow);
        prop_assert_eq!(&conflict.held_by_execution, &back.held_by_execution);
    }
}

// ── WaitConditionOptions defaults ───────────────────────────────────────────

#[test]
fn wait_condition_options_defaults_valid() {
    let opts = WaitConditionOptions::default();
    assert!(opts.tail_lines > 0, "tail_lines should be positive");
    assert!(
        opts.poll_initial <= opts.poll_max,
        "poll_initial should be <= poll_max"
    );
    assert!(opts.max_polls > 0, "max_polls should be positive");
    assert!(
        opts.allow_idle_heuristics,
        "default should allow idle heuristics"
    );
}

// ── WorkflowStep serde ──────────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_step_serde_roundtrip(
        name in arb_step_id(),
        description in arb_short_text(),
    ) {
        let step = WorkflowStep::new(name.clone(), description.clone());
        let json = serde_json::to_string(&step).unwrap();
        let back: WorkflowStep = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.name, &name);
        prop_assert_eq!(&back.description, &description);
    }
}

// ── DescriptorLimits defaults ───────────────────────────────────────────────

#[test]
fn descriptor_limits_defaults_valid() {
    let limits = DescriptorLimits::default();
    assert_eq!(limits.max_steps, 32);
    assert_eq!(limits.max_wait_timeout_ms, 120_000);
    assert_eq!(limits.max_sleep_ms, 30_000);
    assert_eq!(limits.max_text_len, 8_192);
    assert_eq!(limits.max_match_len, 1_024);
}

// ── DescriptorTrigger serde ─────────────────────────────────────────────────

proptest! {
    #[test]
    fn descriptor_trigger_serde_roundtrip(
        event_types in proptest::collection::vec("[a-z.]{3,20}", 0..3),
        agent_types in proptest::collection::vec("[a-z_]{3,15}", 0..2),
        rule_ids in proptest::collection::vec("[a-z.]{3,20}", 0..2),
    ) {
        let trigger = DescriptorTrigger {
            event_types: event_types.clone(),
            agent_types: agent_types.clone(),
            rule_ids: rule_ids.clone(),
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let back: DescriptorTrigger = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.event_types, &event_types);
        prop_assert_eq!(&back.agent_types, &agent_types);
        prop_assert_eq!(&back.rule_ids, &rule_ids);
    }
}

// ── WorkflowExecution serde ─────────────────────────────────────────────────

proptest! {
    #[test]
    fn workflow_execution_serde_roundtrip(
        id in arb_step_id(),
        workflow_name in arb_short_text(),
        pane_id in any::<u64>(),
        current_step in 0usize..50,
        status in arb_execution_status(),
        started_at in any::<i64>(),
        updated_at in any::<i64>(),
    ) {
        let exec = WorkflowExecution {
            id: id.clone(),
            workflow_name: workflow_name.clone(),
            pane_id,
            current_step,
            status,
            started_at,
            updated_at,
        };
        let json = serde_json::to_string(&exec).unwrap();
        let back: WorkflowExecution = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.id, &id);
        prop_assert_eq!(&back.workflow_name, &workflow_name);
        prop_assert_eq!(back.pane_id, pane_id);
        prop_assert_eq!(back.current_step, current_step);
        prop_assert_eq!(back.status, status);
        prop_assert_eq!(back.started_at, started_at);
        prop_assert_eq!(back.updated_at, updated_at);
    }
}

// ── GroupLock via PaneWorkflowLockManager ────────────────────────────────────

proptest! {
    #[test]
    fn group_lock_acquire_all_or_none(
        pane_ids_raw in proptest::collection::vec(1u64..1000, 2..6),
    ) {
        let mgr = PaneWorkflowLockManager::new();
        // Deduplicate: try_acquire_group locks sequentially, so duplicate
        // IDs would self-conflict on the second acquire.
        let mut pane_ids = pane_ids_raw.clone();
        pane_ids.sort();
        pane_ids.dedup();
        if pane_ids.len() < 2 {
            return Ok(());
        }
        let result = mgr.try_acquire_group(&pane_ids, "wf1", "exec1");
        match &result {
            GroupLockResult::Acquired { locked_panes } => {
                // All requested panes should be locked
                prop_assert_eq!(locked_panes.len(), pane_ids.len());
            }
            GroupLockResult::PartialFailure { .. } => {
                // Partial failure means some were conflicted — shouldn't happen
                // with a fresh manager
                prop_assert!(false, "fresh manager should acquire all locks");
            }
        }
    }

    #[test]
    fn group_lock_conflict_detected(
        pane_id in 1u64..1000,
    ) {
        let mgr = PaneWorkflowLockManager::new();
        // Lock one pane with workflow 1
        let _ = mgr.try_acquire(pane_id, "wf1", "exec1");
        // Try to group-lock overlapping panes with workflow 2
        let result = mgr.try_acquire_group(&[pane_id, pane_id + 1], "wf2", "exec2");
        match &result {
            GroupLockResult::PartialFailure { conflicts, .. } => {
                prop_assert!(!conflicts.is_empty(), "should have at least one conflict");
                prop_assert_eq!(conflicts[0].pane_id, pane_id);
                prop_assert_eq!(&conflicts[0].held_by_workflow, "wf1");
            }
            GroupLockResult::Acquired { .. } => {
                prop_assert!(false, "should not acquire when overlap exists");
            }
        }
    }
}
