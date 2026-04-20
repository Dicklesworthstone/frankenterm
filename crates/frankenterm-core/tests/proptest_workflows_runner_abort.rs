// Property-based tests for workflows/runner AbortResult.
//
// Covers JSON field emission and success/failure invariants for the
// serialize-only abort result carrier.

use frankenterm_core::workflows::AbortResult;
use proptest::prelude::*;

fn arb_abort_result() -> impl Strategy<Value = AbortResult> {
    prop_oneof![
        (
            "[a-z0-9-]{8,24}",
            "[a-z_]{3,24}",
            0u64..500,
            "[a-z_]{3,24}",
            0usize..50,
            prop::option::of("[a-z ,._-]{3,40}"),
            1u64..9_999_999_999_999,
        )
            .prop_map(
                |(
                    execution_id,
                    workflow_name,
                    pane_id,
                    previous_status,
                    aborted_at_step,
                    reason,
                    aborted_at,
                )| AbortResult {
                    aborted: true,
                    execution_id,
                    workflow_name,
                    pane_id,
                    previous_status,
                    aborted_at_step,
                    reason,
                    aborted_at: Some(aborted_at),
                    error_reason: None,
                },
            ),
        (
            "[a-z0-9-]{8,24}",
            "[a-z_]{3,24}",
            0u64..500,
            "[a-z_]{3,24}",
            0usize..50,
            "[a-z_]{3,32}",
        )
            .prop_map(
                |(
                    execution_id,
                    workflow_name,
                    pane_id,
                    previous_status,
                    aborted_at_step,
                    error_reason,
                )| AbortResult {
                    aborted: false,
                    execution_id,
                    workflow_name,
                    pane_id,
                    previous_status,
                    aborted_at_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some(error_reason),
                },
            ),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn abort_result_json_matches_option_fields(result in arb_abort_result()) {
        let json = serde_json::to_value(&result).unwrap();
        let obj = json.as_object().unwrap();

        prop_assert_eq!(obj.get("aborted").unwrap().as_bool(), Some(result.aborted));
        prop_assert_eq!(obj.get("execution_id").unwrap().as_str(), Some(result.execution_id.as_str()));
        prop_assert_eq!(obj.get("workflow_name").unwrap().as_str(), Some(result.workflow_name.as_str()));
        prop_assert_eq!(obj.get("pane_id").unwrap().as_u64(), Some(result.pane_id));
        prop_assert_eq!(obj.get("previous_status").unwrap().as_str(), Some(result.previous_status.as_str()));
        prop_assert_eq!(obj.get("aborted_at_step").unwrap().as_u64(), Some(result.aborted_at_step as u64));

        match &result.reason {
            Some(reason) => prop_assert_eq!(obj.get("reason").unwrap().as_str(), Some(reason.as_str())),
            None => prop_assert!(!obj.contains_key("reason")),
        }

        match result.aborted_at {
            Some(aborted_at) => prop_assert_eq!(obj.get("aborted_at").unwrap().as_u64(), Some(aborted_at)),
            None => prop_assert!(!obj.contains_key("aborted_at")),
        }

        match &result.error_reason {
            Some(error_reason) => prop_assert_eq!(obj.get("error_reason").unwrap().as_str(), Some(error_reason.as_str())),
            None => prop_assert!(!obj.contains_key("error_reason")),
        }
    }

    #[test]
    fn abort_result_success_failure_shape_stays_consistent(result in arb_abort_result()) {
        if result.aborted {
            prop_assert!(result.error_reason.is_none());
            prop_assert!(result.aborted_at.is_some());
        } else {
            prop_assert!(result.error_reason.is_some());
            prop_assert!(result.aborted_at.is_none());
            prop_assert!(result.reason.is_none());
        }
    }
}
