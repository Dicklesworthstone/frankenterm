//! Property-based tests for the NTM parity evaluation engine.
//!
//! Tests cover assertion operator semantics, JSON path resolution, envelope
//! validation, scenario evaluation branching, gate decision logic in
//! build_run_summary, and divergence report consistency.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use serde_json::{Value, json};

use frankenterm_core::ntm_parity::{
    NtmParityAcceptanceMatrix, NtmParityArtifactsContract, NtmParityAssertion,
    NtmParityAssertionOp, NtmParityBlockingGate, NtmParityCommandOutput, NtmParityCorpus,
    NtmParityDivergenceBudget, NtmParityEnvelopeContract, NtmParityGates,
    NtmParityHighPriorityGate, NtmParityPriority, NtmParityScenario, NtmParityScenarioStatus,
    build_divergence_report, build_run_summary, evaluate_scenario,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_priority() -> impl Strategy<Value = NtmParityPriority> {
    prop_oneof![
        Just(NtmParityPriority::Blocking),
        Just(NtmParityPriority::High),
    ]
}

fn arb_assertion_op() -> impl Strategy<Value = NtmParityAssertionOp> {
    prop_oneof![
        Just(NtmParityAssertionOp::Eq),
        Just(NtmParityAssertionOp::IsArray),
        Just(NtmParityAssertionOp::HasAny),
        Just(NtmParityAssertionOp::In),
        Just(NtmParityAssertionOp::Contains),
    ]
}

fn arb_scenario_status() -> impl Strategy<Value = NtmParityScenarioStatus> {
    prop_oneof![
        Just(NtmParityScenarioStatus::Pass),
        Just(NtmParityScenarioStatus::Fail),
        Just(NtmParityScenarioStatus::IntentionalDelta),
        Just(NtmParityScenarioStatus::Untested),
    ]
}

fn arb_id_string() -> impl Strategy<Value = String> {
    "[A-Z][A-Z0-9_-]{2,12}"
}

fn arb_json_primitive() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        (-1000i64..1000i64).prop_map(|n| json!(n)),
        "[a-z]{1,10}".prop_map(|s| json!(s)),
    ]
}

fn arb_eq_assertion(path: String, value: Value) -> NtmParityAssertion {
    NtmParityAssertion {
        path,
        op: NtmParityAssertionOp::Eq,
        value: Some(value),
    }
}

fn arb_command_output(scenario_id: String, stdout: Value) -> NtmParityCommandOutput {
    NtmParityCommandOutput {
        scenario_id,
        command: "ft robot state".to_string(),
        expanded_command: "ft robot --format json state".to_string(),
        exit_code: Some(0),
        duration_ms: 10,
        stdout: stdout.to_string(),
        stderr: String::new(),
        execution_error: None,
    }
}

fn arb_matrix(
    blocking_ids: Vec<String>,
    high_priority_ids: Vec<String>,
) -> NtmParityAcceptanceMatrix {
    NtmParityAcceptanceMatrix {
        schema_version: "1.0".to_string(),
        bead_id: "ft-test".to_string(),
        title: "test matrix".to_string(),
        gates: NtmParityGates {
            blocking_scenarios: NtmParityBlockingGate {
                required_ids: blocking_ids,
                rule: "all must pass".to_string(),
            },
            high_priority_scenarios: NtmParityHighPriorityGate {
                required_pass_rate: 0.9,
                ids: high_priority_ids,
            },
            envelope_contract: NtmParityEnvelopeContract {
                rule: "envelope".to_string(),
            },
            divergence_budget: NtmParityDivergenceBudget {
                max_blocking_divergence: 0,
                max_high_priority_divergence: 1,
                notes: String::new(),
            },
        },
        result_schema: BTreeMap::new(),
        artifacts_contract: NtmParityArtifactsContract {
            required_files: vec!["summary.json".to_string()],
            artifact_root: "artifacts/".to_string(),
        },
    }
}

fn make_scenario_result(
    scenario_id: &str,
    status: NtmParityScenarioStatus,
    priority: &str,
    envelope_valid: bool,
) -> frankenterm_core::ntm_parity::NtmParityScenarioResult {
    frankenterm_core::ntm_parity::NtmParityScenarioResult {
        scenario_id: scenario_id.to_string(),
        status,
        artifacts: vec!["a.json".to_string()],
        notes: String::new(),
        domain: "test".to_string(),
        priority: priority.to_string(),
        command: "ft robot state".to_string(),
        exit_code: Some(0),
        duration_ms: 5,
        envelope_valid,
        matched_branch: if status.is_pass() {
            Some("success".to_string())
        } else {
            None
        },
        assertion_results: Vec::new(),
    }
}

// =============================================================================
// Priority as_str roundtrip
// =============================================================================

proptest! {
    #[test]
    fn priority_as_str_is_stable(priority in arb_priority()) {
        let s = priority.as_str();
        let check = matches!(
            (priority, s),
            (NtmParityPriority::Blocking, "blocking") |
            (NtmParityPriority::High, "high")
        );
        prop_assert!(check, "priority {:?} => {:?}", priority, s);
    }
}

// =============================================================================
// AssertionOp as_str roundtrip
// =============================================================================

proptest! {
    #[test]
    fn assertion_op_as_str_covers_all_variants(op in arb_assertion_op()) {
        let s = op.as_str();
        let valid = matches!(s, "eq" | "is_array" | "has_any" | "in" | "contains");
        prop_assert!(valid, "op {:?} => {:?}", op, s);
    }
}

// =============================================================================
// ScenarioStatus.is_pass
// =============================================================================

proptest! {
    #[test]
    fn is_pass_only_true_for_pass(status in arb_scenario_status()) {
        let result = status.is_pass();
        let expected = matches!(status, NtmParityScenarioStatus::Pass);
        prop_assert_eq!(result, expected);
    }
}

// =============================================================================
// Corpus serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn corpus_serde_roundtrip(
        title in "[a-z ]{3,20}",
        notes in "[a-z ]{0,10}",
    ) {
        let corpus = NtmParityCorpus {
            schema_version: "1.0".to_string(),
            bead_id: "ft-test".to_string(),
            title: title.clone(),
            updated_at: "2026-03-07".to_string(),
            notes: notes.clone(),
            scenarios: vec![],
        };
        let json = serde_json::to_string(&corpus).unwrap();
        let decoded = NtmParityCorpus::from_json_str(&json).unwrap();
        prop_assert_eq!(decoded.title, title);
        prop_assert_eq!(decoded.notes, notes);
        prop_assert_eq!(decoded.scenarios.len(), 0);
    }
}

// =============================================================================
// AcceptanceMatrix serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn matrix_serde_roundtrip(
        max_blocking in 0usize..5,
        max_hp in 0usize..5,
        pass_rate in 0.5f64..1.0,
    ) {
        let mut matrix = arb_matrix(vec!["A".to_string()], vec!["B".to_string()]);
        matrix.gates.divergence_budget.max_blocking_divergence = max_blocking;
        matrix.gates.divergence_budget.max_high_priority_divergence = max_hp;
        matrix.gates.high_priority_scenarios.required_pass_rate = pass_rate;

        let json = serde_json::to_string(&matrix).unwrap();
        let decoded = NtmParityAcceptanceMatrix::from_json_str(&json).unwrap();
        prop_assert_eq!(
            decoded.gates.divergence_budget.max_blocking_divergence,
            max_blocking
        );
        prop_assert_eq!(
            decoded.gates.divergence_budget.max_high_priority_divergence,
            max_hp
        );
    }
}

// =============================================================================
// Eq assertion: passes iff actual == expected
// =============================================================================

proptest! {
    #[test]
    fn eq_assertion_passes_for_matching_value(val in arb_json_primitive()) {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![arb_eq_assertion("$.field".to_string(), val.clone())],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "field": val});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "expected Pass for matching eq, got {:?}", result.status);
    }

    #[test]
    fn eq_assertion_fails_for_mismatched_value(
        val in arb_json_primitive(),
        other in arb_json_primitive(),
    ) {
        // Skip if values happen to be equal
        prop_assume!(val != other);
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![arb_eq_assertion("$.field".to_string(), val)],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "field": other});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        // Should not match success branch
        let check = result.matched_branch.as_deref() != Some("success");
        prop_assert!(check, "eq with mismatched value should not pass success branch");
    }
}

// =============================================================================
// IsArray assertion
// =============================================================================

proptest! {
    #[test]
    fn is_array_passes_for_arrays(elems in prop::collection::vec(arb_json_primitive(), 0..5)) {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.data".to_string(),
                op: NtmParityAssertionOp::IsArray,
                value: None,
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "data": elems});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "is_array should pass for arrays");
    }

    #[test]
    fn is_array_fails_for_non_arrays(val in arb_json_primitive()) {
        prop_assume!(!val.is_array());
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.data".to_string(),
                op: NtmParityAssertionOp::IsArray,
                value: None,
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "data": val});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        let check = result.matched_branch.as_deref() != Some("success");
        prop_assert!(check, "is_array should fail for non-array {:?}", val);
    }
}

// =============================================================================
// Contains assertion
// =============================================================================

proptest! {
    #[test]
    fn contains_passes_when_substring_present(
        prefix in "[a-z]{1,5}",
        needle in "[a-z]{1,5}",
        suffix in "[a-z]{0,5}",
    ) {
        let haystack = format!("{prefix}{needle}{suffix}");
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.msg".to_string(),
                op: NtmParityAssertionOp::Contains,
                value: Some(json!(needle)),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "msg": haystack});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "contains should pass when substring present");
    }
}

// =============================================================================
// In assertion
// =============================================================================

proptest! {
    #[test]
    fn in_assertion_passes_when_value_in_array(
        values in prop::collection::vec("[a-z]{2,6}", 1..5),
        idx in any::<prop::sample::Index>(),
    ) {
        let actual = values[idx.index(values.len())].clone();
        let options: Vec<Value> = values.iter().map(|s| json!(s)).collect();
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.code".to_string(),
                op: NtmParityAssertionOp::In,
                value: Some(Value::Array(options)),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "code": actual});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "in assertion should pass when value in array");
    }

    #[test]
    fn in_assertion_fails_when_value_absent(
        values in prop::collection::vec("[a-z]{2,6}", 1..5),
    ) {
        let actual = "ZZNOTINSET".to_string();
        prop_assume!(!values.contains(&actual));
        let options: Vec<Value> = values.iter().map(|s| json!(s)).collect();
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.code".to_string(),
                op: NtmParityAssertionOp::In,
                value: Some(Value::Array(options)),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "code": actual});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        let check = result.matched_branch.as_deref() != Some("success");
        prop_assert!(check, "in assertion should fail when value absent");
    }
}

// =============================================================================
// HasAny assertion
// =============================================================================

proptest! {
    #[test]
    fn has_any_passes_when_key_exists(
        existing_key in "[a-z]{2,6}",
        extra_keys in prop::collection::vec("[a-z]{2,6}", 0..3),
    ) {
        let mut search_keys = extra_keys;
        search_keys.push(existing_key.clone());
        let key_values: Vec<Value> = search_keys.iter().map(|k| json!(k)).collect();

        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$.data".to_string(),
                op: NtmParityAssertionOp::HasAny,
                value: Some(Value::Array(key_values)),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let mut obj = serde_json::Map::new();
        obj.insert(existing_key, json!("val"));
        let stdout = json!({"ok": true, "data": Value::Object(obj)});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "has_any should pass when key exists");
    }
}

// =============================================================================
// Envelope validation
// =============================================================================

proptest! {
    #[test]
    fn envelope_valid_for_ok_true_json(
        extra_key in "[a-z]{2,6}",
        extra_val in "[a-z]{2,6}",
    ) {
        let stdout = json!({"ok": true, extra_key: extra_val});
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![],
            failure_assertions: vec![],
            artifact_key: "test".to_string(),
        };
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.envelope_valid, "ok:true should satisfy envelope contract");
    }

    #[test]
    fn envelope_valid_for_error_code_json(error_code in "[a-z.]{3,20}") {
        let stdout = json!({"ok": false, "error": {"code": error_code}});
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![],
            failure_assertions: vec![],
            artifact_key: "test".to_string(),
        };
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.envelope_valid, "error.code should satisfy envelope contract");
    }

    #[test]
    fn toon_envelope_passes_for_nonempty_stdout(content in "[a-z][a-z ]{0,29}") {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format toon state".to_string(),
            success_assertions: vec![],
            failure_assertions: vec![],
            artifact_key: "test".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: "TEST".to_string(),
            command: "ft robot --format toon state".to_string(),
            expanded_command: "ft robot --format toon state".to_string(),
            exit_code: Some(0),
            duration_ms: 5,
            stdout: content,
            stderr: String::new(),
            execution_error: None,
        };
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.envelope_valid, "TOON non-empty stdout should be valid envelope");
    }

    #[test]
    fn envelope_invalid_for_non_json_non_toon(garbage in "[a-z]{3,20}") {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![],
            failure_assertions: vec![],
            artifact_key: "test".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: "TEST".to_string(),
            command: "ft robot --format json state".to_string(),
            expanded_command: "ft robot --format json state".to_string(),
            exit_code: Some(0),
            duration_ms: 5,
            stdout: garbage,
            stderr: String::new(),
            execution_error: None,
        };
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(!result.envelope_valid, "non-JSON, non-TOON stdout should fail envelope");
    }
}

// =============================================================================
// Scenario evaluation: intentional delta and untested
// =============================================================================

proptest! {
    #[test]
    fn intentional_delta_note_produces_intentional_delta_status(
        note in "[a-z ]{3,20}",
    ) {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![arb_eq_assertion("$.ok".to_string(), json!(true))],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        // Stdout doesn't match assertions
        let stdout = json!({"ok": false});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(
            &scenario,
            &output,
            vec!["a.json".to_string()],
            Some(&note),
        );
        let check = matches!(result.status, NtmParityScenarioStatus::IntentionalDelta);
        prop_assert!(check, "should be IntentionalDelta when note provided and assertions fail");
    }

    #[test]
    fn execution_error_produces_untested_status(error_msg in "[a-z ]{3,20}") {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![arb_eq_assertion("$.ok".to_string(), json!(true))],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: "TEST".to_string(),
            command: "ft robot state".to_string(),
            expanded_command: "ft robot --format json state".to_string(),
            exit_code: None,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            execution_error: Some(error_msg),
        };
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        let check = matches!(result.status, NtmParityScenarioStatus::Untested);
        prop_assert!(check, "execution_error without intentional delta should be Untested");
    }

    #[test]
    fn envelope_branch_matches_when_assertions_fail_but_json_envelope_is_valid(
        error_code in "[a-z.]{3,24}",
    ) {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json send 0 test".to_string(),
            success_assertions: vec![arb_eq_assertion("$.ok".to_string(), json!(true))],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": false, "error": {"code": error_code}});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);

        prop_assert_eq!(result.matched_branch.as_deref(), Some("envelope"));
        prop_assert!(result.status.is_pass());
        prop_assert!(result.envelope_valid);
        prop_assert_eq!(result.assertion_results.len(), 1);
        prop_assert!(!result.assertion_results[0].passed);
    }
}

// =============================================================================
// JSON path resolution via $stdout and $stderr
// =============================================================================

proptest! {
    #[test]
    fn stdout_path_captures_raw_stdout(content in "[a-z0-9 ]{1,30}") {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$stdout".to_string(),
                op: NtmParityAssertionOp::Contains,
                value: Some(json!(content.clone())),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: "TEST".to_string(),
            command: "ft robot state".to_string(),
            expanded_command: "ft robot --format json state".to_string(),
            exit_code: Some(0),
            duration_ms: 5,
            stdout: content,
            stderr: String::new(),
            execution_error: None,
        };
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        // $stdout contains assertion should find its own content
        let assertion_passed = result.assertion_results.iter().any(|a| a.passed);
        prop_assert!(assertion_passed, "$stdout contains should match its own content");
    }

    #[test]
    fn stderr_path_captures_raw_stderr(content in "[a-z0-9 ]{1,30}") {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![NtmParityAssertion {
                path: "$stderr".to_string(),
                op: NtmParityAssertionOp::Contains,
                value: Some(json!(content.clone())),
            }],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: "TEST".to_string(),
            command: "ft robot state".to_string(),
            expanded_command: "ft robot --format json state".to_string(),
            exit_code: Some(0),
            duration_ms: 5,
            stdout: json!({"ok": true}).to_string(),
            stderr: content,
            execution_error: None,
        };
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        let assertion_passed = result.assertion_results.iter().any(|a| a.passed);
        prop_assert!(assertion_passed, "$stderr contains should match its own content");
    }
}

// =============================================================================
// Nested JSON path resolution
// =============================================================================

proptest! {
    #[test]
    fn nested_path_resolves_deep_values(
        val in arb_json_primitive(),
    ) {
        let scenario = NtmParityScenario {
            id: "TEST".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![arb_eq_assertion("$.data.nested.value".to_string(), val.clone())],
            failure_assertions: Vec::new(),
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, "data": {"nested": {"value": val}}});
        let output = arb_command_output("TEST".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "nested path should resolve deep values");
    }
}

// =============================================================================
// build_run_summary gate logic
// =============================================================================

proptest! {
    #[test]
    fn summary_counts_are_consistent(
        pass_n in 0usize..5,
        fail_n in 0usize..5,
        delta_n in 0usize..3,
        untested_n in 0usize..3,
    ) {
        let total = pass_n + fail_n + delta_n + untested_n;
        let mut results = Vec::with_capacity(total);
        for i in 0..pass_n {
            results.push(make_scenario_result(
                &format!("P{i}"), NtmParityScenarioStatus::Pass, "blocking", true,
            ));
        }
        for i in 0..fail_n {
            results.push(make_scenario_result(
                &format!("F{i}"), NtmParityScenarioStatus::Fail, "blocking", true,
            ));
        }
        for i in 0..delta_n {
            results.push(make_scenario_result(
                &format!("D{i}"), NtmParityScenarioStatus::IntentionalDelta, "high", true,
            ));
        }
        for i in 0..untested_n {
            results.push(make_scenario_result(
                &format!("U{i}"), NtmParityScenarioStatus::Untested, "high", true,
            ));
        }

        let matrix = arb_matrix(vec![], vec![]);
        let summary = build_run_summary("run", &matrix, &results);

        prop_assert_eq!(summary.scenario_count, total);
        prop_assert_eq!(summary.pass_count, pass_n);
        prop_assert_eq!(summary.fail_count, fail_n);
        prop_assert_eq!(summary.intentional_delta_count, delta_n);
        prop_assert_eq!(summary.untested_count, untested_n);
        prop_assert_eq!(summary.divergence_count, fail_n + delta_n + untested_n);
    }

    #[test]
    fn blocking_gate_fails_when_blocking_scenario_fails(
        blocking_id in arb_id_string(),
    ) {
        let result = make_scenario_result(
            &blocking_id, NtmParityScenarioStatus::Fail, "blocking", true,
        );
        let matrix = arb_matrix(vec![blocking_id.clone()], vec![]);
        let summary = build_run_summary("run", &matrix, &[result]);

        // G-01 should fail since there's a blocking failure and budget is 0
        let g01 = summary.gate_results.iter().find(|g| g.gate_id == "G-01").unwrap();
        prop_assert!(!g01.passed, "G-01 should fail when blocking scenario fails with zero budget");
        prop_assert!(!summary.overall_passed, "overall should fail when blocking gate fails");
    }

    #[test]
    fn all_gates_pass_when_everything_passes(n in 1usize..6) {
        let ids: Vec<String> = (0..n).map(|i| format!("S{i}")).collect();
        let results: Vec<_> = ids.iter().map(|id| {
            make_scenario_result(id, NtmParityScenarioStatus::Pass, "blocking", true)
        }).collect();
        let matrix = arb_matrix(ids.clone(), ids);
        let summary = build_run_summary("run", &matrix, &results);

        prop_assert!(summary.overall_passed, "all gates should pass when everything passes");
        prop_assert!(summary.blocking_failures.is_empty());
        prop_assert!(summary.high_priority_failures.is_empty());
        prop_assert!(summary.envelope_violations.is_empty());
    }

    #[test]
    fn envelope_gate_fails_when_envelope_invalid(
        id in arb_id_string(),
    ) {
        let result = make_scenario_result(
            &id, NtmParityScenarioStatus::Pass, "blocking", false,
        );
        let matrix = arb_matrix(vec![], vec![]);
        let summary = build_run_summary("run", &matrix, &[result]);

        let g03 = summary.gate_results.iter().find(|g| g.gate_id == "G-03").unwrap();
        prop_assert!(!g03.passed, "G-03 should fail when envelope is invalid");
        prop_assert_eq!(summary.envelope_violations.len(), 1);
    }

    #[test]
    fn artifacts_gate_fails_when_artifacts_empty(
        id in arb_id_string(),
    ) {
        let mut result = make_scenario_result(
            &id, NtmParityScenarioStatus::Pass, "blocking", true,
        );
        result.artifacts.clear();
        let matrix = arb_matrix(vec![], vec![]);
        let summary = build_run_summary("run", &matrix, &[result]);

        let art_gate = summary.gate_results.iter().find(|g| g.gate_id == "ARTIFACTS").unwrap();
        prop_assert!(!art_gate.passed, "ARTIFACTS gate should fail when artifacts empty");
    }
}

// =============================================================================
// High-priority pass rate gate
// =============================================================================

proptest! {
    #[test]
    fn hp_gate_passes_when_pass_rate_met(
        pass_n in 9usize..=10,
    ) {
        let ids: Vec<String> = (0..10).map(|i| format!("HP{i}")).collect();
        let results: Vec<_> = ids.iter().enumerate().map(|(i, id)| {
            if i < pass_n {
                make_scenario_result(id, NtmParityScenarioStatus::Pass, "high", true)
            } else {
                make_scenario_result(id, NtmParityScenarioStatus::IntentionalDelta, "high", true)
            }
        }).collect();

        let mut matrix = arb_matrix(vec![], ids);
        matrix.gates.high_priority_scenarios.required_pass_rate = 0.9;
        matrix.gates.divergence_budget.max_high_priority_divergence = 1;

        let summary = build_run_summary("run", &matrix, &results);
        let g02 = summary.gate_results.iter().find(|g| g.gate_id == "G-02").unwrap();
        prop_assert!(g02.passed, "G-02 should pass when pass rate >= 0.9 and deltas within budget");
    }

    #[test]
    fn hp_gate_fails_when_intentional_delta_budget_exceeded(prefix in "[A-Z]{2,4}") {
        let ids: Vec<String> = (0..10).map(|i| format!("{prefix}{i}")).collect();
        let results: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                if i < 8 {
                    make_scenario_result(id, NtmParityScenarioStatus::Pass, "high", true)
                } else {
                    make_scenario_result(
                        id,
                        NtmParityScenarioStatus::IntentionalDelta,
                        "high",
                        true,
                    )
                }
            })
            .collect();

        let mut matrix = arb_matrix(vec![], ids);
        matrix.gates.high_priority_scenarios.required_pass_rate = 0.8;
        matrix.gates.divergence_budget.max_high_priority_divergence = 1;

        let summary = build_run_summary("run", &matrix, &results);
        let g02 = summary
            .gate_results
            .iter()
            .find(|g| g.gate_id == "G-02")
            .unwrap();

        prop_assert!(
            !g02.passed,
            "G-02 should fail when intentional deltas exceed budget"
        );
        prop_assert_eq!(summary.high_priority_intentional_deltas.len(), 2);
    }
}

// =============================================================================
// Divergence report consistency
// =============================================================================

proptest! {
    #[test]
    fn divergence_report_counts_match_entries(
        pass_n in 0usize..5,
        fail_n in 0usize..5,
    ) {
        let total = pass_n + fail_n;
        let blocking_ids: Vec<String> = (0..fail_n).map(|i| format!("BLK{i}")).collect();
        let mut results = Vec::with_capacity(total);
        for i in 0..pass_n {
            results.push(make_scenario_result(
                &format!("P{i}"), NtmParityScenarioStatus::Pass, "blocking", true,
            ));
        }
        for i in 0..fail_n {
            results.push(make_scenario_result(
                &format!("BLK{i}"), NtmParityScenarioStatus::Fail, "blocking", true,
            ));
        }

        let matrix = arb_matrix(blocking_ids, vec![]);
        let report = build_divergence_report("run", &matrix, &results);

        prop_assert_eq!(report.total_divergences, report.divergences.len());
        prop_assert_eq!(report.blocking_divergence_count, fail_n);
        // All passing results should not appear in divergences
        let pass_in_divergences = report.divergences.iter()
            .filter(|d| d.status.is_pass())
            .count();
        prop_assert_eq!(pass_in_divergences, 0, "passing results should not be in divergences");
    }

    #[test]
    fn divergence_report_captures_envelope_violations(n in 1usize..5) {
        let results: Vec<_> = (0..n).map(|i| {
            make_scenario_result(
                &format!("E{i}"), NtmParityScenarioStatus::Pass, "blocking", false,
            )
        }).collect();

        let matrix = arb_matrix(vec![], vec![]);
        let report = build_divergence_report("run", &matrix, &results);

        prop_assert_eq!(report.envelope_violation_count, n);
        // Envelope-invalid but status=Pass results appear in divergences
        prop_assert_eq!(report.total_divergences, n);
    }
}

// =============================================================================
// Scenario result metadata preservation
// =============================================================================

proptest! {
    #[test]
    fn evaluate_preserves_scenario_metadata(
        id in arb_id_string(),
        domain in "[a-z]{3,10}",
        priority in arb_priority(),
        duration in 0u64..10000,
    ) {
        let scenario = NtmParityScenario {
            id: id.clone(),
            domain: domain.clone(),
            priority,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![],
            failure_assertions: vec![],
            artifact_key: "test".to_string(),
        };
        let output = NtmParityCommandOutput {
            scenario_id: id.clone(),
            command: "ft robot state".to_string(),
            expanded_command: "ft robot --format json state".to_string(),
            exit_code: Some(0),
            duration_ms: duration,
            stdout: json!({"ok": true}).to_string(),
            stderr: String::new(),
            execution_error: None,
        };
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert_eq!(result.scenario_id, id);
        prop_assert_eq!(result.domain, domain);
        prop_assert_eq!(result.priority, priority.as_str());
        prop_assert_eq!(result.duration_ms, duration);
        prop_assert_eq!(result.exit_code, Some(0));
    }
}

// =============================================================================
// Empty scenarios
// =============================================================================

proptest! {
    #[test]
    fn empty_assertions_with_valid_envelope_passes(
        extra_key in "[a-z]{2,6}",
    ) {
        let scenario = NtmParityScenario {
            id: "EMPTY".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::High,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json state".to_string(),
            success_assertions: vec![],
            failure_assertions: vec![],
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": true, extra_key: "val"});
        let output = arb_command_output("EMPTY".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        // Empty success_assertions means success_passed = true (vacuous truth)
        prop_assert!(result.status.is_pass(), "empty assertions with valid envelope should pass");
        prop_assert_eq!(result.matched_branch.as_deref(), Some("success"));
    }
}

// =============================================================================
// Summary with zero scenarios
// =============================================================================

proptest! {
    #[test]
    fn empty_results_produce_passing_summary(run_id in "[a-z]{3,10}") {
        let matrix = arb_matrix(vec![], vec![]);
        let summary = build_run_summary(&run_id, &matrix, &[]);
        prop_assert!(summary.overall_passed, "empty results should produce passing summary");
        prop_assert_eq!(summary.scenario_count, 0);
        prop_assert_eq!(summary.pass_count, 0);
        prop_assert_eq!(summary.divergence_count, 0);
        prop_assert_eq!(summary.run_id, run_id);
    }
}

// =============================================================================
// Failure branch selection
// =============================================================================

proptest! {
    #[test]
    fn failure_branch_matches_when_success_fails_but_failure_passes(
        error_code in "[a-z.]{3,15}",
    ) {
        let scenario = NtmParityScenario {
            id: "FAILBRANCH".to_string(),
            domain: "test".to_string(),
            priority: NtmParityPriority::Blocking,
            ntm_equivalent: "test".to_string(),
            ft_command: "ft robot --format json send 0 test".to_string(),
            success_assertions: vec![arb_eq_assertion("$.ok".to_string(), json!(true))],
            failure_assertions: vec![NtmParityAssertion {
                path: "$.error.code".to_string(),
                op: NtmParityAssertionOp::Eq,
                value: Some(json!(error_code.clone())),
            }],
            artifact_key: "test".to_string(),
        };
        let stdout = json!({"ok": false, "error": {"code": error_code}});
        let output = arb_command_output("FAILBRANCH".to_string(), stdout);
        let result = evaluate_scenario(&scenario, &output, vec!["a.json".to_string()], None);
        prop_assert!(result.status.is_pass(), "failure branch should satisfy scenario");
        prop_assert_eq!(result.matched_branch.as_deref(), Some("failure"));
    }
}

// =============================================================================
// Scenario result serde roundtrip
// =============================================================================

proptest! {
    #[test]
    fn scenario_result_serde_roundtrip(
        id in arb_id_string(),
        status in arb_scenario_status(),
    ) {
        let result = make_scenario_result(&id, status, "blocking", true);
        let json_str = serde_json::to_string(&result).unwrap();
        let decoded: frankenterm_core::ntm_parity::NtmParityScenarioResult =
            serde_json::from_str(&json_str).unwrap();
        prop_assert_eq!(decoded.scenario_id, id);
        prop_assert_eq!(decoded.status, status);
    }
}
