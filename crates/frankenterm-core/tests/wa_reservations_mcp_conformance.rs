#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkTestClient, FrameworkTool, framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneReservation, StorageHandle};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

const FIXTURE_PANE_ID: u64 = 42;
const OTHER_PANE_ID: u64 = 77;

struct TestHarness {
    _workspace: tempfile::TempDir,
    db_path: PathBuf,
    client: FrameworkTestClient,
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

fn runtime() -> frankenterm_core::runtime_async::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn seed_reservation(
    harness: &TestHarness,
    pane_id: u64,
    owner_kind: &str,
    owner_id: &str,
    reason: Option<&str>,
    ttl_ms: i64,
) -> PaneReservation {
    runtime().block_on(async {
        let storage = StorageHandle::new(&harness.db_path.to_string_lossy())
            .await
            .expect("open storage");
        let reservation = storage
            .create_reservation(pane_id, owner_kind, owner_id, reason, ttl_ms)
            .await
            .expect("create reservation");
        storage.shutdown().await.expect("shutdown storage");
        reservation
    })
}

fn list_active_reservations(harness: &TestHarness) -> Vec<PaneReservation> {
    runtime().block_on(async {
        let storage = StorageHandle::new(&harness.db_path.to_string_lossy())
            .await
            .expect("open storage");
        let reservations = storage
            .list_active_reservations()
            .await
            .expect("list active reservations");
        storage.shutdown().await.expect("shutdown storage");
        reservations
    })
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

#[test]
fn mcp_conformance_wa_reserve_contract_matches_expected_envelope() {
    let mut harness = new_harness();
    let input_schema = tool_input_schema(&mut harness.client, "wa.reserve");
    assert_schema_matches_manifest("wa.reserve", &input_schema);

    let success_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.reserve",
                json!({
                    "pane_id": FIXTURE_PANE_ID,
                    "owner_kind": "agent",
                    "owner_id": "LilacBay",
                    "reason": "conformance reserve",
                    "ttl_ms": 1500
                }),
            )
            .expect("call wa.reserve"),
    );

    assert_success_envelope_shape(&success_envelope);
    let reservation = success_envelope["data"]["reservation"]
        .as_object()
        .expect("wa.reserve reservation object");
    assert_eq!(reservation["id"], Value::from(1));
    assert_eq!(reservation["pane_id"], Value::from(FIXTURE_PANE_ID));
    assert_eq!(
        reservation["owner_kind"],
        Value::String("agent".to_string())
    );
    assert_eq!(
        reservation["owner_id"],
        Value::String("LilacBay".to_string())
    );
    assert_eq!(
        reservation["reason"],
        Value::String("conformance reserve".to_string())
    );
    assert_eq!(reservation["status"], Value::String("active".to_string()));
    assert!(reservation.get("released_at").is_none());
    let created_at = reservation["created_at"]
        .as_i64()
        .expect("created_at integer");
    let expires_at = reservation["expires_at"]
        .as_i64()
        .expect("expires_at integer");
    assert_eq!(
        expires_at - created_at,
        1500,
        "ttl delta should match requested ttl_ms"
    );

    let boundary_invalid_params_error = harness
        .client
        .call_tool(
            "wa.reserve",
            json!({
                "pane_id": FIXTURE_PANE_ID,
                "owner_kind": "agent",
                "owner_id": "LilacBay",
                "ttl_ms": 999
            }),
        )
        .err()
        .map(|err| err.to_string())
        .expect("wa.reserve invalid params should fail at schema boundary");
    assert_boundary_error_contains(&boundary_invalid_params_error, "root.ttl_ms");
}

#[test]
fn mcp_conformance_wa_reservations_contract_matches_expected_envelope() {
    let mut harness = new_harness();
    let input_schema = tool_input_schema(&mut harness.client, "wa.reservations");
    assert_schema_matches_manifest("wa.reservations", &input_schema);

    let first = seed_reservation(
        &harness,
        FIXTURE_PANE_ID,
        "agent",
        "LilacBay",
        Some("first reservation"),
        2_000,
    );
    let _second = seed_reservation(
        &harness,
        OTHER_PANE_ID,
        "workflow",
        "wf-123",
        Some("second reservation"),
        3_000,
    );

    let success_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool("wa.reservations", json!({ "pane_id": FIXTURE_PANE_ID }))
            .expect("call wa.reservations"),
    );

    assert_success_envelope_shape(&success_envelope);
    let data = success_envelope["data"]
        .as_object()
        .expect("wa.reservations data object");
    assert_eq!(data["total"], Value::from(1));
    assert_eq!(data["pane_filter"], Value::from(FIXTURE_PANE_ID));
    let reservations = data["reservations"].as_array().expect("reservations array");
    assert_eq!(reservations.len(), 1, "expected one filtered reservation");
    let reservation = reservations.first().expect("filtered reservation");
    assert_eq!(reservation["id"], Value::from(first.id));
    assert_eq!(reservation["pane_id"], Value::from(FIXTURE_PANE_ID));
    assert_eq!(
        reservation["owner_kind"],
        Value::String("agent".to_string())
    );
    assert_eq!(
        reservation["owner_id"],
        Value::String("LilacBay".to_string())
    );
    assert_eq!(
        reservation["reason"],
        Value::String("first reservation".to_string())
    );
    assert_eq!(reservation["status"], Value::String("active".to_string()));
    assert!(reservation.get("released_at").is_none());

    let boundary_invalid_params_error = harness
        .client
        .call_tool("wa.reservations", json!({ "pane_id": -1 }))
        .err()
        .map(|err| err.to_string())
        .expect("wa.reservations invalid params should fail at schema boundary");
    assert_boundary_error_contains(&boundary_invalid_params_error, "root.pane_id");
}

#[test]
fn mcp_conformance_wa_release_contract_matches_expected_envelope() {
    let mut harness = new_harness();
    let input_schema = tool_input_schema(&mut harness.client, "wa.release");
    assert_schema_matches_manifest("wa.release", &input_schema);

    let reservation = seed_reservation(
        &harness,
        FIXTURE_PANE_ID,
        "manual",
        "operator-1",
        Some("release me"),
        5_000,
    );

    let success_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool("wa.release", json!({ "reservation_id": reservation.id }))
            .expect("call wa.release"),
    );

    assert_success_envelope_shape(&success_envelope);
    let data = success_envelope["data"]
        .as_object()
        .expect("wa.release data object");
    assert_eq!(data["reservation_id"], Value::from(reservation.id));
    assert_eq!(data["released"], Value::Bool(true));

    let active = list_active_reservations(&harness);
    assert!(
        active.is_empty(),
        "released reservation should no longer appear in active list"
    );

    let boundary_invalid_params_error = harness
        .client
        .call_tool("wa.release", json!({ "reservation_id": "oops" }))
        .err()
        .map(|err| err.to_string())
        .expect("wa.release invalid params should fail at schema boundary");
    assert_boundary_error_contains(&boundary_invalid_params_error, "reservation_id");
}
