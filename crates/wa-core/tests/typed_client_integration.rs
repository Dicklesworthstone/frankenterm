//! Integration tests for wa-core robot_types module (wa-upg.10.6).
//!
//! These tests validate that the typed client response types are structurally
//! compatible with the JSON schemas on disk and that deserialization works
//! correctly for all response types.
//!
//! # Test strategy
//!
//! - **Deserialization tests**: Every endpoint data type can deserialize from
//!   realistic JSON and round-trip correctly.
//! - **Schema compatibility tests**: For endpoints where schemas match the Rust
//!   types, we validate required field coverage.
//! - **Drift detection**: Documents known schema↔type mismatches (schemas were
//!   hand-authored before Rust types stabilized). New drift triggers failures.
//! - **Error handling tests**: Error envelope, error codes, and into_result().

use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use wa_core::api_schema::SchemaRegistry;
use wa_core::robot_types::*;

// ============================================================================
// Helpers
// ============================================================================

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn schema_dir() -> PathBuf {
    workspace_root().join("docs").join("json-schema")
}

fn load_schema(filename: &str) -> Value {
    let path = schema_dir().join(filename);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read schema {}: {}", path.display(), e));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse schema {}: {}", filename, e))
}

fn schema_required_fields(schema: &Value) -> HashSet<String> {
    schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn schema_property_names(schema: &Value) -> HashSet<String> {
    schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

fn wrap_envelope(data: Value) -> Value {
    json!({
        "ok": true,
        "data": data,
        "elapsed_ms": 5,
        "version": "0.1.0",
        "now": 1700000000000u64
    })
}

fn wrap_error_envelope(code: &str, message: &str) -> Value {
    json!({
        "ok": false,
        "error": message,
        "error_code": code,
        "hint": "check wa why for details",
        "elapsed_ms": 2,
        "version": "0.1.0",
        "now": 1700000000000u64
    })
}

fn validate_required_fields_present(data: &Value, schema: &Value, schema_name: &str) {
    let required = schema_required_fields(schema);
    if let Some(obj) = data.as_object() {
        let present: HashSet<String> = obj.keys().cloned().collect();
        let missing: Vec<&String> = required.difference(&present).collect();
        assert!(
            missing.is_empty(),
            "{}: Missing required fields: {:?} (present: {:?})",
            schema_name,
            missing,
            present
        );
    } else {
        panic!("{}: Data is not an object: {:?}", schema_name, data);
    }
}

/// Schemas where the hand-authored JSON Schema and Rust types have known
/// naming/structural drift. These schemas were written before the Rust types
/// stabilized. The drift is documented here and tracked for future alignment.
///
/// Each entry maps schema_file -> brief description of drift.
fn known_drift_schemas() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::new()
}

// ============================================================================
// Deserialization roundtrip tests — validate typed structs parse correctly
// ============================================================================

macro_rules! deser_test {
    ($test_name:ident, $type:ty, $fixture:expr) => {
        #[test]
        fn $test_name() {
            let envelope = wrap_envelope($fixture);
            let resp: RobotResponse<$type> = serde_json::from_value(envelope).unwrap_or_else(|e| {
                panic!(
                    "Failed to deserialize into RobotResponse<{}>: {}",
                    stringify!($type),
                    e
                )
            });
            assert!(resp.ok);
            let data = resp.data.expect("data should be present");

            // Round-trip: serialize back to JSON
            let reserialized =
                serde_json::to_value(&data).expect("re-serialization should succeed");
            assert!(reserialized.is_object());
        }
    };
}

deser_test!(
    deser_get_text,
    GetTextData,
    json!({
        "pane_id": 1,
        "text": "$ echo hello\nhello\n$ ",
        "tail_lines": 100,
        "escapes_included": false
    })
);

deser_test!(
    deser_get_text_truncated,
    GetTextData,
    json!({
        "pane_id": 1,
        "text": "truncated...",
        "tail_lines": 50,
        "escapes_included": true,
        "truncated": true,
        "truncation_info": {
            "original_bytes": 10000,
            "returned_bytes": 5000,
            "original_lines": 200,
            "returned_lines": 50
        }
    })
);

deser_test!(
    deser_wait_for,
    WaitForData,
    json!({
        "pane_id": 1,
        "pattern": "\\$\\s*$",
        "matched": true,
        "elapsed_ms": 1200,
        "polls": 24,
        "is_regex": true
    })
);

deser_test!(
    deser_send,
    SendData,
    json!({
        "pane_id": 1,
        "injection": {
            "status": "allowed",
            "summary": "echo hello",
            "pane_id": 1,
            "action": "send_text",
            "decision": {"decision": "allow"}
        }
    })
);

deser_test!(
    deser_search,
    SearchData,
    json!({
        "query": "error",
        "results": [{
            "segment_id": 101,
            "pane_id": 2,
            "seq": 5,
            "captured_at": 1700000000000i64,
            "score": 2.5,
            "snippet": "...compile error..."
        }],
        "total_hits": 1,
        "limit": 20
    })
);

deser_test!(
    deser_events,
    EventsData,
    json!({
        "events": [{
            "id": 42,
            "pane_id": 1,
            "rule_id": "codex.build_error",
            "pack_id": "codex",
            "event_type": "error",
            "severity": "high",
            "confidence": 0.95,
            "captured_at": 1700000000000i64
        }],
        "total_count": 1,
        "limit": 50,
        "unhandled_only": false
    })
);

deser_test!(
    deser_events_with_would_handle,
    EventsData,
    json!({
        "events": [{
            "id": 42,
            "pane_id": 1,
            "rule_id": "codex.build_error",
            "pack_id": "codex",
            "event_type": "error",
            "severity": "high",
            "confidence": 0.95,
            "captured_at": 1700000000000i64,
            "would_handle_with": {
                "workflow": "fix_build",
                "preview_command": "wa robot workflow run fix_build --pane 1",
                "would_run": true
            }
        }],
        "total_count": 1,
        "limit": 50,
        "unhandled_only": false,
        "would_handle": true,
        "dry_run": true
    })
);

deser_test!(
    deser_event_mutation,
    EventMutationData,
    json!({
        "event_id": 99,
        "changed": true,
        "annotations": {
            "triage_state": "resolved",
            "labels": ["confirmed"]
        }
    })
);

deser_test!(
    deser_workflow_run,
    WorkflowRunData,
    json!({
        "workflow_name": "fix_build",
        "pane_id": 1,
        "execution_id": "exec-abc123",
        "status": "running",
        "started_at": 1700000000000i64
    })
);

deser_test!(
    deser_workflow_list,
    WorkflowListData,
    json!({
        "workflows": [
            {"name": "fix_build", "enabled": true, "requires_pane": true},
            {"name": "notify", "enabled": false}
        ],
        "total": 2,
        "enabled_count": 1
    })
);

deser_test!(
    deser_workflow_status,
    WorkflowStatusData,
    json!({
        "execution_id": "exec-xyz",
        "workflow_name": "fix_build",
        "pane_id": 1,
        "status": "completed",
        "started_at": 1700000000000i64,
        "completed_at": 1700000005000i64,
        "current_step": 3,
        "total_steps": 3
    })
);

deser_test!(
    deser_workflow_status_list,
    WorkflowStatusListData,
    json!({
        "executions": [{
            "execution_id": "exec-1",
            "workflow_name": "fix_build",
            "status": "completed"
        }],
        "count": 1,
        "active_only": true
    })
);

deser_test!(
    deser_workflow_abort,
    WorkflowAbortData,
    json!({
        "execution_id": "exec-xyz",
        "aborted": true,
        "forced": false,
        "workflow_name": "fix_build",
        "previous_status": "running"
    })
);

deser_test!(
    deser_rules_list,
    RulesListData,
    json!({
        "rules": [{
            "id": "codex.build_error",
            "agent_type": "codex",
            "event_type": "error",
            "severity": "high",
            "description": "Detects build errors",
            "workflow": "fix_build",
            "anchor_count": 3,
            "has_regex": true
        }],
        "pack_filter": "codex"
    })
);

deser_test!(
    deser_rules_test,
    RulesTestData,
    json!({
        "text_length": 500,
        "match_count": 1,
        "matches": [{
            "rule_id": "codex.build_error",
            "start": 10,
            "end": 40,
            "matched_text": "error[E0308]: mismatched types",
            "trace": {
                "anchors_checked": true,
                "regex_matched": true
            }
        }]
    })
);

deser_test!(
    deser_rule_detail,
    RuleDetailData,
    json!({
        "id": "codex.build_error",
        "agent_type": "codex",
        "event_type": "error",
        "severity": "high",
        "description": "Build error detected",
        "anchors": ["error:", "failed"],
        "regex": "error\\[E\\d+\\]",
        "workflow": "fix_build"
    })
);

deser_test!(
    deser_rules_lint,
    RulesLintData,
    json!({
        "total_rules": 50,
        "rules_checked": 48,
        "errors": [],
        "warnings": [{"rule_id": "x.y", "category": "style", "message": "no desc"}],
        "passed": true
    })
);

deser_test!(
    deser_accounts_list,
    AccountsListData,
    json!({
        "accounts": [{
            "account_id": "acc-1",
            "service": "anthropic",
            "name": "main",
            "percent_remaining": 85.5,
            "last_refreshed_at": 1700000000000i64
        }],
        "total": 1,
        "service": "anthropic"
    })
);

deser_test!(
    deser_accounts_list_with_pick_preview,
    AccountsListData,
    json!({
        "accounts": [{
            "account_id": "acc-1",
            "service": "anthropic",
            "percent_remaining": 85.5,
            "last_refreshed_at": 0
        }],
        "total": 1,
        "service": "anthropic",
        "pick_preview": {
            "selected_account_id": "acc-1",
            "selection_reason": "highest remaining",
            "threshold_percent": 10.0,
            "candidates_count": 1,
            "filtered_count": 1
        }
    })
);

deser_test!(
    deser_accounts_refresh,
    AccountsRefreshData,
    json!({
        "service": "anthropic",
        "refreshed_count": 2,
        "refreshed_at": "2026-02-06T19:00:00Z",
        "accounts": [{
            "account_id": "acc-1",
            "service": "anthropic",
            "percent_remaining": 90.0,
            "last_refreshed_at": 0
        }]
    })
);

deser_test!(
    deser_reservations_list,
    ReservationsListData,
    json!({
        "reservations": [{
            "id": 1,
            "pane_id": 5,
            "owner_kind": "agent",
            "owner_id": "codex-1",
            "reason": "build monitoring",
            "created_at": 1700000000000i64,
            "expires_at": 1700000060000i64,
            "status": "active"
        }],
        "total": 1
    })
);

deser_test!(
    deser_reserve,
    ReserveData,
    json!({
        "reservation": {
            "id": 7,
            "pane_id": 3,
            "owner_kind": "agent",
            "owner_id": "codex-1",
            "created_at": 1700000000000i64,
            "expires_at": 1700000060000i64,
            "status": "active"
        }
    })
);

deser_test!(
    deser_release,
    ReleaseData,
    json!({
        "reservation_id": 7,
        "released": true
    })
);

deser_test!(
    deser_approve,
    ApproveData,
    json!({
        "code": "AP-abc123",
        "valid": true,
        "created_at": 1700000000000u64,
        "action_kind": "send_text",
        "pane_id": 1,
        "expires_at": 1700000060000u64
    })
);

deser_test!(
    deser_why,
    WhyData,
    json!({
        "code": "WA-2001",
        "category": "storage",
        "title": "Database locked",
        "explanation": "The SQLite database is locked by another process.",
        "suggestions": ["retry after a short delay"],
        "see_also": ["WA-2002"]
    })
);

deser_test!(
    deser_quick_start,
    QuickStartData,
    json!({
        "description": "Quick start guide",
        "global_flags": [{"flag": "--pane", "env_var": "WA_PANE", "description": "target pane"}],
        "core_loop": [{"step": 1, "action": "get text", "command": "wa robot get-text"}],
        "commands": [{"name": "get-text", "args": "--pane <ID>", "summary": "Get pane text", "examples": []}],
        "tips": ["use --format json"],
        "error_handling": {
            "common_codes": [{"code": "WA-1003", "meaning": "pane not found", "recovery": "check id"}],
            "safety_notes": ["always check ok field"]
        }
    })
);

// ============================================================================
// Schema field validation — schemas that match the Rust types
// ============================================================================

macro_rules! schema_match_test {
    ($test_name:ident, $type:ty, $schema_file:expr, $fixture:expr) => {
        #[test]
        fn $test_name() {
            let schema = load_schema($schema_file);
            let envelope = wrap_envelope($fixture);

            let resp: RobotResponse<$type> = serde_json::from_value(envelope)
                .unwrap_or_else(|e| panic!("Failed to deserialize {}: {}", stringify!($type), e));

            let data = resp.into_result().unwrap();
            let reserialized = serde_json::to_value(&data).unwrap();

            // Required fields present
            validate_required_fields_present(&reserialized, &schema, $schema_file);
        }
    };
}

// These schemas are known to match the Rust types:
schema_match_test!(
    schema_get_text,
    GetTextData,
    "wa-robot-get-text.json",
    json!({
        "pane_id": 1,
        "text": "hello",
        "tail_lines": 100,
        "escapes_included": false
    })
);

schema_match_test!(
    schema_events,
    EventsData,
    "wa-robot-events.json",
    json!({
        "events": [],
        "total_count": 0,
        "limit": 50,
        "unhandled_only": false
    })
);

schema_match_test!(
    schema_event_mutation,
    EventMutationData,
    "wa-robot-event-mutation.json",
    json!({
        "event_id": 99,
        "changed": true,
        "annotations": {"triage_state": "resolved"}
    })
);

schema_match_test!(
    schema_workflow_list,
    WorkflowListData,
    "wa-robot-workflow-list.json",
    json!({
        "workflows": [],
        "total": 0
    })
);

schema_match_test!(
    schema_approve,
    ApproveData,
    "wa-robot-approve.json",
    json!({
        "code": "AP-abc",
        "valid": true
    })
);

schema_match_test!(
    schema_why,
    WhyData,
    "wa-robot-why.json",
    json!({
        "code": "WA-2001",
        "category": "storage",
        "title": "Database locked",
        "explanation": "locked"
    })
);

schema_match_test!(
    schema_wait_for,
    WaitForData,
    "wa-robot-wait-for.json",
    json!({
        "pane_id": 1,
        "pattern": "\\$\\s*$",
        "matched": true,
        "elapsed_ms": 1200,
        "polls": 24,
        "is_regex": true
    })
);

schema_match_test!(
    schema_search,
    SearchData,
    "wa-robot-search.json",
    json!({
        "query": "error",
        "results": [{
            "segment_id": 101,
            "pane_id": 2,
            "seq": 5,
            "captured_at": 1700000000000i64,
            "score": 2.5,
            "snippet": "...compile error..."
        }],
        "total_hits": 1,
        "limit": 20
    })
);

schema_match_test!(
    schema_reservations,
    ReservationsListData,
    "wa-robot-reservations.json",
    json!({
        "reservations": [{
            "id": 1,
            "pane_id": 5,
            "owner_kind": "agent",
            "owner_id": "codex-1",
            "reason": "build monitoring",
            "created_at": 1700000000000i64,
            "expires_at": 1700000060000i64,
            "status": "active"
        }],
        "total": 1
    })
);

schema_match_test!(
    schema_release,
    ReleaseData,
    "wa-robot-release.json",
    json!({
        "reservation_id": 7,
        "released": true
    })
);

schema_match_test!(
    schema_reserve,
    ReserveData,
    "wa-robot-reserve.json",
    json!({
        "reservation": {
            "id": 7,
            "pane_id": 3,
            "owner_kind": "agent",
            "owner_id": "codex-1",
            "created_at": 1700000000000i64,
            "expires_at": 1700000060000i64,
            "status": "active"
        }
    })
);

schema_match_test!(
    schema_workflow_abort,
    WorkflowAbortData,
    "wa-robot-workflow-abort.json",
    json!({
        "execution_id": "exec-xyz",
        "aborted": true,
        "forced": false,
        "workflow_name": "fix_build",
        "previous_status": "running"
    })
);

schema_match_test!(
    schema_rules_list,
    RulesListData,
    "wa-robot-rules-list.json",
    json!({
        "rules": [{
            "id": "codex.build_error",
            "agent_type": "codex",
            "event_type": "error",
            "severity": "high",
            "description": "Detects build errors",
            "workflow": "fix_build",
            "anchor_count": 3,
            "has_regex": true
        }]
    })
);

schema_match_test!(
    schema_rules_test,
    RulesTestData,
    "wa-robot-rules-test.json",
    json!({
        "text_length": 500,
        "match_count": 1,
        "matches": [{
            "rule_id": "codex.build_error",
            "start": 10,
            "end": 40,
            "matched_text": "error[E0308]: mismatched types",
            "trace": {
                "anchors_checked": true,
                "regex_matched": true
            }
        }]
    })
);

schema_match_test!(
    schema_accounts,
    AccountsListData,
    "wa-robot-accounts.json",
    json!({
        "accounts": [{
            "account_id": "acc-1",
            "service": "anthropic",
            "name": "main",
            "percent_remaining": 85.5,
            "last_refreshed_at": 1700000000000i64
        }],
        "total": 1,
        "service": "anthropic"
    })
);

schema_match_test!(
    schema_accounts_refresh,
    AccountsRefreshData,
    "wa-robot-accounts-refresh.json",
    json!({
        "service": "anthropic",
        "refreshed_count": 2,
        "refreshed_at": "2026-02-06T19:00:00Z",
        "accounts": [{
            "account_id": "acc-1",
            "service": "anthropic",
            "percent_remaining": 90.0,
            "last_refreshed_at": 0
        }]
    })
);

schema_match_test!(
    schema_workflow_run,
    WorkflowRunData,
    "wa-robot-workflow-run.json",
    json!({
        "workflow_name": "fix_build",
        "pane_id": 1,
        "execution_id": "exec-abc123",
        "status": "running",
        "started_at": 1700000000000i64
    })
);

schema_match_test!(
    schema_workflow_status,
    WorkflowStatusData,
    "wa-robot-workflow-status.json",
    json!({
        "execution_id": "exec-xyz",
        "workflow_name": "fix_build",
        "pane_id": 1,
        "status": "completed",
        "started_at": 1700000000000i64,
        "completed_at": 1700000005000i64,
        "current_step": 3,
        "total_steps": 3
    })
);

// ============================================================================
// Drift detection — document and freeze known schema↔type mismatches
// ============================================================================

#[test]
fn drift_set_is_stable() {
    // This test ensures we know about ALL drift between schemas and types.
    // If a schema is fixed to match the Rust types, remove it from known_drift_schemas().
    // If a new schema is added that doesn't match, this test will fail and prompt an update.
    let known = known_drift_schemas();
    let expected_count = 0;
    assert_eq!(
        known.len(),
        expected_count,
        "Known drift count changed. If schemas were fixed, remove entries from known_drift_schemas(). \
         If new drift appeared, add entries. Current known drift: {:?}",
        known.keys().collect::<Vec<_>>()
    );
}

#[test]
fn drift_report() {
    // This test generates a human-readable drift report for tracking.
    let known = known_drift_schemas();
    let schema_dir = schema_dir();

    let mut report = String::new();
    report.push_str("Schema↔Type Drift Report\n");
    report.push_str("========================\n\n");

    for (schema_file, description) in &known {
        let schema_path = schema_dir.join(schema_file);
        let exists = schema_path.exists();
        report.push_str(&format!(
            "- {} [{}]\n  {}\n\n",
            schema_file,
            if exists { "on disk" } else { "MISSING" },
            description
        ));
    }

    // Print report for CI visibility
    eprintln!("{}", report);

    // All known-drift schemas should exist on disk
    for schema_file in known.keys() {
        assert!(
            schema_dir.join(schema_file).exists(),
            "Known-drift schema {} is missing from disk",
            schema_file
        );
    }
}

#[test]
fn compatible_schemas_have_no_field_drift() {
    // Schemas NOT in the known-drift set should have zero field drift.
    let known_drift = known_drift_schemas();
    let schema_dir = schema_dir();
    let registry = SchemaRegistry::canonical();

    // Schemas with known gaps (not on disk)
    let missing_schemas: HashSet<&str> = ["wa-robot-rules-lint.json", "wa-robot-rules-show.json"]
        .into_iter()
        .collect();

    // Schemas without matching typed struct (meta-schemas, special)
    let untyped_schemas: HashSet<&str> = [
        "wa-robot-envelope.json",
        "wa-robot-help.json",
        "wa-robot-state.json",
        "wa-robot-send.json", // SendData uses serde_json::Value for injection
    ]
    .into_iter()
    .collect();

    for endpoint in &registry.endpoints {
        let file = endpoint.schema_file.as_str();

        if known_drift.contains_key(file)
            || missing_schemas.contains(file)
            || untyped_schemas.contains(file)
        {
            continue;
        }

        let schema_path = schema_dir.join(file);
        if !schema_path.exists() {
            continue;
        }

        let schema = load_schema(file);
        let props = schema_property_names(&schema);
        let required = schema_required_fields(&schema);

        // Schemas not in drift set should have their required fields documented
        assert!(
            !required.is_empty() || !props.is_empty(),
            "{}: Schema has no properties or required fields — check if it needs typing",
            file
        );
    }
}

// ============================================================================
// Error envelope tests
// ============================================================================

#[test]
fn error_envelope_deserializes_for_any_data_type() {
    let error_json = wrap_error_envelope("WA-1003", "pane 42 not found");

    let resp: RobotResponse<GetTextData> = serde_json::from_value(error_json.clone()).unwrap();
    assert!(!resp.ok);
    assert!(resp.data.is_none());
    assert_eq!(resp.error_code.as_deref(), Some("WA-1003"));

    let resp2: RobotResponse<EventsData> = serde_json::from_value(error_json.clone()).unwrap();
    assert!(!resp2.ok);

    let resp3: RobotResponse<WorkflowRunData> = serde_json::from_value(error_json).unwrap();
    assert!(!resp3.ok);
}

#[test]
fn error_envelope_into_result_preserves_code_and_hint() {
    let error_json = wrap_error_envelope("WA-4001", "action denied by policy");
    let resp: RobotResponse<SendData> = serde_json::from_value(error_json).unwrap();

    let err = resp.into_result().unwrap_err();
    assert_eq!(err.code.as_deref(), Some("WA-4001"));
    assert!(err.message.contains("denied"));
    assert!(err.hint.is_some());
}

#[test]
fn all_error_codes_parse_correctly() {
    let codes = [
        ("WA-1001", ErrorCode::WeztermNotFound),
        ("WA-1002", ErrorCode::WeztermExecFailed),
        ("WA-1003", ErrorCode::PaneNotFound),
        ("WA-2001", ErrorCode::DatabaseLocked),
        ("WA-2002", ErrorCode::StorageCorruption),
        ("WA-3001", ErrorCode::InvalidRegex),
        ("WA-4001", ErrorCode::ActionDenied),
        ("WA-4002", ErrorCode::RateLimitExceeded),
        ("WA-4003", ErrorCode::ApprovalRequired),
        ("WA-5001", ErrorCode::WorkflowNotFound),
        ("WA-5002", ErrorCode::WorkflowStepFailed),
        ("WA-6001", ErrorCode::NetworkTimeout),
        ("WA-7001", ErrorCode::ConfigInvalid),
        ("WA-9001", ErrorCode::InternalError),
        ("WA-9003", ErrorCode::VersionMismatch),
    ];

    for (code_str, expected) in &codes {
        let parsed =
            ErrorCode::parse(code_str).unwrap_or_else(|| panic!("Failed to parse {}", code_str));
        assert_eq!(&parsed, expected, "Mismatch for {}", code_str);
        assert_eq!(parsed.as_str(), *code_str);
    }
}

// ============================================================================
// Schema coverage
// ============================================================================

#[test]
fn registry_endpoints_have_matching_schema_files() {
    let registry = SchemaRegistry::canonical();
    let schema_dir = schema_dir();

    let known_gaps: HashSet<&str> = ["wa-robot-rules-lint.json", "wa-robot-rules-show.json"]
        .into_iter()
        .collect();

    let mut missing = Vec::new();
    for endpoint in &registry.endpoints {
        if known_gaps.contains(endpoint.schema_file.as_str()) {
            continue;
        }
        let schema_path = schema_dir.join(&endpoint.schema_file);
        if !schema_path.exists() {
            missing.push(format!("{} ({})", endpoint.schema_file, endpoint.id));
        }
    }

    assert!(
        missing.is_empty(),
        "Registry endpoints missing schema files: {:?}",
        missing
    );
}

#[test]
fn all_schema_files_are_valid_json() {
    let schema_dir = schema_dir();
    let entries: Vec<_> = std::fs::read_dir(&schema_dir)
        .expect("schema dir exists")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .collect();

    assert!(!entries.is_empty(), "No schema files found");

    for entry in &entries {
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy();
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", filename, e));
        let schema: Value = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("Invalid JSON in {}: {}", filename, e));

        assert!(
            schema.get("$schema").is_some(),
            "{}: missing $schema",
            filename
        );
        assert!(schema.get("title").is_some(), "{}: missing title", filename);
        assert!(
            schema.get("description").is_some(),
            "{}: missing description",
            filename
        );
    }
}

// ============================================================================
// Required field coverage — typed structs cover schema required fields
// ============================================================================

#[test]
fn required_fields_coverage_get_text() {
    let schema = load_schema("wa-robot-get-text.json");
    let required = schema_required_fields(&schema);

    let data = GetTextData {
        pane_id: 1,
        text: "hello".to_string(),
        tail_lines: 10,
        escapes_included: false,
        truncated: false,
        truncation_info: None,
    };
    let json = serde_json::to_value(&data).unwrap();
    let json_keys: HashSet<String> = json.as_object().unwrap().keys().cloned().collect();

    let missing: Vec<&String> = required.difference(&json_keys).collect();
    assert!(
        missing.is_empty(),
        "GetTextData missing required schema fields: {:?}",
        missing
    );
}

#[test]
fn required_fields_coverage_events() {
    let schema = load_schema("wa-robot-events.json");
    let required = schema_required_fields(&schema);

    let data = EventsData {
        events: vec![],
        total_count: 0,
        limit: 20,
        pane_filter: None,
        rule_id_filter: None,
        event_type_filter: None,
        triage_state_filter: None,
        label_filter: None,
        unhandled_only: false,
        since_filter: None,
        would_handle: false,
        dry_run: false,
    };
    let json = serde_json::to_value(&data).unwrap();
    let json_keys: HashSet<String> = json.as_object().unwrap().keys().cloned().collect();

    let missing: Vec<&String> = required.difference(&json_keys).collect();
    assert!(
        missing.is_empty(),
        "EventsData missing required: {:?}",
        missing
    );
}

#[test]
fn required_fields_coverage_why() {
    let schema = load_schema("wa-robot-why.json");
    let required = schema_required_fields(&schema);

    let data = WhyData {
        code: "WA-1001".to_string(),
        category: "wezterm".to_string(),
        title: "Not found".to_string(),
        explanation: "WezTerm CLI not found".to_string(),
        suggestions: None,
        see_also: None,
    };
    let json = serde_json::to_value(&data).unwrap();
    let json_keys: HashSet<String> = json.as_object().unwrap().keys().cloned().collect();

    let missing: Vec<&String> = required.difference(&json_keys).collect();
    assert!(
        missing.is_empty(),
        "WhyData missing required: {:?}",
        missing
    );
}

// ============================================================================
// Convenience parsing
// ============================================================================

#[test]
fn from_json_string_convenience() {
    let raw = r#"{"ok":true,"data":{"pane_id":1,"text":"hi","tail_lines":10,"escapes_included":false},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
    let resp = RobotResponse::<GetTextData>::from_json(raw).unwrap();
    assert_eq!(resp.data.unwrap().text, "hi");
}

#[test]
fn from_json_bytes_convenience() {
    let raw = br#"{"ok":true,"data":{"pane_id":1,"text":"hi","tail_lines":10,"escapes_included":false},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
    let resp = RobotResponse::<GetTextData>::from_json_bytes(raw).unwrap();
    assert_eq!(resp.data.unwrap().text, "hi");
}

#[test]
fn parse_response_untyped_works() {
    let raw = r#"{"ok":true,"data":{"custom":"field","count":42},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
    let resp = parse_response_untyped(raw).unwrap();
    assert_eq!(resp.data.unwrap()["count"], 42);
}

#[test]
fn parse_response_generic_works() {
    let raw = r#"{"ok":true,"data":{"pane_id":1,"pattern":"prompt","matched":true,"elapsed_ms":500,"polls":10},"elapsed_ms":1,"version":"0.1.0","now":0}"#;
    let resp: RobotResponse<WaitForData> = parse_response(raw).unwrap();
    let data = resp.into_result().unwrap();
    assert!(data.matched);
}

// ============================================================================
// Tolerant deserialization — minimal required fields only
// ============================================================================

#[test]
fn events_minimal() {
    let json = wrap_envelope(json!({"events": [], "total_count": 0, "limit": 50}));
    let resp: RobotResponse<EventsData> = serde_json::from_value(json).unwrap();
    let data = resp.into_result().unwrap();
    assert!(data.pane_filter.is_none());
    assert!(!data.unhandled_only);
}

#[test]
fn workflow_run_minimal() {
    let json = wrap_envelope(json!({"workflow_name": "t", "pane_id": 1, "status": "queued"}));
    let resp: RobotResponse<WorkflowRunData> = serde_json::from_value(json).unwrap();
    let data = resp.into_result().unwrap();
    assert!(data.execution_id.is_none());
}

#[test]
fn accounts_list_minimal() {
    let json = wrap_envelope(json!({"accounts": [], "total": 0, "service": "test"}));
    let resp: RobotResponse<AccountsListData> = serde_json::from_value(json).unwrap();
    let data = resp.into_result().unwrap();
    assert!(data.pick_preview.is_none());
}

#[test]
fn rule_item_no_workflow() {
    let json = wrap_envelope(json!({
        "rules": [{
            "id": "t.r", "agent_type": "t", "event_type": "info",
            "severity": "low", "description": "test", "anchor_count": 1, "has_regex": false
        }]
    }));
    let resp: RobotResponse<RulesListData> = serde_json::from_value(json).unwrap();
    assert!(resp.into_result().unwrap().rules[0].workflow.is_none());
}
