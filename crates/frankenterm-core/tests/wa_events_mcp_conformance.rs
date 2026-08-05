#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkDeliveryAcknowledgingTransport, FrameworkJsonRpcMessage,
    FrameworkTestClient, FrameworkTool, FrameworkTransport, FrameworkTransportError,
    framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{
    EventDeliveryLease, EventDeliveryReservation, EventStreamQuery, PaneRecord, StorageHandle,
    StoredEvent,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;

struct FailResponseTransport<T> {
    inner: T,
    response_count: usize,
    fail_on_response: usize,
}

impl<T> FailResponseTransport<T> {
    fn new(inner: T, fail_on_response: usize) -> Self {
        Self {
            inner,
            response_count: 0,
            fail_on_response,
        }
    }
}

impl<T: FrameworkTransport> FrameworkTransport for FailResponseTransport<T> {
    fn send(
        &mut self,
        cx: &frankenterm_core::cx::Cx,
        message: &FrameworkJsonRpcMessage,
    ) -> Result<(), FrameworkTransportError> {
        if matches!(message, FrameworkJsonRpcMessage::Response(_)) {
            self.response_count += 1;
            if self.response_count == self.fail_on_response {
                // Closing the memory endpoint makes the peer observe a terminal
                // disconnect instead of waiting forever for the injected-lost
                // response.
                let _ = self.inner.close();
                return Err(FrameworkTransportError::Io(std::io::Error::other(
                    "injected MCP response write failure",
                )));
            }
        }
        self.inner.send(cx, message)
    }

    fn recv(
        &mut self,
        cx: &frankenterm_core::cx::Cx,
    ) -> Result<FrameworkJsonRpcMessage, FrameworkTransportError> {
        self.inner.recv(cx)
    }

    fn close(&mut self) -> Result<(), FrameworkTransportError> {
        self.inner.close()
    }
}

impl<T: FrameworkDeliveryAcknowledgingTransport> FrameworkDeliveryAcknowledgingTransport
    for FailResponseTransport<T>
{
    fn send_with_delivery_ack(
        &mut self,
        cx: &frankenterm_core::cx::Cx,
        message: &FrameworkJsonRpcMessage,
    ) -> Result<(), FrameworkTransportError> {
        if matches!(message, FrameworkJsonRpcMessage::Response(_)) {
            self.response_count += 1;
            if self.response_count == self.fail_on_response {
                let _ = self.inner.close();
                return Err(FrameworkTransportError::Io(std::io::Error::other(
                    "injected MCP response write failure",
                )));
            }
        }
        self.inner.send_with_delivery_ack(cx, message)
    }
}

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
        server.run_transport_returning(server_transport);
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

fn parse_toon_tool_envelope(contents: &[FrameworkContent]) -> Value {
    let decoded =
        toon_rust::try_decode(first_text_content(contents), None).expect("decode TOON envelope");
    let json_text =
        toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    serde_json::from_str(&json_text).expect("TOON envelope should stringify back to JSON")
}

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        canonical_value(actual_schema),
        canonical_value(&expected_schema),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = std::collections::BTreeMap::new();
            for (key, child) in map {
                sorted.insert(key.clone(), canonical_value(child));
            }

            let mut rebuilt = serde_json::Map::new();
            for (key, child) in sorted {
                rebuilt.insert(key, child);
            }
            Value::Object(rebuilt)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        _ => value.clone(),
    }
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

    ToolContractCapture {
        tool: tool_name.to_string(),
        input_schema,
        success_envelope,
        boundary_invalid_params_error,
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
    seed_events_fixture_at(&harness.db_path);
}

fn seed_events_fixture_at(db_path: &PathBuf) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
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

// ---------------------------------------------------------------------------
// Coverage gaps documented in ft-yav06:
//   1. Filter correctness (not just pass-through): seed events that should be
//      excluded, assert they don't come back.
//   2. Empty storage returns empty events:[].
//   3. Limit clamps results and total_count reflects underlying row count.
//   4. Upper bound of limit (1001) rejects with the same schema-validation path.
// ---------------------------------------------------------------------------

fn make_event_with(id_hint: u64, rule_id: &str, detected_at: i64) -> StoredEvent {
    StoredEvent {
        id: 0,
        pane_id: FIXTURE_PANE_ID,
        rule_id: rule_id.to_string(),
        agent_type: "codex".to_string(),
        event_type: "usage_limit".to_string(),
        severity: "warning".to_string(),
        confidence: 0.5,
        extracted: None,
        matched_text: Some(format!("fixture {id_hint}")),
        segment_id: None,
        detected_at,
        dedupe_key: Some(format!(
            "wa-events-coverage-{id_hint}-{rule_id}-{detected_at}"
        )),
        handled_at: None,
        handled_by_workflow_id: None,
        handled_status: None,
    }
}

fn seed_many_events(harness: &TestHarness, events: Vec<StoredEvent>) {
    seed_many_events_at(&harness.db_path, events);
}

fn seed_many_events_at(db_path: &PathBuf, events: Vec<StoredEvent>) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("open storage");
        storage
            .upsert_pane(make_pane(FIXTURE_PANE_ID, FIXTURE_TS))
            .await
            .expect("upsert pane");
        for event in events {
            storage
                .record_event(event)
                .await
                .expect("record coverage event");
        }
        storage.shutdown().await.expect("shutdown storage");
    });
}

fn load_fixture_events(db_path: &PathBuf) -> Vec<StoredEvent> {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("open storage");
        let events = storage
            .get_events_stream(EventStreamQuery {
                after_id: Some(0),
                limit: Some(1_000),
                pane_id: Some(FIXTURE_PANE_ID),
                rule_id: None,
                event_type: None,
                triage_state: None,
                label: None,
                unhandled_only: false,
                since: None,
                until: None,
            })
            .await
            .expect("query fixture events");
        storage.shutdown().await.expect("shutdown storage");
        events
    })
}

fn reserve_fixture_event_id(
    db_path: &PathBuf,
    event_id: i64,
    ttl: std::time::Duration,
) -> EventDeliveryLease {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("open storage");
        let reservation = storage
            .reserve_event_delivery(event_id, ttl)
            .await
            .expect("reserve fixture event");
        storage.shutdown().await.expect("shutdown storage");
        match reservation {
            EventDeliveryReservation::Acquired(lease) => lease,
            other => panic!("expected fixture delivery lease, got {other:?}"),
        }
    })
}

fn reserve_fixture_event(db_path: &PathBuf, ttl: std::time::Duration) -> EventDeliveryLease {
    reserve_fixture_event_id(db_path, 1, ttl)
}

fn reserve_fixture_event_ids(
    db_path: &PathBuf,
    event_ids: impl IntoIterator<Item = i64>,
    ttl: std::time::Duration,
) -> Vec<EventDeliveryLease> {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("open storage");
        let mut leases = Vec::new();
        for event_id in event_ids {
            let reservation = storage
                .reserve_event_delivery(event_id, ttl)
                .await
                .unwrap_or_else(|error| panic!("reserve fixture event {event_id}: {error}"));
            match reservation {
                EventDeliveryReservation::Acquired(lease) => leases.push(lease),
                other => panic!("expected fixture delivery lease for {event_id}, got {other:?}"),
            }
        }
        storage.shutdown().await.expect("shutdown storage");
        leases
    })
}

fn release_fixture_event(db_path: &PathBuf, lease: &EventDeliveryLease) {
    release_fixture_events(db_path, std::slice::from_ref(lease));
}

fn release_fixture_events(db_path: &PathBuf, leases: &[EventDeliveryLease]) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("open storage");
        for lease in leases {
            assert!(
                storage
                    .release_event_delivery(lease)
                    .await
                    .expect("release fixture event"),
                "fixture lease for event {} must still be owned",
                lease.event_id()
            );
        }
        storage.shutdown().await.expect("shutdown storage");
    });
}

fn mark_fixture_event_handled(db_path: &PathBuf, event_id: i64) {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime");
    runtime.block_on(async {
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("open storage");
        storage
            .mark_event_handled(
                event_id,
                Some("fixture.concurrent-handler".to_string()),
                "handled",
            )
            .await
            .expect("mark fixture event handled");
        storage.shutdown().await.expect("shutdown storage");
    });
}

fn wait_for_event_delivery_lease(
    db_path: &PathBuf,
    event_id: i64,
    watchdog: std::time::Duration,
) {
    let connection = rusqlite::Connection::open(db_path).expect("open lease observer");
    connection
        .busy_timeout(std::time::Duration::from_secs(1))
        .expect("configure lease observer busy timeout");
    let started = std::time::Instant::now();
    loop {
        let leased: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE id = ?1 AND delivery_lease_token IS NOT NULL",
                [event_id],
                |row| row.get(0),
            )
            .expect("query event delivery lease state");
        if leased == 1 {
            return;
        }
        assert!(
            started.elapsed() < watchdog,
            "event {event_id} was not leased within {watchdog:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn epoch_ms_i64() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock after Unix epoch")
            .as_millis(),
    )
    .expect("test epoch milliseconds fit i64")
}

fn call_events(harness: &mut TestHarness, args: Value) -> Value {
    parse_tool_envelope(
        &harness
            .client
            .call_tool("wa.events", args)
            .expect("call wa.events"),
    )
}

fn current_event_cursor_epoch(db_path: &std::path::Path) -> String {
    let connection = rusqlite::Connection::open(db_path).expect("open cursor epoch fixture DB");
    connection
        .query_row(
            "SELECT cursor_epoch FROM event_retention_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("read current event cursor epoch")
}

fn hash_await_event_scope_field(hasher: &mut sha2::Sha256, field: &[u8]) {
    use sha2::Digest;

    let length = u64::try_from(field.len()).expect("MCP conformance scope field length fits u64");
    hasher.update(length.to_be_bytes());
    hasher.update(field);
}

fn hash_await_event_scope_conditions(
    hasher: &mut sha2::Sha256,
    set_name: &[u8],
    args: &Value,
) {
    let mut conditions = args
        .get(std::str::from_utf8(set_name).expect("ASCII condition-set name"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|condition| {
            condition
                .as_str()
                .expect("MCP conformance condition string")
                .trim()
                .strip_prefix("rule:")
                .expect("MCP conformance uses supported rule conditions")
                .to_string()
        })
        .collect::<Vec<_>>();
    conditions.sort_unstable();
    conditions.dedup();

    hash_await_event_scope_field(hasher, set_name);
    hash_await_event_scope_field(hasher, conditions.len().to_string().as_bytes());
    for glob in conditions {
        hash_await_event_scope_field(hasher, b"rule");
        hash_await_event_scope_field(hasher, glob.as_bytes());
    }
}

fn canonical_await_event_cursor_scope(args: &Value) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hash_await_event_scope_field(
        &mut hasher,
        b"frankenterm.wa.await_event.cursor-scope.v1",
    );
    hash_await_event_scope_conditions(&mut hasher, b"any", args);
    hash_await_event_scope_conditions(&mut hasher, b"all", args);
    hash_await_event_scope_field(&mut hasher, b"pane");
    if let Some(pane_id) = args.get("pane").and_then(Value::as_u64) {
        hash_await_event_scope_field(&mut hasher, b"some");
        hash_await_event_scope_field(&mut hasher, &pane_id.to_be_bytes());
    } else {
        hash_await_event_scope_field(&mut hasher, b"none");
    }
    let claim = args.get("claim").and_then(Value::as_bool).unwrap_or(false);
    let unhandled_only = args
        .get("unhandled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || claim;
    hash_await_event_scope_field(&mut hasher, b"unhandled_only");
    hash_await_event_scope_field(&mut hasher, &[u8::from(unhandled_only)]);
    hash_await_event_scope_field(&mut hasher, b"claim");
    hash_await_event_scope_field(&mut hasher, &[u8::from(claim)]);
    hash_await_event_scope_field(&mut hasher, b"quiescence_mode");
    hash_await_event_scope_field(&mut hasher, b"unsupported-db-events-only");
    hex::encode(hasher.finalize())
}

fn with_canonical_await_event_cursor_scope(mut args: Value) -> Value {
    if args.get("cursor").is_some() && args.get("cursor_scope").is_none() {
        args["cursor_scope"] = Value::String(canonical_await_event_cursor_scope(&args));
    }
    args
}

fn with_current_event_cursor_token(db_path: &std::path::Path, mut args: Value) -> Value {
    if args.get("cursor").is_some() {
        if args.get("cursor_epoch").is_none() {
            args["cursor_epoch"] = Value::String(current_event_cursor_epoch(db_path));
        }
        args = with_canonical_await_event_cursor_scope(args);
    }
    args
}

fn assert_canonical_cursor_epoch(value: &Value) {
    let epoch = value.as_str().expect("cursor epoch string");
    assert_eq!(epoch.len(), 32, "cursor epoch is a 128-bit hex token");
    assert!(
        epoch
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "cursor epoch must be canonical lowercase hexadecimal"
    );
}

fn assert_canonical_cursor_scope(value: &Value) {
    let scope = value.as_str().expect("cursor scope string");
    assert_eq!(scope.len(), 64, "cursor scope is a SHA-256 hex token");
    assert!(
        scope
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "cursor scope must be canonical lowercase hexadecimal"
    );
}

fn assert_await_event_cursor_contract(envelope: &Value, args: &Value) {
    if envelope["ok"] != Value::Bool(true) || !envelope["data"]["final_cursor"].is_number() {
        return;
    }
    assert_canonical_cursor_epoch(&envelope["data"]["final_cursor_epoch"]);
    assert_canonical_cursor_scope(&envelope["data"]["final_cursor_scope"]);
    assert_eq!(
        envelope["data"]["final_cursor_scope"],
        canonical_await_event_cursor_scope(args),
        "the emitted cursor must remain bound to the semantic request scope"
    );
    if let Some(expected_cursor_epoch) = args.get("cursor_epoch") {
        assert_eq!(
            envelope["data"]["final_cursor_epoch"], expected_cursor_epoch,
            "a current-epoch resume must preserve its cursor epoch"
        );
    }
    if let Some(expected_cursor_scope) = args.get("cursor_scope") {
        assert_eq!(
            envelope["data"]["final_cursor_scope"], expected_cursor_scope,
            "a valid resume must preserve its canonical cursor scope"
        );
    }
    assert!(
        envelope["data"]["pending_finalize"].is_boolean(),
        "successful cursor results expose finalization state"
    );
}

fn call_await_event(harness: &mut TestHarness, args: Value) -> Value {
    let args = with_current_event_cursor_token(&harness.db_path, args);
    let envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool("wa.await_event", args.clone())
            .expect("call wa.await_event"),
    );
    assert_await_event_cursor_contract(&envelope, &args);
    envelope
}

fn assert_await_event_success_data(envelope: &Value, claim: bool) {
    let data = envelope["data"]
        .as_object()
        .expect("wa.await_event data object");
    assert_eq!(data["type"], Value::String("await_result".to_string()));
    assert_eq!(data["satisfied"], Value::Bool(true));
    assert_eq!(data["timed_out"], Value::Bool(false));
    assert_eq!(data["final_cursor"], Value::from(if claim { 0 } else { 1 }));
    assert_canonical_cursor_epoch(&data["final_cursor_epoch"]);
    assert_canonical_cursor_scope(&data["final_cursor_scope"]);
    assert_eq!(data["unhandled_only"], Value::Bool(claim));
    assert_eq!(data["claim"], Value::Bool(claim));
    if claim {
        assert_eq!(
            data["claim_delivery"],
            Value::String("pending_finalize_after_delivery_ack".to_string())
        );
        assert_eq!(data["candidate_cursor"], Value::from(1));
        assert_eq!(data["pending_finalize"], Value::Bool(true));
    } else {
        assert!(data.get("claim_delivery").is_none());
        assert!(data.get("candidate_cursor").is_none());
        assert_eq!(data["pending_finalize"], Value::Bool(false));
    }
    assert_eq!(data["any"][0]["condition"], "rule:codex.*");
    assert_eq!(data["any"][0]["met"], Value::Bool(true));

    let events = data["events"]
        .as_array()
        .expect("wa.await_event events array");
    assert_eq!(events.len(), 1, "expected one matching event");
    let event = events.first().expect("matching event");
    assert_eq!(event["id"], Value::from(1));
    assert_eq!(event["pane_id"], Value::from(FIXTURE_PANE_ID));
    assert_eq!(event["rule_id"], Value::String(FIXTURE_RULE_ID.to_string()));
    assert_eq!(
        event["event_type"],
        Value::String("usage_limit".to_string())
    );
    // The payload reflects the durable row as read before transport. A claim
    // is finalized only after the complete response crosses the transport's
    // sender-side delivery boundary, so
    // these pre-finalize fields must never be fabricated.
    assert!(event.get("handled_at").is_none());
    assert!(event.get("workflow_id").is_none());
}

#[test]
fn mcp_conformance_wa_events_empty_storage_returns_empty_events() {
    let mut harness = new_harness();
    let envelope = call_events(&mut harness, json!({"limit": 10}));
    assert_success_envelope_shape(&envelope);
    let data = envelope["data"].as_object().expect("wa.events data object");
    assert_eq!(data["total_count"], Value::from(0));
    assert_eq!(data["events"], json!([]));
}

#[test]
fn mcp_conformance_wa_events_rule_id_filter_excludes_non_matching() {
    // Three events, three different rule_ids, same pane. Filtering on one rule_id
    // must return exactly the matching event — not all three — or the SQL filter
    // is a no-op and the existing happy-path test wouldn't catch it.
    let mut harness = new_harness();
    seed_many_events(
        &harness,
        vec![
            make_event_with(1, "codex.usage.reached", FIXTURE_TS),
            make_event_with(2, "claude_code.compaction.offered", FIXTURE_TS + 1),
            make_event_with(3, "gemini.model.used", FIXTURE_TS + 2),
        ],
    );

    let envelope = call_events(
        &mut harness,
        json!({
            "limit": 10,
            "pane": FIXTURE_PANE_ID,
            "rule_id": "claude_code.compaction.offered",
        }),
    );
    assert_success_envelope_shape(&envelope);
    let data = envelope["data"].as_object().expect("data object");
    let events = data["events"].as_array().expect("events array");
    assert_eq!(
        events.len(),
        1,
        "rule_id filter should return only the matching event, got {events:?}"
    );
    assert_eq!(events[0]["rule_id"], "claude_code.compaction.offered");
    assert_eq!(data["total_count"], Value::from(1));
}

#[test]
fn mcp_conformance_wa_await_event_contract_matches_expected_envelope() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);
    let input_schema = tool_input_schema(&mut harness.client, "wa.await_event");
    assert_schema_matches_manifest("wa.await_event", &input_schema);

    let envelope = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10
        }),
    );
    assert_success_envelope_shape(&envelope);
    assert_await_event_success_data(&envelope, false);

    let err = harness
        .client
        .call_tool(
            "wa.await_event",
            json!({
                "any": ["rule:codex.*"],
                "timeout_secs": 0
            }),
        )
        .err()
        .map(|err| err.to_string())
        .expect("timeout_secs:0 must fail framework schema validation");
    assert!(
        err.contains("[-32602]"),
        "expected framework invalid-params code in error: {err}"
    );
    assert!(
        err.contains("root.timeout_secs: value must be >= 1"),
        "expected wa.await_event timeout lower-bound schema failure in error: {err}"
    );
}

#[test]
fn mcp_conformance_wa_await_event_checkpoint_bootstraps_a_usable_scope_bound_token() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);

    let checkpoint = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "pane": FIXTURE_PANE_ID,
            "checkpoint_only": true
        }),
    );
    assert_success_envelope_shape(&checkpoint);
    assert_eq!(checkpoint["data"]["satisfied"], Value::Bool(false));
    assert_eq!(checkpoint["data"]["timed_out"], Value::Bool(false));
    assert_eq!(checkpoint["data"]["events"], json!([]));
    assert_eq!(checkpoint["data"]["final_cursor"], Value::from(1));
    assert_eq!(
        checkpoint["data"]["bootstrap_state"],
        Value::String("storage_tail_checkpoint".to_string())
    );
    assert!(checkpoint["data"].get("candidate_cursor").is_none());
    assert_eq!(checkpoint["data"]["pending_finalize"], Value::Bool(false));

    seed_many_events(
        &harness,
        vec![make_event_with(2, FIXTURE_RULE_ID, FIXTURE_TS + 1)],
    );
    let resumed = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": checkpoint["data"]["final_cursor"].clone(),
            "cursor_epoch": checkpoint["data"]["final_cursor_epoch"].clone(),
            "cursor_scope": checkpoint["data"]["final_cursor_scope"].clone(),
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10
        }),
    );
    assert_success_envelope_shape(&resumed);
    assert_eq!(resumed["data"]["satisfied"], Value::Bool(true));
    assert_eq!(resumed["data"]["events"][0]["id"], Value::from(2));
    assert_eq!(resumed["data"]["final_cursor"], Value::from(2));
    assert!(resumed["data"].get("candidate_cursor").is_none());
    assert_eq!(resumed["data"]["pending_finalize"], Value::Bool(false));
}

#[test]
fn mcp_conformance_wa_await_event_claim_marks_event_handled() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);

    let envelope = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    assert_success_envelope_shape(&envelope);
    assert_await_event_success_data(&envelope, true);

    let after_claim = call_events(
        &mut harness,
        json!({
            "limit": 10,
            "pane": FIXTURE_PANE_ID,
            "unhandled": true
        }),
    );
    let data = after_claim["data"]
        .as_object()
        .expect("wa.events data object");
    assert_eq!(
        data["events"],
        json!([]),
        "claimed event must no longer appear in unhandled wa.events results"
    );
}

#[test]
fn mcp_conformance_wa_await_event_claim_toon_finalizes_after_transport() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);

    let arguments = with_current_event_cursor_token(
        &harness.db_path,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10,
            "claim": true,
            "format": "toon"
        }),
    );
    let contents = harness
        .client
        .call_tool("wa.await_event", arguments.clone())
        .expect("call TOON wa.await_event claim");
    let envelope = parse_toon_tool_envelope(&contents);
    assert_success_envelope_shape(&envelope);
    assert_await_event_cursor_contract(&envelope, &arguments);
    assert_await_event_success_data(&envelope, true);

    // A memory-transport peer can receive the response after its channel send
    // succeeds but while the server-side post-send finalizer is still running.
    // The server loop is sequential, so a subsequent request is a deterministic
    // barrier proving that finalization completed without weakening the
    // at-least-once delivery contract.
    let _barrier = call_events(&mut harness, json!({"limit": 1}));
    let events = load_fixture_events(&harness.db_path);
    assert_eq!(events.len(), 1);
    assert!(events[0].handled_at.is_some());
    assert_eq!(
        events[0].handled_by_workflow_id.as_deref(),
        Some("mcp.wa.await_event")
    );
}

#[test]
fn mcp_conformance_idless_long_poll_is_rejected_before_dispatch() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);
    let cx = frankenterm_core::cx::Cx::for_request();
    let notification_arguments = with_current_event_cursor_token(
        &harness.db_path,
        json!({
            "any": ["rule:never.matches"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 6,
            "poll_interval_ms": 1_000,
            "claim": true
        }),
    );
    let notification: FrameworkJsonRpcMessage = serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": "wa.await_event",
            "arguments": notification_arguments
        }
    }))
    .expect("construct id-less tools/call notification");

    harness
        .client
        .transport_mut()
        .send(&cx, &notification)
        .expect("send claim-capable notification");
    // The server processes transport messages sequentially. This ordinary
    // request forces it past the notification and proves the notification's
    // unresolved action was released before this response was written. Use a
    // synchronization channel and a generous watchdog instead of a brittle
    // sub-second wall-time assertion. The notification's own six-second bound
    // ensures the worker can still be joined after a failure without leaving a
    // long-running detached test thread.
    let TestHarness {
        _workspace,
        db_path,
        mut client,
    } = harness;
    let (barrier_tx, barrier_rx) = std::sync::mpsc::sync_channel(1);
    let barrier_worker = std::thread::spawn(move || {
        let result = client.list_tools().map(|_| ());
        let _ = barrier_tx.send(result);
    });
    let barrier_result = barrier_rx.recv_timeout(std::time::Duration::from_secs(5));
    if barrier_result.is_err() {
        barrier_worker
            .join()
            .expect("join delayed post-notification barrier worker");
        panic!(
            "an id-less long poll monopolized the sequential server beyond the five-second watchdog"
        );
    }
    barrier_result
        .expect("barrier result checked above")
        .expect("send post-notification response barrier");
    barrier_worker
        .join()
        .expect("join post-notification barrier worker");

    let events = load_fixture_events(&db_path);
    assert_eq!(events.len(), 1);
    assert!(
        events[0].handled_at.is_none(),
        "an id-less tool call has no response boundary and must never dispatch/finalize"
    );
    let lease = reserve_fixture_event(&db_path, std::time::Duration::from_secs(1));
    release_fixture_event(&db_path, &lease);
}

fn assert_await_event_claim_send_failure_releases_lease(
    requested_format: Option<&str>,
    db_name: &str,
) {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    let db_path = workspace.path().join(db_name);
    let server = build_server_with_db(&Config::default(), Some(db_path.clone()))
        .expect("build MCP server");
    let (client_transport, server_transport) = framework_create_memory_transport_pair();
    let server_thread = std::thread::spawn(move || {
        // Response 1 is initialize. Response 2 is wa.await_event and is the
        // injected transport failure whose staged lease must be released.
        server.run_transport_returning(FailResponseTransport::new(server_transport, 2));
    });
    let mut client = FrameworkTestClient::new(client_transport);
    client.initialize().expect("initialize MCP client");
    seed_events_fixture_at(&db_path);

    let mut arguments = with_current_event_cursor_token(&db_path, json!({
        "any": ["rule:codex.*"],
        "cursor": 0,
        "pane": FIXTURE_PANE_ID,
        "timeout_secs": 1,
        "poll_interval_ms": 10,
        "claim": true
    }));
    if let Some(format) = requested_format {
        arguments["format"] = Value::String(format.to_string());
    }
    let error = client
        .call_tool("wa.await_event", arguments)
        .expect_err("injected response failure must disconnect the client");
    assert!(
        error.to_string().contains("closed") || error.to_string().contains("Closed"),
        "unexpected transport failure: {error}"
    );
    server_thread.join().expect("join failed-response server");

    let events = load_fixture_events(&db_path);
    assert_eq!(events.len(), 1);
    assert!(
        events[0].handled_at.is_none(),
        "failed response must not mark its event handled"
    );

    // Immediate reacquisition proves the known-failure path released the lease
    // instead of merely relying on crash-expiry recovery.
    let lease = reserve_fixture_event(&db_path, std::time::Duration::from_secs(1));
    release_fixture_event(&db_path, &lease);
}

#[test]
fn mcp_conformance_wa_await_event_claim_json_send_failure_releases_lease() {
    assert_await_event_claim_send_failure_releases_lease(
        Some("json"),
        "mcp-json-send-failure.sqlite3",
    );
}

#[test]
fn mcp_conformance_wa_await_event_claim_toon_send_failure_releases_lease() {
    assert_await_event_claim_send_failure_releases_lease(
        Some("toon"),
        "mcp-toon-send-failure.sqlite3",
    );
}

#[test]
fn mcp_conformance_wa_await_event_live_lease_retains_cursor_until_retry() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);
    let competing_lease =
        reserve_fixture_event(&harness.db_path, std::time::Duration::from_secs(5));

    let timed_out = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    assert_success_envelope_shape(&timed_out);
    assert_eq!(timed_out["data"]["satisfied"], Value::Bool(false));
    assert_eq!(timed_out["data"]["timed_out"], Value::Bool(true));
    assert_eq!(timed_out["data"]["final_cursor"], Value::from(0));
    assert_eq!(timed_out["data"]["events"], json!([]));
    assert!(timed_out["data"].get("candidate_cursor").is_none());
    assert!(timed_out["data"].get("claim_delivery").is_none());
    assert_eq!(timed_out["data"]["pending_finalize"], Value::Bool(false));

    release_fixture_event(&harness.db_path, &competing_lease);
    let retried = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    assert_success_envelope_shape(&retried);
    assert_await_event_success_data(&retried, true);
    let _delivery_barrier = call_events(&mut harness, json!({"limit": 1}));
}

#[test]
fn mcp_conformance_wa_await_event_observes_early_lease_release_before_timeout() {
    let mut harness = new_harness();
    seed_events_fixture(&harness);
    let competing_lease =
        reserve_fixture_event(&harness.db_path, std::time::Duration::from_secs(5));
    let release_db_path = harness.db_path.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        release_fixture_event(&release_db_path, &competing_lease);
    });

    let acquired = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 2,
            "poll_interval_ms": 20,
            "claim": true
        }),
    );
    releaser.join().expect("join early lease releaser");
    assert_success_envelope_shape(&acquired);
    assert_await_event_success_data(&acquired, true);
    let _delivery_barrier = call_events(&mut harness, json!({"limit": 1}));
}

#[test]
fn mcp_conformance_retried_hole_across_large_backlog_preserves_order_and_cursor() {
    let mut harness = new_harness();
    let mut backlog = Vec::with_capacity(502);
    backlog.push(make_event_with(1, "rule.a", FIXTURE_TS));
    for id in 2..=501 {
        backlog.push(make_event_with(
            id,
            "noise.unmatched",
            FIXTURE_TS + i64::try_from(id).expect("fixture id fits i64"),
        ));
    }
    backlog.push(make_event_with(502, "rule.b", FIXTURE_TS + 502));
    seed_many_events(&harness, backlog);
    let competing_lease = reserve_fixture_event_id(
        &harness.db_path,
        1,
        std::time::Duration::from_secs(5),
    );
    let release_db_path = harness.db_path.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        release_fixture_event(&release_db_path, &competing_lease);
    });

    let acquired = call_await_event(
        &mut harness,
        json!({
            "all": ["rule:rule.a", "rule:rule.b"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 2,
            "poll_interval_ms": 20,
            "claim": true
        }),
    );
    releaser.join().expect("join ordered-hole lease releaser");
    assert_success_envelope_shape(&acquired);
    assert_eq!(acquired["data"]["satisfied"], Value::Bool(true));
    assert_eq!(acquired["data"]["timed_out"], Value::Bool(false));
    assert_eq!(acquired["data"]["final_cursor"], Value::from(0));
    assert_eq!(acquired["data"]["candidate_cursor"], Value::from(502));
    assert_eq!(acquired["data"]["pending_finalize"], Value::Bool(true));
    assert_eq!(
        acquired["data"]["events"]
            .as_array()
            .expect("ordered matched events")
            .iter()
            .map(|event| event["id"].as_i64().expect("event id"))
            .collect::<Vec<_>>(),
        vec![1, 502],
        "acquisition order 502 then 1 must not leak into the ascending-ID response contract"
    );
    assert!(
        acquired["data"]["all"]
            .as_array()
            .expect("all-condition statuses")
            .iter()
            .all(|condition| condition["met"] == Value::Bool(true))
    );

    let _delivery_barrier = call_events(&mut harness, json!({"limit": 1}));
    let events = load_fixture_events(&harness.db_path);
    assert_eq!(events.len(), 502);
    assert!(events[0].handled_at.is_some());
    assert!(events[501].handled_at.is_some());
    assert!(
        events[1..501]
            .iter()
            .all(|event| event.handled_at.is_none()),
        "unmatched backlog rows must not be claimed"
    );
}

#[test]
fn mcp_conformance_foreign_hole_outlives_met_mask_until_exact_refetch() {
    let harness = new_harness();
    seed_many_events(
        &harness,
        vec![
            make_event_with(1, "rule.a", FIXTURE_TS),
            make_event_with(2, "rule.a", FIXTURE_TS + 1),
        ],
    );
    let competing_lease = reserve_fixture_event_id(
        &harness.db_path,
        1,
        std::time::Duration::from_secs(10),
    );

    let db_path = harness.db_path.clone();
    let mut await_client = spawn_client(Some(db_path.clone()));
    let await_arguments = with_current_event_cursor_token(
        &db_path,
        json!({
            "all": ["rule:rule.a", "rule:rule.b"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 5,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    let expected_cursor_epoch = await_arguments["cursor_epoch"].clone();
    let expected_cursor_scope = await_arguments["cursor_scope"].clone();
    let waiter = std::thread::spawn(move || {
        let envelope = parse_tool_envelope(
            &await_client
                .call_tool("wa.await_event", await_arguments)
                .expect("call delayed-B A/A/B await"),
        );
        await_client
            .call_tool("wa.events", json!({"limit": 1}))
            .expect("delayed-B delivery barrier");
        envelope
    });

    // Synchronize on id=2 being leased by the waiter. At that point the A
    // condition is met while id=1 remains a foreign-owned cursor hole.
    wait_for_event_delivery_lease(&db_path, 2, std::time::Duration::from_secs(5));
    release_fixture_event(&db_path, &competing_lease);
    seed_many_events_at(
        &db_path,
        vec![make_event_with(3, "rule.b", FIXTURE_TS + 2)],
    );

    let acquired = waiter.join().expect("join delayed-B A/A/B waiter");
    assert_success_envelope_shape(&acquired);
    assert_eq!(acquired["data"]["satisfied"], Value::Bool(true));
    assert_eq!(acquired["data"]["timed_out"], Value::Bool(false));
    assert_eq!(acquired["data"]["final_cursor"], Value::from(0));
    assert_eq!(acquired["data"]["candidate_cursor"], Value::from(3));
    assert_eq!(acquired["data"]["pending_finalize"], Value::Bool(true));
    assert_eq!(
        acquired["data"]["final_cursor_epoch"], expected_cursor_epoch
    );
    assert_eq!(
        acquired["data"]["final_cursor_scope"], expected_cursor_scope
    );
    assert_eq!(
        acquired["data"]["events"]
            .as_array()
            .expect("delayed-B matched events")
            .iter()
            .map(|event| event["id"].as_i64().expect("event id"))
            .collect::<Vec<_>>(),
        vec![1, 2, 3],
        "the id=1 hole must survive after id=2 satisfies the same A mask"
    );

    let events = load_fixture_events(&db_path);
    assert_eq!(events.len(), 3);
    assert!(
        events.iter().all(|event| event.handled_at.is_some()),
        "every emitted A/A/B event must finalize after the delivery barrier"
    );
}

#[test]
fn mcp_conformance_exact_hole_refetch_does_not_substitute_next_unhandled_row() {
    let harness = new_harness();
    seed_many_events(
        &harness,
        vec![
            make_event_with(1, "rule.a", FIXTURE_TS),
            make_event_with(2, "rule.b", FIXTURE_TS + 1),
        ],
    );
    let _competing_lease = reserve_fixture_event_id(
        &harness.db_path,
        1,
        std::time::Duration::from_secs(10),
    );

    let db_path = harness.db_path.clone();
    let mut await_client = spawn_client(Some(db_path.clone()));
    let await_arguments = with_current_event_cursor_token(
        &db_path,
        json!({
            "all": ["rule:rule.a", "rule:rule.b"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 5,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    let expected_cursor_epoch = await_arguments["cursor_epoch"].clone();
    let expected_cursor_scope = await_arguments["cursor_scope"].clone();
    let waiter = std::thread::spawn(move || {
        let envelope = parse_tool_envelope(
            &await_client
                .call_tool("wa.await_event", await_arguments)
                .expect("call exact-refetch await"),
        );
        await_client
            .call_tool("wa.events", json!({"limit": 1}))
            .expect("exact-refetch delivery barrier");
        envelope
    });

    wait_for_event_delivery_lease(&db_path, 2, std::time::Duration::from_secs(5));
    // This clears id=1's lease and makes it invisible to the exact unhandled
    // refetch. The query's first row is now id=2, which must not be mistaken
    // for id=1 merely because it is the next row after cursor 0.
    mark_fixture_event_handled(&db_path, 1);
    seed_many_events_at(
        &db_path,
        vec![make_event_with(3, "rule.a", FIXTURE_TS + 2)],
    );

    let acquired = waiter.join().expect("join exact-refetch waiter");
    assert_success_envelope_shape(&acquired);
    assert_eq!(acquired["data"]["final_cursor"], Value::from(1));
    assert_eq!(acquired["data"]["candidate_cursor"], Value::from(3));
    assert_eq!(acquired["data"]["pending_finalize"], Value::Bool(true));
    assert_eq!(
        acquired["data"]["final_cursor_epoch"], expected_cursor_epoch
    );
    assert_eq!(
        acquired["data"]["final_cursor_scope"], expected_cursor_scope
    );
    assert_eq!(
        acquired["data"]["events"]
            .as_array()
            .expect("exact-refetch matched events")
            .iter()
            .map(|event| event["id"].as_i64().expect("event id"))
            .collect::<Vec<_>>(),
        vec![2, 3],
        "handled id=1 must be dropped without substituting id=2 as its refetch"
    );
    let events = load_fixture_events(&db_path);
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].handled_by_workflow_id.as_deref(),
        Some("fixture.concurrent-handler")
    );
    assert!(events[1].handled_at.is_some());
    assert!(events[2].handled_at.is_some());
}

#[test]
fn mcp_conformance_blocked_hole_cap_fails_closed_across_pages() {
    const BLOCKED_CAP: i64 = 500;
    let mut harness = new_harness();
    seed_many_events(
        &harness,
        (1..=BLOCKED_CAP + 1)
            .map(|id| {
                make_event_with(
                    u64::try_from(id).expect("positive fixture id fits u64"),
                    "rule.a",
                    FIXTURE_TS + id,
                )
            })
            .collect(),
    );
    let competing_leases = reserve_fixture_event_ids(
        &harness.db_path,
        1..=BLOCKED_CAP + 1,
        std::time::Duration::from_secs(60),
    );

    let saturated = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:rule.a"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 5,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    assert_common_envelope_fields(&saturated, false);
    assert_eq!(saturated["error_code"], "FT-MCP-0005");
    assert_eq!(
        saturated["error"],
        "wa.await_event cannot safely track more than 500 concurrently leased matching events"
    );

    // Saturation occurred on page two before id=501 could become an untracked
    // cursor advance. Reusing the original cursor after releasing only id=1
    // must still discover and claim the earliest event.
    release_fixture_event(&harness.db_path, &competing_leases[0]);
    let retried = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:rule.a"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 2,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    assert_success_envelope_shape(&retried);
    assert_eq!(retried["data"]["events"][0]["id"], Value::from(1));
    assert_eq!(retried["data"]["final_cursor"], Value::from(0));
    assert_eq!(retried["data"]["candidate_cursor"], Value::from(1));
    assert_eq!(retried["data"]["pending_finalize"], Value::Bool(true));
    let _delivery_barrier = call_events(&mut harness, json!({"limit": 1}));
    release_fixture_events(&harness.db_path, &competing_leases[1..]);
}

#[test]
fn mcp_conformance_storage_paths_are_redacted_from_event_tool_errors() {
    let workspace = tempfile::tempdir().expect("create redaction workspace");
    let secret_marker = "operator-secret-mcp-database";
    let invalid_db_path = workspace.path().join(secret_marker);
    std::fs::create_dir(&invalid_db_path).expect("create directory at invalid database path");
    let mut client = spawn_client(Some(invalid_db_path.clone()));

    for (tool, arguments) in [
        ("wa.events", json!({"limit": 1})),
        (
            "wa.await_event",
            with_canonical_await_event_cursor_scope(json!({
                    "any": ["rule:rule.a"],
                    "cursor": 0,
                    "cursor_epoch": "00000000000000000000000000000000",
                    "timeout_secs": 1,
                    "poll_interval_ms": 10
                })),
        ),
    ] {
        let envelope = parse_tool_envelope(
            &client
                .call_tool(tool, arguments)
                .unwrap_or_else(|error| panic!("call {tool} redaction case: {error}")),
        );
        assert_common_envelope_fields(&envelope, false);
        assert_eq!(envelope["error_code"], "FT-MCP-0005");
        assert_eq!(envelope["error"], "Storage unavailable");
        let serialized = envelope.to_string();
        let invalid_db_path_text = invalid_db_path.to_string_lossy();
        assert!(!serialized.contains(secret_marker));
        assert!(!serialized.contains(invalid_db_path_text.as_ref()));
    }
}

#[test]
fn mcp_conformance_no_cursor_boundary_precedes_delayed_storage_open() {
    let harness = new_harness();
    // Initialize schema and pane metadata before taking the deliberate writer
    // lock. The awaited event itself is inserted while the handler is blocked
    // opening storage.
    seed_many_events(&harness, Vec::new());
    let lock_connection =
        rusqlite::Connection::open(&harness.db_path).expect("open delayed-open lock connection");
    lock_connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .expect("configure delayed-open busy timeout");
    lock_connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold delayed-open writer lock");

    let db_path = harness.db_path.clone();
    let mut await_client = spawn_client(Some(db_path));
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let waiter = std::thread::spawn(move || {
        started_tx.send(()).expect("signal delayed-open call start");
        let result = await_client
            .call_tool(
                "wa.await_event",
                json!({
                    "any": ["rule:rule.open_window"],
                    "pane": FIXTURE_PANE_ID,
                    "timeout_secs": 2,
                    "poll_interval_ms": 10
                }),
            )
            .map(|contents| parse_tool_envelope(&contents))
            .map_err(|error| error.to_string());
        let _ = result_tx.send(result);
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("delayed-open request thread started");
    // Give the in-memory server a generous scheduling window to enter the
    // storage-open path held by the transaction above.
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        matches!(
            result_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ),
        "delayed-open fixture must hold the request before it can respond"
    );

    let detected_during_open_ms = epoch_ms_i64();
    lock_connection
        .execute(
            "INSERT INTO events (
                id, pane_id, rule_id, agent_type, event_type, severity, confidence,
                matched_text, detected_at, dedupe_key
             ) SELECT max_event_id + 1, ?1, ?2, 'codex', 'usage_limit', 'warning',
                      0.5, ?3, ?4, ?5
               FROM event_retention_state WHERE singleton = 1",
            rusqlite::params![
                i64::try_from(FIXTURE_PANE_ID).expect("fixture pane id fits i64"),
                "rule.open_window",
                "detected while storage open was blocked",
                detected_during_open_ms,
                "wa-events-delayed-open"
            ],
        )
        .expect("insert event during delayed storage open");
    lock_connection
        .execute_batch("COMMIT")
        .expect("release delayed-open writer lock");

    let envelope = result_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("delayed-open request completed after lock release")
        .expect("delayed-open MCP call succeeded");
    waiter.join().expect("join delayed-open waiter");
    assert_success_envelope_shape(&envelope);
    assert_eq!(envelope["data"]["satisfied"], Value::Bool(true));
    assert_eq!(envelope["data"]["timed_out"], Value::Bool(false));
    assert_eq!(envelope["data"]["events"][0]["id"], Value::from(1));
    assert_canonical_cursor_epoch(&envelope["data"]["final_cursor_epoch"]);
    assert_canonical_cursor_scope(&envelope["data"]["final_cursor_scope"]);
    assert!(envelope["data"].get("candidate_cursor").is_none());
    assert_eq!(envelope["data"]["pending_finalize"], Value::Bool(false));
}

#[test]
fn mcp_conformance_live_lease_does_not_block_a_later_matching_event() {
    let mut harness = new_harness();
    seed_many_events(
        &harness,
        vec![
            make_event_with(1, FIXTURE_RULE_ID, FIXTURE_TS),
            make_event_with(2, FIXTURE_RULE_ID, FIXTURE_TS + 1),
        ],
    );
    let competing_lease =
        reserve_fixture_event(&harness.db_path, std::time::Duration::from_secs(5));

    let claimed = call_await_event(
        &mut harness,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    assert_success_envelope_shape(&claimed);
    assert_eq!(claimed["data"]["satisfied"], Value::Bool(true));
    assert_eq!(claimed["data"]["events"].as_array().map(Vec::len), Some(1));
    assert_eq!(claimed["data"]["events"][0]["id"], Value::from(2));
    assert_eq!(
        claimed["data"]["final_cursor"],
        Value::from(0),
        "the exposed cursor must remain before the still-leased id=1 hole"
    );
    assert_eq!(claimed["data"]["candidate_cursor"], Value::from(0));
    assert_eq!(claimed["data"]["pending_finalize"], Value::Bool(true));

    let _delivery_barrier = call_events(&mut harness, json!({"limit": 1}));
    let events = load_fixture_events(&harness.db_path);
    assert!(events[0].handled_at.is_none());
    assert!(events[1].handled_at.is_some());
    release_fixture_event(&harness.db_path, &competing_lease);
}

#[test]
fn mcp_conformance_wa_await_event_concurrent_claimers_emit_event_once() {
    let workspace = tempfile::tempdir().expect("create temp workspace");
    let db_path = workspace.path().join("mcp-concurrent-claims.sqlite3");
    let mut client_a = spawn_client(Some(db_path.clone()));
    let mut client_b = spawn_client(Some(db_path.clone()));
    seed_events_fixture_at(&db_path);
    let args = with_current_event_cursor_token(
        &db_path,
        json!({
            "any": ["rule:codex.*"],
            "cursor": 0,
            "pane": FIXTURE_PANE_ID,
            "timeout_secs": 1,
            "poll_interval_ms": 10,
            "claim": true
        }),
    );
    let expected_cursor_epoch = args["cursor_epoch"].clone();
    let expected_cursor_scope = args["cursor_scope"].clone();
    let args_b = args.clone();

    let claim_a = std::thread::spawn(move || {
        let envelope = parse_tool_envelope(
            &client_a
                .call_tool("wa.await_event", args)
                .expect("call concurrent claimant A"),
        );
        client_a
            .call_tool("wa.events", json!({"limit": 1}))
            .expect("claimant A delivery barrier");
        envelope
    });
    let claim_b = std::thread::spawn(move || {
        let envelope = parse_tool_envelope(
            &client_b
                .call_tool("wa.await_event", args_b)
                .expect("call concurrent claimant B"),
        );
        client_b
            .call_tool("wa.events", json!({"limit": 1}))
            .expect("claimant B delivery barrier");
        envelope
    });
    let envelope_a = claim_a.join().expect("join claimant A");
    let envelope_b = claim_b.join().expect("join claimant B");
    assert_eq!(
        envelope_a["data"]["final_cursor_epoch"], expected_cursor_epoch
    );
    assert_eq!(
        envelope_b["data"]["final_cursor_epoch"], expected_cursor_epoch
    );
    assert_eq!(
        envelope_a["data"]["final_cursor_scope"], expected_cursor_scope
    );
    assert_eq!(
        envelope_b["data"]["final_cursor_scope"], expected_cursor_scope
    );

    let emitted_a = envelope_a["data"]["events"]
        .as_array()
        .expect("claimant A events")
        .len();
    let emitted_b = envelope_b["data"]["events"]
        .as_array()
        .expect("claimant B events")
        .len();
    assert_eq!(
        emitted_a + emitted_b,
        1,
        "atomic reservation must permit exactly one emitted claim"
    );
    assert_eq!(
        [&envelope_a, &envelope_b]
            .iter()
            .filter(|envelope| envelope["data"]["satisfied"] == Value::Bool(true))
            .count(),
        1,
        "exactly one concurrent claimant must satisfy on the single event"
    );

    for envelope in [&envelope_a, &envelope_b] {
        let emitted = envelope["data"]["events"]
            .as_array()
            .expect("concurrent claimant events")
            .len();
        if emitted == 1 {
            assert_eq!(envelope["data"]["final_cursor"], Value::from(0));
            assert_eq!(envelope["data"]["candidate_cursor"], Value::from(1));
            assert_eq!(envelope["data"]["pending_finalize"], Value::Bool(true));
        } else {
            let loser_cursor = envelope["data"]["final_cursor"].as_i64();
            assert!(
                loser_cursor == Some(0) || loser_cursor == Some(1),
                "the losing claimant may retain the live hole or observe its finalization"
            );
            assert!(envelope["data"].get("candidate_cursor").is_none());
            assert_eq!(envelope["data"]["pending_finalize"], Value::Bool(false));
        }
    }

    let events = load_fixture_events(&db_path);
    assert_eq!(events.len(), 1);
    assert!(events[0].handled_at.is_some());
}

#[test]
fn mcp_conformance_wa_events_limit_caps_results_and_orders_newest_first() {
    // Seed five events spaced one ms apart. limit:3 must return three events,
    // newest-first (highest detected_at first). Pins both the clamp and the
    // documented ordering, which the single-event happy-path test can't.
    let mut harness = new_harness();
    let events: Vec<StoredEvent> = (0..5)
        .map(|i| make_event_with(i as u64, FIXTURE_RULE_ID, FIXTURE_TS + i64::from(i)))
        .collect();
    seed_many_events(&harness, events);

    let envelope = call_events(&mut harness, json!({"limit": 3, "pane": FIXTURE_PANE_ID}));
    assert_success_envelope_shape(&envelope);
    let data = envelope["data"].as_object().expect("data object");
    let returned = data["events"].as_array().expect("events array");
    assert_eq!(
        returned.len(),
        3,
        "limit:3 must clamp a 5-row result to 3, got {} rows",
        returned.len()
    );
    assert_eq!(
        data["total_count"],
        Value::from(3),
        "total_count on wa.events reflects the returned row count, not pre-limit cardinality",
    );

    let timestamps: Vec<i64> = returned
        .iter()
        .map(|event| event["captured_at"].as_i64().expect("captured_at i64"))
        .collect();
    let mut sorted_desc = timestamps.clone();
    sorted_desc.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        timestamps, sorted_desc,
        "wa.events must return rows newest-first (detected_at DESC); got {timestamps:?}"
    );
}

#[test]
fn mcp_conformance_wa_events_rejects_limit_above_schema_maximum() {
    // Schema says limit: {minimum: 1, maximum: 1000}. The existing happy-path
    // test covers limit:0 at the lower bound; limit:1001 at the upper bound
    // must fail the same framework InvalidParams path, not silently clamp.
    let mut harness = new_harness();
    let err = harness
        .client
        .call_tool("wa.events", json!({"limit": 1001}))
        .err()
        .map(|e| e.to_string())
        .expect("limit:1001 must fail framework schema validation");
    assert!(
        err.contains("[-32602]"),
        "expected framework invalid-params code in error: {err}"
    );
    assert!(
        err.contains("root.limit: value must be <= 1000"),
        "expected wa.events limit upper-bound schema failure in error: {err}"
    );
}
