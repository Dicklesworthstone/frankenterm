#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle, StoredEvent};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const FIXTURE_TS: i64 = 1_700_000_000_123;
const FIXTURE_PANE_ID: u64 = 7;
const FIXTURE_RULE_ID: &str = "codex.usage.reached";

struct TestHarness {
    _workspace: tempfile::TempDir,
    db_path: PathBuf,
    client: FrameworkTestClient,
}

#[derive(Serialize)]
struct ToolContractCapture {
    tool: String,
    input_schema: Value,
    success_envelope: Value,
    boundary_invalid_params_error: String,
}

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let server = build_server_with_db(&Config::default(), db_path).expect("build MCP server");
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    std::thread::spawn(move || {
        let _ = server.run_transport(server_transport);
    });

    let mut client = FrameworkTestClient::new(client_transport);
    client
        .initialize()
        .expect("initialize in-memory MCP client");
    client
}

fn new_harness() -> TestHarness {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    let db_path = workspace.path().join("mcp.sqlite3");
    let client = spawn_client(Some(db_path.clone()));
    TestHarness {
        _workspace: workspace,
        db_path,
        client,
    }
}

fn tool_input_schema(client: &mut FrameworkTestClient, tool_name: &str) -> Value {
    client
        .list_tools()
        .expect("list tools")
        .into_iter()
        .find(|tool| tool.name == tool_name)
        .map(|tool: FrameworkTool| tool.input_schema)
        .unwrap_or_else(|| panic!("missing tool {tool_name}"))
}

fn manifest_tool_schema(tool_name: &str) -> Value {
    let manifest_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("mcp_manifest.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("read mcp manifest fixture"),
    )
    .expect("parse mcp manifest fixture");
    manifest["tools"]
        .as_array()
        .expect("manifest tools array")
        .iter()
        .find(|tool| tool["name"] == tool_name)
        .and_then(|tool| tool.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| panic!("missing manifest schema for {tool_name}"))
}

fn first_text_content(contents: &[FrameworkContent]) -> &str {
    contents
        .first()
        .and_then(|content| match content {
            FrameworkContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("expected first MCP content to be text")
}

fn parse_tool_envelope(contents: &[FrameworkContent]) -> Value {
    serde_json::from_str(first_text_content(contents)).expect("parse JSON envelope")
}

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        serde_json::to_string_pretty(actual_schema).expect("serialize actual schema"),
        serde_json::to_string_pretty(&expected_schema).expect("serialize expected schema"),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

fn assert_common_envelope_fields(envelope: &Value, ok: bool) {
    assert_eq!(envelope["ok"], ok);
    assert!(envelope["elapsed_ms"].is_number());
    assert!(envelope["now"].is_number());
    assert_eq!(envelope["mcp_version"], "v1");
    assert!(envelope["version"].is_string());
}

fn assert_success_envelope_shape(envelope: &Value) {
    assert_common_envelope_fields(envelope, true);
    assert!(envelope["data"].is_object());
    assert!(envelope.get("error").is_none());
    assert!(envelope.get("error_code").is_none());
    assert!(envelope.get("hint").is_none());
}

fn assert_events_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("wa.events data object");
    assert_eq!(data["total_count"], Value::from(1));
    assert_eq!(data["limit"], Value::from(10));
    assert_eq!(data["pane_filter"], Value::from(FIXTURE_PANE_ID));
    assert_eq!(
        data["rule_id_filter"],
        Value::String(FIXTURE_RULE_ID.to_string())
    );
    assert_eq!(data["label_filter"], Value::String("urgent".to_string()));
    assert_eq!(data["unhandled_only"], Value::Bool(true));
    assert!(data.get("event_type_filter").is_none());
    assert!(data.get("triage_state_filter").is_none());
    assert!(data.get("since_filter").is_none());

    let events = data["events"].as_array().expect("wa.events events array");
    assert_eq!(events.len(), 1, "expected one filtered event");
    let event = events.first().expect("seeded wa.events item");
    assert_eq!(event["id"], Value::from(1));
    assert_eq!(event["pane_id"], Value::from(FIXTURE_PANE_ID));
    assert_eq!(event["rule_id"], Value::String(FIXTURE_RULE_ID.to_string()));
    assert_eq!(event["pack_id"], Value::String("builtin:codex".to_string()));
    assert_eq!(
        event["event_type"],
        Value::String("usage_limit".to_string())
    );
    assert_eq!(event["severity"], Value::String("warning".to_string()));
    assert_eq!(event["confidence"], Value::from(0.88));
    assert_eq!(
        event["extracted"],
        json!({
            "reset_at": "2026-04-24T00:00:00Z",
            "source": "fixture"
        })
    );
    assert_eq!(event["captured_at"], Value::from(FIXTURE_TS));
    assert!(event.get("handled_at").is_none());
    assert!(event.get("workflow_id").is_none());
    assert_eq!(
        event["annotations"],
        json!({
            "triage_state": null,
            "triage_updated_at": null,
            "triage_updated_by": null,
            "note": null,
            "note_updated_at": null,
            "note_updated_by": null,
            "labels": ["customer-impacting", "urgent"]
        })
    );
}

fn assert_boundary_invalid_params_error(error: &str) {
    assert!(
        error.contains("[-32602]"),
        "expected framework invalid-params code in error: {error}"
    );
    assert!(
        error.contains("root.limit: value must be >= 1"),
        "expected wa.events limit schema boundary failure in error: {error}"
    );
}

fn capture_tool_contract(
    tool_name: &str,
    success_setup: impl FnOnce(&mut TestHarness),
    success_args: impl FnOnce(&TestHarness) -> Value,
    boundary_invalid_setup: impl FnOnce(&mut TestHarness),
    boundary_invalid_args: impl FnOnce(&TestHarness) -> Value,
) -> ToolContractCapture {
    let mut harness = new_harness();
    success_setup(&mut harness);
    let input_schema = tool_input_schema(&mut harness.client, tool_name);
    assert_schema_matches_manifest(tool_name, &input_schema);
    let success_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(tool_name, success_args(&harness))
            .unwrap_or_else(|err| panic!("call {tool_name} success case: {err}")),
    );

    boundary_invalid_setup(&mut harness);
    let boundary_invalid_params_error = harness
        .client
        .call_tool(tool_name, boundary_invalid_args(&harness))
        .err()
        .map(|err| err.to_string())
        .unwrap_or_else(|| panic!("expected {tool_name} boundary-invalid case to fail"));

    let capture = ToolContractCapture {
        tool: tool_name.to_string(),
        input_schema,
        success_envelope,
        boundary_invalid_params_error,
    };
    capture
}

fn make_pane(pane_id: u64, ts: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(1),
        title: Some("codex".to_string()),
        cwd: Some("/tmp/wa-events".to_string()),
        tty_name: None,
        first_seen_at: ts,
        last_seen_at: ts,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn make_event() -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id: FIXTURE_PANE_ID,
        rule_id: FIXTURE_RULE_ID.to_string(),
        agent_type: "codex".to_string(),
        event_type: "usage_limit".to_string(),
        severity: "warning".to_string(),
        confidence: 0.88,
        extracted: Some(json!({
            "reset_at": "2026-04-24T00:00:00Z",
            "source": "fixture"
        })),
        matched_text: Some("Usage limit reached".to_string()),
        segment_id: None,
        detected_at: FIXTURE_TS,
        dedupe_key: Some("wa-events-fixture".to_string()),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

fn seed_events_fixture(harness: &TestHarness) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&harness.db_path.to_string_lossy())
            .await
            .expect("open storage");
        storage
            .upsert_pane(make_pane(FIXTURE_PANE_ID, FIXTURE_TS))
            .await
            .expect("upsert pane");
        let event_id = storage
            .record_event(make_event())
            .await
            .expect("record event");
        storage
            .add_event_label(event_id, "urgent".to_string(), Some("ops".to_string()))
            .await
            .expect("add urgent label");
        storage
            .add_event_label(event_id, "customer-impacting".to_string(), None)
            .await
            .expect("add customer-impacting label");
        storage.shutdown().await.expect("shutdown storage");
    });
}

#[test]
fn mcp_conformance_wa_events_contract_matches_expected_envelope() {
    let capture = capture_tool_contract(
        "wa.events",
        |harness| seed_events_fixture(harness),
        |_| {
            json!({
                "limit": 10,
                "pane": FIXTURE_PANE_ID,
                "rule_id": FIXTURE_RULE_ID,
                "label": "urgent",
                "unhandled": true
            })
        },
        |_| {},
        |_| json!({"limit": 0}),
    );
    assert_eq!(capture.tool, "wa.events");
    assert_success_envelope_shape(&capture.success_envelope);
    assert_events_success_data(&capture.success_envelope);
    assert_boundary_invalid_params_error(&capture.boundary_invalid_params_error);
}
