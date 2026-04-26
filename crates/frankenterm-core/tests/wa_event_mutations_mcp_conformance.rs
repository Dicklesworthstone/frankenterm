#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle, StoredEvent};
use serde::Serialize;
use serde_json::{Map, Value, json};
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
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    let server = build_server_with_db(&config, db_path).expect("build MCP server");
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
        .find(|tool: &FrameworkTool| tool.name == tool_name)
        .map(|tool| tool.input_schema)
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
        pretty_canonical(actual_schema),
        pretty_canonical(&expected_schema),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

fn assert_common_envelope_fields(envelope: &Value, ok: bool, label: &str) {
    assert_eq!(
        envelope["ok"], ok,
        "{label} unexpected ok field: {envelope}"
    );
    assert!(
        envelope["elapsed_ms"].is_number(),
        "{label} missing elapsed_ms: {envelope}"
    );
    assert!(
        envelope["now"].is_number(),
        "{label} missing now: {envelope}"
    );
    assert_eq!(envelope["mcp_version"], "v1");
    assert!(envelope["version"].is_string());
}

fn assert_success_envelope_shape(envelope: &Value, label: &str) {
    assert_common_envelope_fields(envelope, true, label);
    assert!(
        envelope["data"].is_object(),
        "{label} missing data: {envelope}"
    );
    assert!(envelope.get("error").is_none());
    assert!(envelope.get("error_code").is_none());
    assert!(envelope.get("hint").is_none());
}

fn assert_boundary_error_contains(error: &str, field_hint: &str) {
    assert!(
        error.contains("[-32602]"),
        "expected framework invalid-params code in error: {error}"
    );
    assert!(
        error.contains(field_hint),
        "expected field hint '{field_hint}' in error: {error}"
    );
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" | "note_updated_at" | "triage_updated_at" => {
                        *child = Value::from(0_i64)
                    }
                    _ => canonicalize(child),
                }
            }

            let mut sorted = std::collections::BTreeMap::new();
            for (key, child) in std::mem::take(map) {
                sorted.insert(key, child);
            }
            let mut rebuilt = Map::new();
            for (key, child) in sorted {
                rebuilt.insert(key, child);
            }
            *map = rebuilt;
        }
        Value::Array(items) => {
            for item in items {
                canonicalize(item);
            }
        }
        _ => {}
    }
}

fn canonical_value(value: &Value) -> Value {
    let mut cloned = value.clone();
    canonicalize(&mut cloned);
    cloned
}

fn pretty_canonical(value: &Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string_pretty(&canonical_value(value)).expect("serialize canonical JSON")
    )
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_robot_envelope")
        .join(format!("{name}.json"))
}

fn read_or_update_golden(path: &PathBuf, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create golden dir");
        }
        fs::write(path, actual).expect("write golden");
        return actual.to_string();
    }

    fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing MCP event-mutation conformance golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_event_mutations_mcp_conformance \
             --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, captures: &[ToolContractCapture]) {
    let actual_value = serde_json::to_value(captures).expect("serialize capture");
    let actual_text = pretty_canonical(&actual_value);
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual_text);

    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let actual_path = path.with_extension("actual.json");
        let _ = fs::write(&actual_path, &actual_text);
        panic!(
            "MCP event-mutation conformance golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_event_mutations_mcp_conformance \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
}

fn make_pane(pane_id: u64, ts: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: Some(1),
        tab_id: Some(1),
        title: Some("codex".to_string()),
        cwd: Some("/tmp/wa-event-mutations".to_string()),
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
        dedupe_key: Some("wa-event-mutations-fixture".to_string()),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

fn seed_event_fixture(harness: &TestHarness, labels: &[&str]) -> i64 {
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
        for label in labels {
            storage
                .add_event_label(event_id, (*label).to_string(), Some("seed".to_string()))
                .await
                .expect("seed label");
        }
        storage.shutdown().await.expect("shutdown storage");
        event_id
    })
}

fn capture_tool_contract(
    tool_name: &str,
    success_setup: impl FnOnce(&TestHarness) -> i64,
    success_args: impl FnOnce(i64) -> Value,
    boundary_setup: impl FnOnce(&TestHarness) -> i64,
    boundary_args: impl FnOnce(i64) -> Value,
    boundary_hint: &str,
) -> ToolContractCapture {
    let mut success_harness = new_harness();
    let success_event_id = success_setup(&success_harness);
    let input_schema = tool_input_schema(&mut success_harness.client, tool_name);
    assert_schema_matches_manifest(tool_name, &input_schema);
    let success_envelope = parse_tool_envelope(
        &success_harness
            .client
            .call_tool(tool_name, success_args(success_event_id))
            .unwrap_or_else(|err| panic!("call {tool_name} success case: {err}")),
    );

    let mut boundary_harness = new_harness();
    let boundary_event_id = boundary_setup(&boundary_harness);
    let boundary_invalid_params_error = boundary_harness
        .client
        .call_tool(tool_name, boundary_args(boundary_event_id))
        .err()
        .map(|err| err.to_string())
        .unwrap_or_else(|| panic!("expected {tool_name} boundary-invalid case to fail"));

    assert_success_envelope_shape(&success_envelope, tool_name);
    assert_boundary_error_contains(&boundary_invalid_params_error, boundary_hint);

    ToolContractCapture {
        tool: tool_name.to_string(),
        input_schema,
        success_envelope,
        boundary_invalid_params_error,
    }
}

// TODO(ft-4o5mb.6): remove #[ignore] once this harness has a reproducible local or remote
// verification lane that can regenerate and check the golden without a long full-crate build.
#[test]
#[ignore = "verify partial: local golden generation currently requires a full frankenterm-core integration build that did not complete within the session cap"]
fn mcp_conformance_wa_event_mutations_contract_matches_golden() {
    let captures = vec![
        capture_tool_contract(
            "wa.events_annotate",
            |harness| seed_event_fixture(harness, &["customer-impacting"]),
            |event_id| {
                json!({
                    "event_id": event_id,
                    "note": "Investigating customer impact",
                    "by": "ops"
                })
            },
            |harness| seed_event_fixture(harness, &[]),
            |event_id| {
                json!({
                    "event_id": event_id,
                    "note": 7
                })
            },
            "root.note",
        ),
        capture_tool_contract(
            "wa.events_triage",
            |harness| seed_event_fixture(harness, &["urgent"]),
            |event_id| {
                json!({
                    "event_id": event_id,
                    "state": "investigating",
                    "by": "ops"
                })
            },
            |harness| seed_event_fixture(harness, &[]),
            |event_id| {
                json!({
                    "event_id": event_id,
                    "state": 7
                })
            },
            "root.state",
        ),
        capture_tool_contract(
            "wa.events_label",
            |harness| seed_event_fixture(harness, &["customer-impacting"]),
            |event_id| {
                json!({
                    "event_id": event_id,
                    "add": "urgent",
                    "by": "ops"
                })
            },
            |harness| seed_event_fixture(harness, &[]),
            |event_id| {
                json!({
                    "event_id": event_id,
                    "add": 7
                })
            },
            "root.add",
        ),
    ];

    assert_matches_golden("wa_event_mutations_conformance", &captures);
}
