// Property-based tests for workflows/runner, workflows/lock, and workflows/context modules.
//
// Covers: serde roundtrips for WorkflowStartResult, WorkflowExecutionResult,
// PaneLockInfo, WorkflowConfig. Also covers structural invariants for helper
// methods and default values.
#![allow(clippy::ignored_unit_patterns)]

use proptest::prelude::*;

use frankenterm_core::workflows::{
    PaneLockInfo, WorkflowConfig, WorkflowExecutionResult, WorkflowStartResult,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_workflow_start_result() -> impl Strategy<Value = WorkflowStartResult> {
    prop_oneof![
        ("[a-z0-9]{8,16}", "[a-z_]{3,20}").prop_map(|(execution_id, workflow_name)| {
            WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            }
        }),
        "[a-z_.]{3,15}".prop_map(|rule_id| WorkflowStartResult::NoMatchingWorkflow { rule_id }),
        (0u64..10_000, "[a-z_]{3,20}", "[a-z0-9]{8,16}").prop_map(
            |(pane_id, held_by_workflow, held_by_execution)| {
                WorkflowStartResult::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                }
            }
        ),
        "[a-z ]{5,30}".prop_map(|error| WorkflowStartResult::Error { error }),
    ]
}

fn arb_workflow_execution_result() -> impl Strategy<Value = WorkflowExecutionResult> {
    prop_oneof![
        (
            "[a-z0-9]{8,16}",
            prop_oneof![
                Just(serde_json::json!(null)),
                Just(serde_json::json!({"key": "value"})),
                Just(serde_json::json!("done")),
            ],
            0u64..120_000,
            0usize..100,
        )
            .prop_map(|(execution_id, result, elapsed_ms, steps_executed)| {
                WorkflowExecutionResult::Completed {
                    execution_id,
                    result,
                    elapsed_ms,
                    steps_executed,
                }
            }),
        ("[a-z0-9]{8,16}", "[a-z ]{5,30}", 0usize..100, 0u64..120_000).prop_map(
            |(execution_id, reason, step_index, elapsed_ms)| {
                WorkflowExecutionResult::Aborted {
                    execution_id,
                    reason,
                    step_index,
                    elapsed_ms,
                }
            }
        ),
        ("[a-z0-9]{8,16}", 0usize..100, "[a-z ]{5,30}").prop_map(
            |(execution_id, step_index, reason)| {
                WorkflowExecutionResult::PolicyDenied {
                    execution_id,
                    step_index,
                    reason,
                }
            }
        ),
        (prop::option::of("[a-z0-9]{8,16}"), "[a-z ]{5,30}").prop_map(|(execution_id, error)| {
            WorkflowExecutionResult::Error {
                execution_id,
                error,
            }
        }),
    ]
}

fn arb_pane_lock_info() -> impl Strategy<Value = PaneLockInfo> {
    (
        0u64..10_000,
        "[a-z_]{3,20}",
        "[a-z0-9]{8,16}",
        0i64..9_999_999_999_999i64,
    )
        .prop_map(
            |(pane_id, workflow_name, execution_id, locked_at_ms)| PaneLockInfo {
                pane_id,
                workflow_name,
                execution_id,
                locked_at_ms,
            },
        )
}

fn arb_workflow_config() -> impl Strategy<Value = WorkflowConfig> {
    (1000u64..120_000, 1u32..10, 100u64..30_000).prop_map(
        |(default_wait_timeout_ms, max_step_retries, retry_delay_ms)| WorkflowConfig {
            default_wait_timeout_ms,
            max_step_retries,
            retry_delay_ms,
        },
    )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn workflow_start_result_serde_roundtrip(val in arb_workflow_start_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn workflow_execution_result_serde_roundtrip(val in arb_workflow_execution_result()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn pane_lock_info_serde_roundtrip(val in arb_pane_lock_info()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: PaneLockInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.pane_id, back.pane_id);
        prop_assert_eq!(&val.workflow_name, &back.workflow_name);
        prop_assert_eq!(&val.execution_id, &back.execution_id);
        prop_assert_eq!(val.locked_at_ms, back.locked_at_ms);
    }

    #[test]
    fn workflow_config_serde_roundtrip(val in arb_workflow_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: WorkflowConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.default_wait_timeout_ms, back.default_wait_timeout_ms);
        prop_assert_eq!(val.max_step_retries, back.max_step_retries);
        prop_assert_eq!(val.retry_delay_ms, back.retry_delay_ms);
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn workflow_start_result_started_has_execution_id(
        execution_id in "[a-z0-9]{8,16}",
        workflow_name in "[a-z_]{3,20}"
    ) {
        let val = WorkflowStartResult::Started { execution_id: execution_id.clone(), workflow_name };
        prop_assert!(val.is_started());
        prop_assert!(!val.is_locked());
        prop_assert_eq!(val.execution_id(), Some(execution_id.as_str()));
    }

    #[test]
    fn workflow_start_result_locked_has_no_execution_id(
        pane_id in 0u64..10_000,
        held_by_workflow in "[a-z_]{3,20}",
        held_by_execution in "[a-z0-9]{8,16}"
    ) {
        let val = WorkflowStartResult::PaneLocked { pane_id, held_by_workflow, held_by_execution };
        prop_assert!(!val.is_started());
        prop_assert!(val.is_locked());
        prop_assert!(val.execution_id().is_none());
    }

    #[test]
    fn workflow_start_result_error_has_no_execution_id(error in "[a-z ]{5,30}") {
        let val = WorkflowStartResult::Error { error };
        prop_assert!(!val.is_started());
        prop_assert!(!val.is_locked());
        prop_assert!(val.execution_id().is_none());
    }

    #[test]
    fn workflow_start_result_no_matching_has_no_execution_id(rule_id in "[a-z_.]{3,15}") {
        let val = WorkflowStartResult::NoMatchingWorkflow { rule_id };
        prop_assert!(!val.is_started());
        prop_assert!(!val.is_locked());
        prop_assert!(val.execution_id().is_none());
    }

    #[test]
    fn workflow_execution_result_completed_has_execution_id(
        execution_id in "[a-z0-9]{8,16}",
        elapsed_ms in 0u64..120_000,
        steps_executed in 0usize..100
    ) {
        let val = WorkflowExecutionResult::Completed {
            execution_id: execution_id.clone(),
            result: serde_json::json!(null),
            elapsed_ms,
            steps_executed,
        };
        prop_assert!(val.is_completed());
        prop_assert!(!val.is_aborted());
        prop_assert_eq!(val.execution_id(), Some(execution_id.as_str()));
    }

    #[test]
    fn workflow_execution_result_aborted_has_execution_id(
        execution_id in "[a-z0-9]{8,16}",
        reason in "[a-z ]{5,30}",
        step_index in 0usize..100
    ) {
        let val = WorkflowExecutionResult::Aborted {
            execution_id: execution_id.clone(),
            reason,
            step_index,
            elapsed_ms: 1000,
        };
        prop_assert!(!val.is_completed());
        prop_assert!(val.is_aborted());
        prop_assert_eq!(val.execution_id(), Some(execution_id.as_str()));
    }

    #[test]
    fn workflow_execution_result_error_execution_id_optional(
        exec_id in prop::option::of("[a-z0-9]{8,16}"),
        error in "[a-z ]{5,30}"
    ) {
        let val = WorkflowExecutionResult::Error {
            execution_id: exec_id,
            error,
        };
        prop_assert!(!val.is_completed());
        prop_assert!(!val.is_aborted());
        // execution_id() returns Option<&str>; just check it doesn't panic
        let _ = val.execution_id();
    }

    #[test]
    fn workflow_start_result_serializes_with_type_tag(val in arb_workflow_start_result()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"type\":"));
    }

    #[test]
    fn workflow_execution_result_serializes_with_type_tag(val in arb_workflow_execution_result()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"type\":"));
    }

    #[test]
    fn workflow_config_default_has_safe_values(_dummy in 0u8..1) {
        let config = WorkflowConfig::default();
        prop_assert!(config.default_wait_timeout_ms > 0);
        prop_assert!(config.max_step_retries > 0);
        prop_assert!(config.retry_delay_ms > 0);
    }

    #[test]
    fn pane_lock_info_has_nonempty_fields(val in arb_pane_lock_info()) {
        prop_assert!(!val.workflow_name.is_empty());
        prop_assert!(!val.execution_id.is_empty());
    }
}
