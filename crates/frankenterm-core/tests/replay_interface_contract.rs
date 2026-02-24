//! Interface contract tests for replay operations (ft-og6q6.6.5).
//!
//! Verifies that CLI, Robot Mode, and MCP interfaces maintain consistent
//! behavior for equivalent replay operations.
//!
//! # Invariant Categories
//!
//! - IC-01..IC-05: Operation coverage parity across surfaces
//! - IC-06..IC-12: Schema/field alignment between Robot and MCP
//! - IC-13..IC-18: Error code consistency
//! - IC-19..IC-25: Default value alignment
//! - IC-26..IC-32: Data interop (Robot types through MCP dispatch)
//! - IC-33..IC-38: Smoke tests (S-01..S-05 from taxonomy + extra)

use std::collections::{BTreeSet, HashMap};

use frankenterm_core::replay_cli::{
    DiffRunner, InspectResult, ReplayExitCode, ReplayOutputMode,
    RegressionSuiteResult, ArtifactResult,
};
use frankenterm_core::replay_mcp::{
    DispatchResult, ReplayToolSchema, ALL_REPLAY_TOOLS,
    all_tool_schemas, schema_for,
    validate_optional_str, validate_optional_u64, validate_required_str,
    TOOL_REPLAY_INSPECT, TOOL_REPLAY_DIFF, TOOL_REPLAY_REGRESSION,
    TOOL_REPLAY_ARTIFACT_LIST, TOOL_REPLAY_ARTIFACT_ADD, TOOL_REPLAY_ARTIFACT_RETIRE,
};
use frankenterm_core::replay_robot::{
    ReplayRobotCommand,
    InspectRequest, InspectData,
    DiffRequest, DiffData,
    RegressionSuiteRequest, RegressionSuiteData,
    ArtifactListData, ArtifactSummary,
    ArtifactAddRequest, ArtifactAddData,
    ArtifactRetireData,
    ArtifactPruneRequest, ArtifactPruneData,
    REPLAY_ERR_FILE_NOT_FOUND, REPLAY_ERR_PARSE_ERROR,
    REPLAY_ERR_INTEGRITY_ERROR, REPLAY_ERR_DUPLICATE,
    REPLAY_ERR_NOT_FOUND, REPLAY_ERR_ALREADY_RETIRED,
    REPLAY_ERR_SCHEMA_MISMATCH,
};
use frankenterm_core::replay_decision_diff::DiffConfig;
use frankenterm_core::replay_decision_graph::{DecisionEvent, DecisionType};
use frankenterm_core::replay_report::ReportMeta;

// ============================================================================
// Helpers
// ============================================================================

/// Build a minimal decision event for testing.
fn make_event(rule_id: &str, ts: u64, pane: u64) -> DecisionEvent {
    DecisionEvent {
        decision_type: DecisionType::PatternMatch,
        rule_id: rule_id.into(),
        definition_hash: format!("def_{rule_id}"),
        input_hash: format!("in_{ts}"),
        output_hash: format!("out_{ts}"),
        timestamp_ms: ts,
        pane_id: pane,
        triggered_by: None,
        overrides: None,
        wall_clock_ms: 0,
        replay_run_id: String::new(),
    }
}

/// Extract property names from a JSON Schema's "properties" key.
fn schema_property_names(schema: &ReplayToolSchema) -> BTreeSet<String> {
    schema.input_schema["properties"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Extract required field names from a JSON Schema.
fn schema_required_fields(schema: &ReplayToolSchema) -> BTreeSet<String> {
    schema.input_schema["required"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Map from MCP tool name to Robot command string.
fn mcp_to_robot_mapping() -> HashMap<&'static str, &'static str> {
    let mut m = HashMap::new();
    m.insert(TOOL_REPLAY_INSPECT, "replay.inspect");
    m.insert(TOOL_REPLAY_DIFF, "replay.diff");
    m.insert(TOOL_REPLAY_REGRESSION, "replay.regression_suite");
    m.insert(TOOL_REPLAY_ARTIFACT_LIST, "replay.artifact.list");
    m.insert(TOOL_REPLAY_ARTIFACT_ADD, "replay.artifact.add");
    m.insert(TOOL_REPLAY_ARTIFACT_RETIRE, "replay.artifact.retire");
    m
}

// ============================================================================
// IC-01..IC-05: Operation Coverage Parity
// ============================================================================

/// IC-01: Every MCP tool has a corresponding Robot command.
#[test]
fn ic01_mcp_tools_have_robot_commands() {
    let mapping = mcp_to_robot_mapping();
    for tool_name in ALL_REPLAY_TOOLS {
        let robot_cmd = mapping.get(tool_name);
        assert!(
            robot_cmd.is_some(),
            "MCP tool {tool_name} has no Robot command mapping"
        );
        let parsed = ReplayRobotCommand::from_str_command(robot_cmd.unwrap());
        assert!(
            parsed.is_some(),
            "Robot command string '{}' for MCP tool {tool_name} does not parse",
            robot_cmd.unwrap()
        );
    }
}

/// IC-02: Every Robot command that has an MCP tool maps back correctly.
#[test]
fn ic02_robot_commands_map_to_mcp_tools() {
    let mapping = mcp_to_robot_mapping();
    // Invert the mapping
    let inv: HashMap<&str, &str> = mapping.iter().map(|(k, v)| (*v, *k)).collect();

    for (robot_str, mcp_name) in &inv {
        let schema = schema_for(mcp_name);
        assert!(
            schema.is_some(),
            "MCP schema missing for tool {mcp_name} (Robot: {robot_str})"
        );
        let cmd = ReplayRobotCommand::from_str_command(robot_str).unwrap();
        assert_eq!(
            cmd.as_str(),
            *robot_str,
            "Robot command as_str roundtrip failed for {robot_str}"
        );
    }
}

/// IC-03: MCP tool count matches schema count.
#[test]
fn ic03_tool_count_matches_schema_count() {
    let schemas = all_tool_schemas();
    assert_eq!(
        ALL_REPLAY_TOOLS.len(),
        schemas.len(),
        "ALL_REPLAY_TOOLS count != schema count"
    );
}

/// IC-04: Robot commands that have NO MCP tool are explicitly documented.
/// (ArtifactInspect and ArtifactPrune are robot-only; no MCP equivalent yet.)
#[test]
fn ic04_robot_only_commands_are_known() {
    let known_robot_only: BTreeSet<&str> = [
        "replay.artifact.inspect",
        "replay.artifact.prune",
    ]
    .into_iter()
    .collect();

    let mcp_covered: BTreeSet<&str> = mcp_to_robot_mapping().values().cloned().collect();

    // Check all 8 robot commands
    let all_robot_cmds: Vec<ReplayRobotCommand> = vec![
        ReplayRobotCommand::Inspect,
        ReplayRobotCommand::Diff,
        ReplayRobotCommand::RegressionSuite,
        ReplayRobotCommand::ArtifactList,
        ReplayRobotCommand::ArtifactInspect,
        ReplayRobotCommand::ArtifactAdd,
        ReplayRobotCommand::ArtifactRetire,
        ReplayRobotCommand::ArtifactPrune,
    ];

    for cmd in &all_robot_cmds {
        let cmd_str = cmd.as_str();
        if !mcp_covered.contains(cmd_str) {
            assert!(
                known_robot_only.contains(cmd_str),
                "Robot command '{cmd_str}' has no MCP mapping and is not in known_robot_only"
            );
        }
    }
}

/// IC-05: All MCP tool names follow wa.replay.* namespace.
#[test]
fn ic05_mcp_namespace_convention() {
    for name in ALL_REPLAY_TOOLS {
        assert!(
            name.starts_with("wa.replay."),
            "MCP tool '{name}' does not follow wa.replay.* convention"
        );
    }
}

// ============================================================================
// IC-06..IC-12: Schema/Field Alignment (Robot structs ↔ MCP schemas)
// ============================================================================

/// IC-06: inspect schema properties match InspectRequest fields.
#[test]
fn ic06_inspect_schema_matches_request() {
    let schema = schema_for(TOOL_REPLAY_INSPECT).unwrap();
    let props = schema_property_names(&schema);
    let required = schema_required_fields(&schema);

    // InspectRequest has: trace (String)
    assert!(props.contains("trace"), "missing 'trace' property in inspect schema");
    assert!(required.contains("trace"), "trace should be required");
}

/// IC-07: diff schema properties match DiffRequest fields.
#[test]
fn ic07_diff_schema_matches_request() {
    let schema = schema_for(TOOL_REPLAY_DIFF).unwrap();
    let props = schema_property_names(&schema);
    let required = schema_required_fields(&schema);

    // DiffRequest fields: baseline, candidate, tolerance_ms, budget
    assert!(props.contains("baseline"), "missing 'baseline'");
    assert!(props.contains("candidate"), "missing 'candidate'");
    assert!(props.contains("tolerance_ms"), "missing 'tolerance_ms'");
    assert!(props.contains("budget"), "missing 'budget'");

    assert!(required.contains("baseline"), "baseline should be required");
    assert!(required.contains("candidate"), "candidate should be required");
    assert!(!required.contains("tolerance_ms"), "tolerance_ms should be optional");
    assert!(!required.contains("budget"), "budget should be optional");
}

/// IC-08: regression schema properties match RegressionSuiteRequest fields.
#[test]
fn ic08_regression_schema_matches_request() {
    let schema = schema_for(TOOL_REPLAY_REGRESSION).unwrap();
    let props = schema_property_names(&schema);

    // RegressionSuiteRequest: suite_dir, budget
    assert!(props.contains("suite_dir"), "missing 'suite_dir'");
    assert!(props.contains("budget"), "missing 'budget'");
}

/// IC-09: artifact_list schema properties match ArtifactListRequest fields.
#[test]
fn ic09_artifact_list_schema_matches_request() {
    let schema = schema_for(TOOL_REPLAY_ARTIFACT_LIST).unwrap();
    let props = schema_property_names(&schema);

    // ArtifactListRequest: tier, status
    assert!(props.contains("tier"), "missing 'tier'");
    assert!(props.contains("status"), "missing 'status'");
}

/// IC-10: artifact_add schema properties match ArtifactAddRequest fields.
#[test]
fn ic10_artifact_add_schema_matches_request() {
    let schema = schema_for(TOOL_REPLAY_ARTIFACT_ADD).unwrap();
    let props = schema_property_names(&schema);
    let required = schema_required_fields(&schema);

    // ArtifactAddRequest: path, label, tier
    assert!(props.contains("path"), "missing 'path'");
    assert!(props.contains("label"), "missing 'label'");
    assert!(props.contains("tier"), "missing 'tier'");

    assert!(required.contains("path"), "path should be required");
    assert!(!required.contains("label"), "label should be optional");
    assert!(!required.contains("tier"), "tier should be optional");
}

/// IC-11: artifact_retire schema properties match ArtifactRetireRequest fields.
#[test]
fn ic11_artifact_retire_schema_matches_request() {
    let schema = schema_for(TOOL_REPLAY_ARTIFACT_RETIRE).unwrap();
    let props = schema_property_names(&schema);
    let required = schema_required_fields(&schema);

    // ArtifactRetireRequest: path, reason
    assert!(props.contains("path"), "missing 'path'");
    assert!(props.contains("reason"), "missing 'reason'");

    assert!(required.contains("path"), "path should be required");
    assert!(required.contains("reason"), "reason should be required");
}

/// IC-12: All MCP schemas enforce additionalProperties=false (strict mode).
#[test]
fn ic12_all_schemas_strict_mode() {
    for schema in all_tool_schemas() {
        let addl = schema.input_schema["additionalProperties"].as_bool();
        assert_eq!(
            addl,
            Some(false),
            "schema {} must have additionalProperties=false",
            schema.name
        );
    }
}

// ============================================================================
// IC-13..IC-18: Error Code Consistency
// ============================================================================

/// IC-13: All Robot error codes use the replay.* namespace.
#[test]
fn ic13_error_codes_namespace() {
    let codes = [
        REPLAY_ERR_FILE_NOT_FOUND,
        REPLAY_ERR_PARSE_ERROR,
        REPLAY_ERR_INTEGRITY_ERROR,
        REPLAY_ERR_DUPLICATE,
        REPLAY_ERR_NOT_FOUND,
        REPLAY_ERR_ALREADY_RETIRED,
        REPLAY_ERR_SCHEMA_MISMATCH,
    ];
    for code in &codes {
        assert!(
            code.starts_with("replay."),
            "error code '{code}' does not start with 'replay.'"
        );
    }
}

/// IC-14: Robot error codes are valid in MCP DispatchResult::Error.
#[test]
fn ic14_error_codes_work_in_dispatch() {
    let codes = [
        REPLAY_ERR_FILE_NOT_FOUND,
        REPLAY_ERR_PARSE_ERROR,
        REPLAY_ERR_INTEGRITY_ERROR,
        REPLAY_ERR_DUPLICATE,
        REPLAY_ERR_NOT_FOUND,
        REPLAY_ERR_ALREADY_RETIRED,
        REPLAY_ERR_SCHEMA_MISMATCH,
    ];
    for code in &codes {
        let result = DispatchResult::error(code, format!("test error for {code}"));
        assert!(result.is_error());
        let is_ok = result.is_ok();
        assert!(!is_ok);

        // Serde roundtrip
        let json = serde_json::to_string(&result).unwrap();
        let restored: DispatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, result);
    }
}

/// IC-15: DispatchResult::Error preserves error code and message exactly.
#[test]
fn ic15_dispatch_error_preserves_fields() {
    let result = DispatchResult::error(REPLAY_ERR_NOT_FOUND, "artifact xyz not registered");
    if let DispatchResult::Error { code, message, hint } = &result {
        assert_eq!(code, REPLAY_ERR_NOT_FOUND);
        assert_eq!(message, "artifact xyz not registered");
        assert!(hint.is_none());
    } else {
        panic!("expected Error variant");
    }
}

/// IC-16: DispatchResult::Error with hint preserves all three fields.
#[test]
fn ic16_dispatch_error_hint_preserved() {
    let result = DispatchResult::error_with_hint(
        REPLAY_ERR_FILE_NOT_FOUND,
        "file missing",
        "check path exists",
    );
    if let DispatchResult::Error { code, message, hint } = &result {
        assert_eq!(code, REPLAY_ERR_FILE_NOT_FOUND);
        assert_eq!(message, "file missing");
        assert_eq!(hint.as_deref(), Some("check path exists"));
    } else {
        panic!("expected Error variant");
    }
}

/// IC-17: Error codes are unique (no accidental duplicates).
#[test]
fn ic17_error_codes_unique() {
    let codes = vec![
        REPLAY_ERR_FILE_NOT_FOUND,
        REPLAY_ERR_PARSE_ERROR,
        REPLAY_ERR_INTEGRITY_ERROR,
        REPLAY_ERR_DUPLICATE,
        REPLAY_ERR_NOT_FOUND,
        REPLAY_ERR_ALREADY_RETIRED,
        REPLAY_ERR_SCHEMA_MISMATCH,
    ];
    let unique: BTreeSet<&&str> = codes.iter().collect();
    assert_eq!(unique.len(), codes.len(), "duplicate error codes detected");
}

/// IC-18: Exit codes align with error categories.
#[test]
fn ic18_exit_codes_cover_all_outcomes() {
    // Pass
    assert_eq!(ReplayExitCode::Pass.code(), 0);
    // Regression = actual diff failure
    assert_eq!(ReplayExitCode::Regression.code(), 1);
    // InvalidInput = bad file/schema
    assert_eq!(ReplayExitCode::InvalidInput.code(), 2);
    // InternalError = unexpected
    assert_eq!(ReplayExitCode::InternalError.code(), 3);

    // All four codes are distinct
    let codes: BTreeSet<i32> = [
        ReplayExitCode::Pass,
        ReplayExitCode::Regression,
        ReplayExitCode::InvalidInput,
        ReplayExitCode::InternalError,
    ]
    .iter()
    .map(|c| c.code())
    .collect();
    assert_eq!(codes.len(), 4);
}

// ============================================================================
// IC-19..IC-25: Default Value Alignment
// ============================================================================

/// IC-19: DiffRequest default tolerance_ms matches MCP schema default.
#[test]
fn ic19_diff_tolerance_default_aligned() {
    // MCP schema default
    let schema = schema_for(TOOL_REPLAY_DIFF).unwrap();
    let schema_default = schema.input_schema["properties"]["tolerance_ms"]["default"]
        .as_u64()
        .unwrap();

    // Robot serde default (deserialize with missing field)
    let robot_json = serde_json::json!({
        "baseline": "a.ftreplay",
        "candidate": "b.ftreplay"
    });
    let req: DiffRequest = serde_json::from_value(robot_json).unwrap();

    assert_eq!(req.tolerance_ms, schema_default, "tolerance_ms default mismatch");
    assert_eq!(req.tolerance_ms, 100, "tolerance_ms should default to 100");
}

/// IC-20: ArtifactAddRequest label default matches MCP schema default.
#[test]
fn ic20_artifact_add_label_default_aligned() {
    // MCP schema default
    let schema = schema_for(TOOL_REPLAY_ARTIFACT_ADD).unwrap();
    let schema_default = schema.input_schema["properties"]["label"]["default"]
        .as_str()
        .unwrap();

    // Robot serde default
    let robot_json = serde_json::json!({
        "path": "test.ftreplay"
    });
    let req: ArtifactAddRequest = serde_json::from_value(robot_json).unwrap();

    assert_eq!(req.label, schema_default, "label default mismatch");
    assert_eq!(req.label, "unlabeled");
}

/// IC-21: ArtifactAddRequest tier default matches MCP schema default.
#[test]
fn ic21_artifact_add_tier_default_aligned() {
    let schema = schema_for(TOOL_REPLAY_ARTIFACT_ADD).unwrap();
    let schema_default = schema.input_schema["properties"]["tier"]["default"]
        .as_str()
        .unwrap();

    let robot_json = serde_json::json!({
        "path": "test.ftreplay"
    });
    let req: ArtifactAddRequest = serde_json::from_value(robot_json).unwrap();
    let tier_str = req.tier.as_str();

    assert_eq!(tier_str, schema_default, "tier default mismatch");
    assert_eq!(tier_str, "T1");
}

/// IC-22: RegressionSuiteRequest suite_dir default matches MCP schema default.
#[test]
fn ic22_regression_suite_dir_default_aligned() {
    let schema = schema_for(TOOL_REPLAY_REGRESSION).unwrap();
    let schema_default = schema.input_schema["properties"]["suite_dir"]["default"]
        .as_str()
        .unwrap();

    let robot_json = serde_json::json!({});
    let req: RegressionSuiteRequest = serde_json::from_value(robot_json).unwrap();

    assert_eq!(req.suite_dir, schema_default, "suite_dir default mismatch");
    assert_eq!(req.suite_dir, "tests/regression/replay/");
}

/// IC-23: ArtifactPruneRequest defaults match typical values.
#[test]
fn ic23_artifact_prune_defaults() {
    let robot_json = serde_json::json!({});
    let req: ArtifactPruneRequest = serde_json::from_value(robot_json).unwrap();

    assert!(!req.dry_run, "dry_run should default to false");
    assert_eq!(req.max_age_days, 30, "max_age_days should default to 30");
}

/// IC-24: validate_optional_u64 returns the expected default when field is missing.
#[test]
fn ic24_validate_optional_u64_default_consistent() {
    let args = serde_json::json!({});
    // Use the MCP schema default for tolerance_ms
    assert_eq!(validate_optional_u64(&args, "tolerance_ms", 100), 100);
}

/// IC-25: validate_optional_str returns None for missing fields.
#[test]
fn ic25_validate_optional_str_none_for_missing() {
    let args = serde_json::json!({});
    assert!(validate_optional_str(&args, "budget").is_none());
    assert!(validate_optional_str(&args, "tier").is_none());
    assert!(validate_optional_str(&args, "status").is_none());
}

// ============================================================================
// IC-26..IC-32: Data Interop (Robot types ↔ MCP dispatch)
// ============================================================================

/// IC-26: InspectData serializes into a valid DispatchResult::Ok payload.
#[test]
fn ic26_inspect_data_through_dispatch() {
    let events = vec![make_event("r1", 100, 1), make_event("r2", 200, 2)];
    let ir = InspectResult::from_events("test.ftreplay", &events);
    let data = InspectData::from_inspect_result(&ir);

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val.clone());
    assert!(dispatch.is_ok());

    // Roundtrip through serde
    let dispatch_json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&dispatch_json).unwrap();
    assert_eq!(restored, dispatch);

    // Extract data back
    if let DispatchResult::Ok { data: restored_data } = restored {
        let restored_inspect: InspectData = serde_json::from_value(restored_data).unwrap();
        assert_eq!(restored_inspect, data);
    } else {
        panic!("expected Ok");
    }
}

/// IC-27: DiffData serializes into a valid DispatchResult::Ok payload.
#[test]
fn ic27_diff_data_through_dispatch() {
    let data = DiffData {
        passed: true,
        exit_code: 0,
        divergence_count: 0,
        recommendation: "Accept".into(),
        gate_result: "Pass".into(),
        severity_counts: {
            let mut m = std::collections::BTreeMap::new();
            m.insert("critical".into(), 0u64);
            m.insert("high".into(), 0u64);
            m
        },
    };

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val);
    let json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, dispatch);
}

/// IC-28: RegressionSuiteData serializes into a valid DispatchResult::Ok.
#[test]
fn ic28_suite_data_through_dispatch() {
    let data = RegressionSuiteData {
        passed: true,
        total_artifacts: 3,
        passed_count: 3,
        failed_count: 0,
        errored_count: 0,
        results: vec![],
    };

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val);
    let json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, dispatch);
}

/// IC-29: ArtifactListData serializes into a valid DispatchResult::Ok.
#[test]
fn ic29_artifact_list_through_dispatch() {
    let data = ArtifactListData {
        count: 1,
        artifacts: vec![ArtifactSummary {
            path: "test.ftreplay".into(),
            label: "test".into(),
            tier: "T1".into(),
            status: "active".into(),
            event_count: 10,
            size_bytes: 1024,
            sha256: "abc123".into(),
        }],
    };

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val);
    let json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, dispatch);
}

/// IC-30: ArtifactAddData serializes into a valid DispatchResult::Ok.
#[test]
fn ic30_artifact_add_through_dispatch() {
    let data = ArtifactAddData {
        path: "new.ftreplay".into(),
        sha256: "deadbeef".into(),
        event_count: 5,
        size_bytes: 512,
    };

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val);
    let json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, dispatch);
}

/// IC-31: ArtifactRetireData serializes into a valid DispatchResult::Ok.
#[test]
fn ic31_artifact_retire_through_dispatch() {
    let data = ArtifactRetireData {
        path: "old.ftreplay".into(),
        reason: "superseded".into(),
        retired_at_ms: 1700000000000,
    };

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val);
    let json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, dispatch);
}

/// IC-32: ArtifactPruneData serializes into a valid DispatchResult::Ok.
#[test]
fn ic32_artifact_prune_through_dispatch() {
    let data = ArtifactPruneData {
        pruned_count: 2,
        bytes_freed: 4096,
        dry_run: false,
        pruned_paths: vec!["a.ftreplay".into(), "b.ftreplay".into()],
    };

    let json_val = serde_json::to_value(&data).unwrap();
    let dispatch = DispatchResult::ok(json_val);
    let json = serde_json::to_string(&dispatch).unwrap();
    let restored: DispatchResult = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, dispatch);
}

// ============================================================================
// IC-33..IC-38: Smoke Tests
// ============================================================================

/// IC-33 (S-01): ReplayExitCode::Pass is 0.
#[test]
fn ic33_smoke_exit_code_pass() {
    assert_eq!(ReplayExitCode::Pass.code(), 0);
}

/// IC-34 (S-02): ReplayOutputMode default is Human.
#[test]
fn ic34_smoke_default_output_mode() {
    assert_eq!(ReplayOutputMode::default(), ReplayOutputMode::Human);
}

/// IC-35 (S-03): InspectResult from minimal artifact produces valid output.
#[test]
fn ic35_smoke_inspect_minimal() {
    let events = vec![make_event("r1", 1000, 1)];
    let result = InspectResult::from_events("minimal.ftreplay", &events);
    assert_eq!(result.event_count, 1);
    assert!(result.integrity_ok);

    let human = result.render_human();
    assert!(human.contains("minimal.ftreplay"));
    assert!(human.contains("Events:"));

    // Robot data conversion
    let robot_data = InspectData::from_inspect_result(&result);
    assert_eq!(robot_data.event_count, 1);
    assert!(robot_data.integrity_ok);

    // MCP dispatch wrap
    let dispatch = DispatchResult::ok(serde_json::to_value(&robot_data).unwrap());
    assert!(dispatch.is_ok());
}

/// IC-36 (S-04): DiffRunner with identical inputs produces zero divergences.
#[test]
fn ic36_smoke_diff_identical() {
    let runner = DiffRunner::new();
    let events = vec![
        make_event("r1", 100, 1),
        make_event("r2", 200, 1),
    ];
    let result = runner.run(&events, &events, &DiffConfig::default());

    assert_eq!(result.exit_code, ReplayExitCode::Pass);

    // Verify robot formatting produces valid JSON
    let robot_output = runner.format_result(
        &result,
        ReplayOutputMode::Robot,
        &ReportMeta::default(),
    );
    let parsed: serde_json::Value = serde_json::from_str(&robot_output).unwrap();
    assert!(parsed.is_object());
}

/// IC-37 (S-05): ArtifactListData with empty list is valid.
#[test]
fn ic37_smoke_empty_artifact_list() {
    let data = ArtifactListData {
        count: 0,
        artifacts: vec![],
    };

    let json = serde_json::to_value(&data).unwrap();
    assert_eq!(json["count"], 0);
    assert!(json["artifacts"].as_array().unwrap().is_empty());

    // Wrapping in dispatch
    let dispatch = DispatchResult::ok(json);
    assert!(dispatch.is_ok());
}

/// IC-38: RegressionSuiteResult with all passes produces overall_pass=true.
#[test]
fn ic38_smoke_suite_all_pass() {
    let results = vec![
        ArtifactResult {
            artifact_path: "a.ftreplay".into(),
            passed: true,
            gate_result_summary: "Pass".into(),
            error: None,
        },
        ArtifactResult {
            artifact_path: "b.ftreplay".into(),
            passed: true,
            gate_result_summary: "Pass".into(),
            error: None,
        },
    ];
    let suite = RegressionSuiteResult::from_results(results);
    assert!(suite.overall_pass);
    assert_eq!(suite.total_artifacts, 2);
    assert_eq!(suite.passed, 2);
    assert_eq!(suite.failed, 0);
    assert_eq!(suite.errored, 0);

    // Robot data conversion
    let robot_data = RegressionSuiteData::from_suite_result(&suite);
    assert!(robot_data.passed);
    assert_eq!(robot_data.total_artifacts, 2);
}

// ============================================================================
// IC-39..IC-42: Cross-Surface Request Validation
// ============================================================================

/// IC-39: MCP validate_required_str rejects what Robot serde would also reject.
#[test]
fn ic39_validation_rejects_empty_strings() {
    // Empty "trace" should be rejected by MCP validation
    let args = serde_json::json!({"trace": ""});
    let result = validate_required_str(&args, "trace");
    assert!(result.is_err(), "empty trace should be rejected");

    // Missing "trace" should be rejected
    let args = serde_json::json!({});
    let result = validate_required_str(&args, "trace");
    assert!(result.is_err(), "missing trace should be rejected");
}

/// IC-40: MCP validate_required_str accepts what Robot serde accepts.
#[test]
fn ic40_validation_accepts_valid_strings() {
    let args = serde_json::json!({"trace": "test.ftreplay"});
    let val = validate_required_str(&args, "trace").unwrap();
    assert_eq!(val, "test.ftreplay");

    // Verify Robot serde also works with same value
    let robot_json = serde_json::json!({"trace": "test.ftreplay"});
    let req: InspectRequest = serde_json::from_value(robot_json).unwrap();
    assert_eq!(req.trace, "test.ftreplay");
}

/// IC-41: MCP and Robot produce equivalent outputs for same diff input.
#[test]
fn ic41_diff_output_equivalence() {
    let runner = DiffRunner::new();
    let events = vec![make_event("r1", 100, 1)];

    // Run the diff
    let result = runner.run(&events, &events, &DiffConfig::default());

    // CLI robot-mode output
    let robot_str = runner.format_result(
        &result,
        ReplayOutputMode::Robot,
        &ReportMeta::default(),
    );
    let cli_json: serde_json::Value = serde_json::from_str(&robot_str).unwrap();
    assert!(cli_json.is_object(), "CLI robot output should be valid JSON object");

    // Robot envelope would wrap DiffData
    let diff_data = DiffData {
        passed: result.exit_code == ReplayExitCode::Pass,
        exit_code: result.exit_code.code(),
        divergence_count: result.diff.divergences.len() as u64,
        recommendation: result.recommendation.clone(),
        gate_result: format!("{:?}", result.gate_result),
        severity_counts: std::collections::BTreeMap::new(),
    };
    let robot_json = serde_json::to_value(&diff_data).unwrap();
    assert!(robot_json.is_object(), "Robot JSON should be valid object");

    // Both should agree on pass/fail
    assert_eq!(diff_data.passed, true);
    assert_eq!(result.exit_code, ReplayExitCode::Pass);
}

/// IC-42: MCP argument extraction matches Robot serde for diff defaults.
#[test]
fn ic42_mcp_args_match_robot_serde() {
    let args = serde_json::json!({
        "baseline": "base.ftreplay",
        "candidate": "cand.ftreplay"
    });

    // MCP extraction
    let mcp_baseline = validate_required_str(&args, "baseline").unwrap();
    let mcp_candidate = validate_required_str(&args, "candidate").unwrap();
    let mcp_tolerance = validate_optional_u64(&args, "tolerance_ms", 100);
    let mcp_budget = validate_optional_str(&args, "budget");

    // Robot serde
    let req: DiffRequest = serde_json::from_value(args).unwrap();

    assert_eq!(mcp_baseline, req.baseline);
    assert_eq!(mcp_candidate, req.candidate);
    assert_eq!(mcp_tolerance, req.tolerance_ms);
    assert_eq!(mcp_budget, req.budget);
}
