// Disabled until replay_mcp and replay_robot modules are created.
#![cfg(any())]
//! Property-based interface contract tests for replay (ft-og6q6.6.5).
//!
//! Invariants tested:
//! - PI-1: MCP tool name → Robot command roundtrip
//! - PI-2: Robot request serde → MCP validation agreement
//! - PI-3: DispatchResult serde roundtrip for arbitrary payloads
//! - PI-4: All error codes produce valid DispatchResult::Error
//! - PI-5: InspectData through dispatch preserves all fields
//! - PI-6: DiffData through dispatch preserves all fields
//! - PI-7: ArtifactSummary through dispatch preserves all fields
//! - PI-8: ArtifactAddData through dispatch preserves all fields
//! - PI-9: ArtifactRetireData through dispatch preserves all fields
//! - PI-10: ArtifactPruneData through dispatch preserves all fields
//! - PI-11: RegressionSuiteData through dispatch preserves all fields
//! - PI-12: MCP schema required fields are subset of properties
//! - PI-13: Diff default tolerance consistent across surfaces
//! - PI-14: Error code + message + hint roundtrip through dispatch
//! - PI-15: All MCP schemas tagged with "replay"
//! - PI-16: MCP validation rejects empty required strings
//! - PI-17: MCP validation accepts non-empty strings
//! - PI-18: Optional u64 validation returns correct value or default
//! - PI-19: ArtifactListData count equals artifacts.len()
//! - PI-20: Suite data passed_count + failed_count + errored_count == total

use proptest::prelude::*;
use std::collections::BTreeMap;

use frankenterm_core::replay_mcp::{
    DispatchResult, TOOL_REPLAY_DIFF, TOOL_REPLAY_INSPECT, all_tool_schemas, schema_for,
    validate_optional_u64, validate_required_str,
};
use frankenterm_core::replay_robot::{
    ArtifactAddData, ArtifactListData, ArtifactPruneData, ArtifactRetireData, ArtifactSummary,
    DiffData, InspectData, REPLAY_ERR_ALREADY_RETIRED, REPLAY_ERR_DUPLICATE,
    REPLAY_ERR_FILE_NOT_FOUND, REPLAY_ERR_INTEGRITY_ERROR, REPLAY_ERR_NOT_FOUND,
    REPLAY_ERR_PARSE_ERROR, REPLAY_ERR_SCHEMA_MISMATCH, RegressionSuiteData, ReplayRobotCommand,
};

/// Error codes used across interfaces.
const ALL_ERROR_CODES: &[&str] = &[
    REPLAY_ERR_FILE_NOT_FOUND,
    REPLAY_ERR_PARSE_ERROR,
    REPLAY_ERR_INTEGRITY_ERROR,
    REPLAY_ERR_DUPLICATE,
    REPLAY_ERR_NOT_FOUND,
    REPLAY_ERR_ALREADY_RETIRED,
    REPLAY_ERR_SCHEMA_MISMATCH,
];

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // ── PI-1: MCP → Robot roundtrip ────────────────────────────────────

    #[test]
    fn pi01_mcp_robot_roundtrip(_dummy in 0u8..1) {
        let mapping = vec![
            (TOOL_REPLAY_INSPECT, "replay.inspect"),
            (TOOL_REPLAY_DIFF, "replay.diff"),
            ("wa.replay.regression", "replay.regression_suite"),
            ("wa.replay.artifact_list", "replay.artifact.list"),
            ("wa.replay.artifact_add", "replay.artifact.add"),
            ("wa.replay.artifact_retire", "replay.artifact.retire"),
        ];
        for (mcp_name, robot_str) in &mapping {
            let cmd = ReplayRobotCommand::from_str_command(robot_str);
            prop_assert!(cmd.is_some(), "Robot parse failed for {}", robot_str);
            prop_assert_eq!(cmd.unwrap().as_str(), *robot_str);

            let schema = schema_for(mcp_name);
            prop_assert!(schema.is_some(), "MCP schema missing for {}", mcp_name);
        }
    }

    // ── PI-2: Robot request serde → MCP validation agreement ──────────

    #[test]
    fn pi02_diff_request_serde_mcp_agree(tol in 1u64..10000) {
        let args = serde_json::json!({
            "baseline": "base.ftreplay",
            "candidate": "cand.ftreplay",
            "tolerance_ms": tol
        });
        let mcp_tol = validate_optional_u64(&args, "tolerance_ms", 100);
        prop_assert_eq!(mcp_tol, tol);

        let req: frankenterm_core::replay_robot::DiffRequest =
            serde_json::from_value(args).unwrap();
        prop_assert_eq!(req.tolerance_ms, tol);
    }

    // ── PI-3: DispatchResult serde roundtrip ──────────────────────────

    #[test]
    fn pi03_dispatch_ok_roundtrip(val in 0u64..10000) {
        let result = DispatchResult::ok(serde_json::json!({"count": val}));
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── PI-4: Error codes in dispatch ─────────────────────────────────

    #[test]
    fn pi04_error_codes_dispatch(idx in 0usize..7, msg in "[a-z ]{5,30}") {
        let code = ALL_ERROR_CODES[idx];
        let result = DispatchResult::error(code, &msg);
        prop_assert!(result.is_error());
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── PI-5: InspectData through dispatch ────────────────────────────

    #[test]
    fn pi05_inspect_data_dispatch(
        events in 0u64..100,
        panes in 0u64..50,
        rules in 0u64..50,
        span in 0u64..100000
    ) {
        let data = InspectData {
            artifact_path: "test.ftreplay".into(),
            event_count: events,
            pane_count: panes,
            rule_count: rules,
            time_span_ms: span,
            decision_types: vec!["PatternMatch".into()],
            integrity_ok: true,
        };
        let json_val = serde_json::to_value(&data).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        if let DispatchResult::Ok { data: d } = restored {
            let inspect: InspectData = serde_json::from_value(d).unwrap();
            prop_assert_eq!(inspect, data);
        } else {
            prop_assert!(false, "expected Ok variant");
        }
    }

    // ── PI-6: DiffData through dispatch ───────────────────────────────

    #[test]
    fn pi06_diff_data_dispatch(
        exit_code in 0i32..4,
        divergences in 0u64..100
    ) {
        let data = DiffData {
            passed: exit_code == 0,
            exit_code,
            divergence_count: divergences,
            recommendation: "Accept".into(),
            gate_result: "Pass".into(),
            severity_counts: BTreeMap::new(),
        };
        let json_val = serde_json::to_value(&data).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        prop_assert_eq!(restored, dispatch);
    }

    // ── PI-7: ArtifactSummary through dispatch ────────────────────────

    #[test]
    fn pi07_artifact_summary_dispatch(
        events in 0u64..1000,
        size in 0u64..100000
    ) {
        let summary = ArtifactSummary {
            path: "artifact.ftreplay".into(),
            label: "test".into(),
            tier: "T1".into(),
            status: "active".into(),
            event_count: events,
            size_bytes: size,
            sha256: "abc123".into(),
        };
        let list = ArtifactListData {
            count: 1,
            artifacts: vec![summary],
        };
        let json_val = serde_json::to_value(&list).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        prop_assert_eq!(restored, dispatch);
    }

    // ── PI-8: ArtifactAddData through dispatch ────────────────────────

    #[test]
    fn pi08_artifact_add_dispatch(events in 0u64..1000, size in 0u64..100000) {
        let data = ArtifactAddData {
            path: "new.ftreplay".into(),
            sha256: "deadbeef".into(),
            event_count: events,
            size_bytes: size,
        };
        let json_val = serde_json::to_value(&data).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        prop_assert_eq!(restored, dispatch);
    }

    // ── PI-9: ArtifactRetireData through dispatch ─────────────────────

    #[test]
    fn pi09_artifact_retire_dispatch(ts in 1000000000000u64..2000000000000u64) {
        let data = ArtifactRetireData {
            path: "old.ftreplay".into(),
            reason: "superseded".into(),
            retired_at_ms: ts,
        };
        let json_val = serde_json::to_value(&data).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        prop_assert_eq!(restored, dispatch);
    }

    // ── PI-10: ArtifactPruneData through dispatch ─────────────────────

    #[test]
    fn pi10_artifact_prune_dispatch(count in 0u64..100, bytes in 0u64..1000000) {
        let data = ArtifactPruneData {
            pruned_count: count,
            bytes_freed: bytes,
            dry_run: false,
            pruned_paths: vec![],
        };
        let json_val = serde_json::to_value(&data).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        prop_assert_eq!(restored, dispatch);
    }

    // ── PI-11: RegressionSuiteData through dispatch ───────────────────

    #[test]
    fn pi11_suite_data_dispatch(
        total in 1u64..50,
        passed in 0u64..50
    ) {
        let passed_count = passed.min(total);
        let failed_count = total - passed_count;
        let data = RegressionSuiteData {
            passed: failed_count == 0,
            total_artifacts: total,
            passed_count,
            failed_count,
            errored_count: 0,
            results: vec![],
        };
        let json_val = serde_json::to_value(&data).unwrap();
        let dispatch = DispatchResult::ok(json_val);
        let roundtrip = serde_json::to_string(&dispatch).unwrap();
        let restored: DispatchResult = serde_json::from_str(&roundtrip).unwrap();
        prop_assert_eq!(restored, dispatch);
    }

    // ── PI-12: Required fields are subset of properties ───────────────

    #[test]
    fn pi12_required_in_properties(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            if let Some(required) = schema.input_schema["required"].as_array() {
                let props = schema.input_schema["properties"].as_object().unwrap();
                for field in required {
                    let name = field.as_str().unwrap();
                    prop_assert!(
                        props.contains_key(name),
                        "schema {} requires '{}' not in properties",
                        schema.name, name
                    );
                }
            }
        }
    }

    // ── PI-13: Diff tolerance consistent ──────────────────────────────

    #[test]
    fn pi13_diff_tolerance_consistent(_dummy in 0u8..1) {
        let schema = schema_for(TOOL_REPLAY_DIFF).unwrap();
        let schema_default = schema.input_schema["properties"]["tolerance_ms"]["default"]
            .as_u64()
            .unwrap();

        // Robot serde default
        let req: frankenterm_core::replay_robot::DiffRequest =
            serde_json::from_value(serde_json::json!({
                "baseline": "a", "candidate": "b"
            })).unwrap();
        prop_assert_eq!(req.tolerance_ms, schema_default);
    }

    // ── PI-14: Error code + message + hint roundtrip ──────────────────

    #[test]
    fn pi14_error_hint_roundtrip(
        idx in 0usize..7,
        msg in "[a-z ]{3,20}",
        hint in "[a-z ]{3,20}"
    ) {
        let code = ALL_ERROR_CODES[idx];
        let result = DispatchResult::error_with_hint(code, &msg, &hint);
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored, result);
    }

    // ── PI-15: All schemas tagged "replay" ────────────────────────────

    #[test]
    fn pi15_all_schemas_tagged(_dummy in 0u8..1) {
        for schema in all_tool_schemas() {
            prop_assert!(
                schema.tags.contains(&"replay".to_string()),
                "schema {} missing 'replay' tag", schema.name
            );
        }
    }

    // ── PI-16: Validation rejects empty strings ───────────────────────

    #[test]
    fn pi16_rejects_empty(field in "[a-z]{3,8}") {
        let args = serde_json::json!({&field: ""});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_err());
    }

    // ── PI-17: Validation accepts non-empty strings ───────────────────

    #[test]
    fn pi17_accepts_nonempty(field in "[a-z]{3,8}", val in "[a-z]{1,20}") {
        let args = serde_json::json!({&field: &val});
        let result = validate_required_str(&args, &field);
        prop_assert!(result.is_ok());
        prop_assert_eq!(result.unwrap(), val);
    }

    // ── PI-18: Optional u64 returns value or default ──────────────────

    #[test]
    fn pi18_optional_u64_behavior(val in 0u64..10000, default_val in 0u64..10000) {
        // With value present
        let args = serde_json::json!({"field": val});
        let result = validate_optional_u64(&args, "field", default_val);
        prop_assert_eq!(result, val);

        // Without value
        let args = serde_json::json!({});
        let result = validate_optional_u64(&args, "field", default_val);
        prop_assert_eq!(result, default_val);
    }

    // ── PI-19: ArtifactListData count equals artifacts.len() ──────────

    #[test]
    fn pi19_list_count_matches_len(n in 0u64..20) {
        let artifacts: Vec<ArtifactSummary> = (0..n)
            .map(|i| ArtifactSummary {
                path: format!("artifact_{i}.ftreplay"),
                label: "test".into(),
                tier: "T1".into(),
                status: "active".into(),
                event_count: i,
                size_bytes: i * 100,
                sha256: format!("sha_{i}"),
            })
            .collect();
        let data = ArtifactListData {
            count: n,
            artifacts,
        };
        prop_assert_eq!(data.count, data.artifacts.len() as u64);
    }

    // ── PI-20: Suite totals are consistent ────────────────────────────

    #[test]
    fn pi20_suite_totals(passed in 0u64..30, failed in 0u64..20, errored in 0u64..10) {
        let total = passed + failed + errored;
        let data = RegressionSuiteData {
            passed: failed == 0 && errored == 0,
            total_artifacts: total,
            passed_count: passed,
            failed_count: failed,
            errored_count: errored,
            results: vec![],
        };
        prop_assert_eq!(
            data.passed_count + data.failed_count + data.errored_count,
            data.total_artifacts
        );
        if data.failed_count == 0 && data.errored_count == 0 {
            prop_assert!(data.passed);
        } else {
            let is_passed = data.passed;
            prop_assert!(!is_passed);
        }
    }
}
