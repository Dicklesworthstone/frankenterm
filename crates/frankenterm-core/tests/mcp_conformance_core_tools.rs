#![cfg(feature = "mcp")]

use frankenterm_core::config::{
    Config, PolicyRule, PolicyRuleDecision, PolicyRuleMatch, PolicyRulesConfig,
};
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkMcpError, FrameworkTestClient, FrameworkTool,
    framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{FtsSyncConfig, PaneRecord, SearchOptions, StorageHandle};
use frankenterm_core::wezterm::set_wezterm_cli_override;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tempfile::TempDir;

struct TestHarness {
    client: FrameworkTestClient,
    db_path: PathBuf,
    // Struct fields drop in declaration order: reset the global override before
    // removing its fake CLI tempdir, then release the process-wide env lock last.
    _override_guard: WeztermCliOverrideGuard,
    _fake_wezterm: FakeWezterm,
    _workspace: TempDir,
    _env_lock: MutexGuard<'static, ()>,
}

#[derive(Debug)]
struct PolicyDeniedAuditRow {
    id: i64,
    tool_name: String,
    decision: String,
    reason_code: String,
    reason: String,
    rule_id: Option<String>,
}

const INCIDENT_SECRET: &str = "sk-abc123456789012345678901234567890123456789012345678901";
const INCIDENT_GET_TEXT_PANE_ID: u64 = 7_373;
const INCIDENT_SEARCH_PANE_ID: u64 = 8_484;

#[derive(Serialize)]
struct ToolGoldenCapture {
    tool: String,
    input_schema: Value,
    success_envelope: Value,
    invalid_args_response: Value,
}

struct FakeWezterm {
    _workspace: TempDir,
    cli_path: PathBuf,
}

struct WeztermCliOverrideGuard;

impl Drop for WeztermCliOverrideGuard {
    fn drop(&mut self) {
        set_wezterm_cli_override(None);
    }
}

fn wezterm_env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl FakeWezterm {
    fn new() -> Self {
        let workspace = tempfile::tempdir().expect("create fake wezterm workspace");
        let state_dir = workspace.path().join("state");
        let text_dir = state_dir.join("texts");
        fs::create_dir_all(&text_dir).expect("create fake wezterm text dir");
        fs::write(
            state_dir.join("panes.json"),
            serde_json::to_string_pretty(&fake_panes()).expect("serialize fake panes"),
        )
        .expect("write fake panes");
        fs::write(text_dir.join("4242.txt"), "alpha\nbeta\ngamma\ndelta\n")
            .expect("write fake get_text pane");
        fs::write(text_dir.join("5252.txt"), "prompt> ").expect("write fake send pane");
        fs::write(text_dir.join("6262.txt"), "build ready").expect("write fake wait_for pane");
        fs::write(
            text_dir.join(format!("{INCIDENT_GET_TEXT_PANE_ID}.txt")),
            format!("incident alpha\nOPENAI_API_KEY={INCIDENT_SECRET}\nomega\n"),
        )
        .expect("write incident get_text pane");

        let cli_path = workspace.path().join("fake-wezterm.py");
        fs::write(&cli_path, fake_wezterm_script(&state_dir)).expect("write fake wezterm cli");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&cli_path)
                .expect("fake cli metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&cli_path, perms).expect("chmod fake wezterm cli");
        }

        Self {
            _workspace: workspace,
            cli_path,
        }
    }

    fn install(&self) -> WeztermCliOverrideGuard {
        set_wezterm_cli_override(Some(self.cli_path.to_string_lossy().into_owned()));
        WeztermCliOverrideGuard
    }
}

impl TestHarness {
    fn new() -> Self {
        Self::new_with_config(default_mcp_test_config())
    }

    fn new_with_config(config: Config) -> Self {
        let env_lock = wezterm_env_lock();
        let fake_wezterm = FakeWezterm::new();
        let override_guard = fake_wezterm.install();
        let workspace = tempfile::tempdir().expect("create conformance workspace");
        let db_path = workspace.path().join("mcp.sqlite3");
        seed_search_db(&db_path);
        let client = spawn_client_with_config(config, Some(db_path.clone()));
        Self {
            client,
            db_path,
            _override_guard: override_guard,
            _fake_wezterm: fake_wezterm,
            _workspace: workspace,
            _env_lock: env_lock,
        }
    }
}

fn fake_panes() -> Value {
    json!([
        {
            "pane_id": 1,
            "tab_id": 1,
            "window_id": 1,
            "domain_name": "local",
            "title": "codex-search",
            "cwd": "file:///tmp/ft-search"
        },
        {
            "pane_id": 4242,
            "tab_id": 1,
            "window_id": 1,
            "domain_name": "local",
            "title": "codex-get-text",
            "cwd": "file:///tmp/ft-get-text"
        },
        {
            "pane_id": 5252,
            "tab_id": 1,
            "window_id": 1,
            "domain_name": "local",
            "title": "codex-send",
            "cwd": "file:///tmp/ft-send"
        },
        {
            "pane_id": 6262,
            "tab_id": 1,
            "window_id": 1,
            "domain_name": "local",
            "title": "codex-wait",
            "cwd": "file:///tmp/ft-wait"
        },
        {
            "pane_id": INCIDENT_GET_TEXT_PANE_ID,
            "tab_id": 1,
            "window_id": 1,
            "domain_name": "local",
            "title": "incident-get-text",
            "cwd": "file:///tmp/ft-incident-get-text"
        },
        {
            "pane_id": INCIDENT_SEARCH_PANE_ID,
            "tab_id": 1,
            "window_id": 1,
            "domain_name": "local",
            "title": "incident-search",
            "cwd": "file:///tmp/ft-incident-search"
        }
    ])
}

fn fake_wezterm_script(state_dir: &Path) -> String {
    let state_dir_literal =
        serde_json::to_string(&state_dir.display().to_string()).expect("state dir path json");
    format!(
        r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

state_dir = Path({state_dir_literal})
panes = json.loads((state_dir / "panes.json").read_text())
texts = state_dir / "texts"

args = sys.argv[1:]
if len(args) < 2 or args[0] != "cli":
    print(f"unsupported args: {{args}}", file=sys.stderr)
    sys.exit(2)

command = args[1]

def pane_path(pane_id: int) -> Path:
    return texts / f"{{pane_id}}.txt"

def ensure_pane(pane_id: int):
    if not any(p["pane_id"] == pane_id for p in panes):
        print(f"pane {{pane_id}} not found", file=sys.stderr)
        sys.exit(1)

if command == "list":
    if args[2:] != ["--format", "json"]:
        print(f"unsupported list args: {{args[2:]}}", file=sys.stderr)
        sys.exit(2)
    print(json.dumps(panes))
    sys.exit(0)

if command == "get-text":
    pane_id = None
    i = 2
    while i < len(args):
        if args[i] == "--pane-id":
            pane_id = int(args[i + 1])
            i += 2
            continue
        if args[i] == "--escapes":
            i += 1
            continue
        print(f"unsupported get-text args: {{args[i:]}}", file=sys.stderr)
        sys.exit(2)
    if pane_id is None:
        print("missing --pane-id", file=sys.stderr)
        sys.exit(2)
    ensure_pane(pane_id)
    sys.stdout.write(pane_path(pane_id).read_text() if pane_path(pane_id).exists() else "")
    sys.exit(0)

if command == "send-text":
    pane_id = None
    no_newline = False
    i = 2
    while i < len(args):
        if args[i] == "--pane-id":
            pane_id = int(args[i + 1])
            i += 2
            continue
        if args[i] == "--no-newline":
            no_newline = True
            i += 1
            continue
        if args[i] == "--no-paste":
            i += 1
            continue
        if args[i] == "--":
            break
        print(f"unsupported send-text args: {{args[i:]}}", file=sys.stderr)
        sys.exit(2)
    if pane_id is None:
        print("missing --pane-id", file=sys.stderr)
        sys.exit(2)
    ensure_pane(pane_id)
    payload = args[i + 1] if i + 1 < len(args) else ""
    suffix = "" if no_newline else "\n"
    target = pane_path(pane_id)
    existing = target.read_text() if target.exists() else ""
    target.write_text(existing + payload + suffix)
    sys.exit(0)

print(f"unsupported command: {{command}}", file=sys.stderr)
sys.exit(2)
"#
    )
}

fn default_mcp_test_config() -> Config {
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    config
}

fn mcp_policy_test_config(
    rule_id: &str,
    action: &str,
    pane_id: Option<u64>,
    decision: PolicyRuleDecision,
    message: &str,
) -> Config {
    let mut config = default_mcp_test_config();
    let mut match_on = PolicyRuleMatch {
        actions: vec![action.to_string()],
        actors: vec!["mcp".to_string()],
        surfaces: vec!["mux".to_string()],
        ..PolicyRuleMatch::default()
    };
    if let Some(pane_id) = pane_id {
        match_on.pane_ids.push(pane_id);
    }
    config.safety.rules = PolicyRulesConfig {
        enabled: true,
        rules: vec![PolicyRule {
            id: rule_id.to_string(),
            description: Some("ft-hp70k incident drill".to_string()),
            priority: 0,
            match_on,
            decision,
            message: Some(message.to_string()),
        }],
    };
    config
}

fn spawn_client_with_config(config: Config, db_path: Option<PathBuf>) -> FrameworkTestClient {
    let server = build_server_with_db(&config, db_path).expect("build MCP server");
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

fn runtime() -> frankenterm_core::runtime_async::Runtime {
    RuntimeBuilder::current_thread()
        .build()
        .expect("build runtime")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |dur| i64::try_from(dur.as_millis()).unwrap_or(i64::MAX))
}

fn pane(pane_id: u64, ts: i64) -> PaneRecord {
    PaneRecord {
        pane_id,
        pane_uuid: None,
        domain: "local".to_string(),
        window_id: None,
        tab_id: None,
        title: Some("search-pane".to_string()),
        cwd: Some("file:///tmp/ft-search".to_string()),
        tty_name: None,
        first_seen_at: ts,
        last_seen_at: ts,
        observed: true,
        ignore_reason: None,
        last_decision_at: None,
    }
}

fn seed_search_db(db_path: &Path) {
    let db_path_string = db_path.to_string_lossy().to_string();
    runtime().block_on(async move {
        let storage = StorageHandle::new(&db_path_string)
            .await
            .expect("open storage");
        storage
            .upsert_pane(pane(1, now_ms()))
            .await
            .expect("upsert pane");
        storage
            .upsert_pane(pane(INCIDENT_SEARCH_PANE_ID, now_ms()))
            .await
            .expect("upsert incident search pane");
        storage
            .append_segment(1, "conformance needle stable", None)
            .await
            .expect("append search segment");
        storage
            .append_segment(
                INCIDENT_SEARCH_PANE_ID,
                &format!("incident search OPENAI_API_KEY={INCIDENT_SECRET} stable"),
                None,
            )
            .await
            .expect("append incident search segment");
        storage
            .rebuild_fts(FtsSyncConfig::default())
            .await
            .expect("rebuild incident search FTS fixture");
        let seeded_segments = storage
            .get_segments(INCIDENT_SEARCH_PANE_ID, 10)
            .await
            .expect("read incident search fixture segments");
        let seeded_hits = storage
            .search_with_results(
                "incident",
                SearchOptions {
                    pane_id: Some(INCIDENT_SEARCH_PANE_ID),
                    include_snippets: Some(false),
                    include_highlights: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("query incident search FTS fixture");
        assert!(
            !seeded_hits.is_empty(),
            "incident search fixture must be indexed before MCP server starts; segments={seeded_segments:?}"
        );
        storage.shutdown().await.expect("shutdown storage");
    });
}

fn policy_denied_audit_rows(db_path: &Path, tool_name: &str) -> Vec<PolicyDeniedAuditRow> {
    let conn = rusqlite::Connection::open(db_path).expect("open db for policy audit rows");
    let mut stmt = conn
        .prepare(
            "SELECT id, tool_name, decision, reason_code, reason, rule_id \
             FROM policy_denied_audit \
             WHERE tool_name = ?1 \
             ORDER BY id",
        )
        .expect("prepare policy_denied_audit query");
    stmt.query_map([tool_name], |row| {
        Ok(PolicyDeniedAuditRow {
            id: row.get(0)?,
            tool_name: row.get(1)?,
            decision: row.get(2)?,
            reason_code: row.get(3)?,
            reason: row.get(4)?,
            rule_id: row.get(5)?,
        })
    })
    .expect("query policy_denied_audit rows")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect policy_denied_audit rows")
}

fn log_incident_drill_case(case: Value) {
    eprintln!(
        "{}",
        serde_json::to_string(&json!({
            "schema": "ft.hp70k.incident_drill.v1",
            "case": case,
        }))
        .expect("serialize incident drill log")
    );
}

fn assert_response_does_not_leak_secret(case_id: &str, envelope: &Value) {
    let rendered = serde_json::to_string(envelope).expect("serialize envelope for leak check");
    assert!(
        !rendered.contains(INCIDENT_SECRET),
        "{case_id} leaked raw secret in response: {rendered}"
    );
}

fn assert_contains_redaction_marker(case_id: &str, text: &str) {
    assert!(
        text.contains("[REDACTED"),
        "{case_id} should include a redaction marker, got {text:?}"
    );
}

fn assert_mcp_policy_error(envelope: &Value, expected_error: &str) {
    assert_common_envelope_fields(envelope, false);
    assert!(envelope.get("data").is_none());
    assert_eq!(envelope["error_code"], "FT-MCP-0006");
    assert_eq!(envelope["error"], expected_error);
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

fn parse_toon_value(text: &str) -> Value {
    let decoded = toon_rust::try_decode(text, None).expect("decode TOON payload");
    let json_text = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    serde_json::from_str(&json_text).expect("TOON payload should stringify back to JSON")
}

fn parse_tool_envelope_with_format(contents: &[FrameworkContent], format: &str) -> Value {
    let text = first_text_content(contents);
    if format == "toon" {
        parse_toon_value(text)
    } else {
        serde_json::from_str(text).expect("parse JSON envelope")
    }
}

fn parse_invalid_args_response(result: Result<Vec<FrameworkContent>, FrameworkMcpError>) -> Value {
    match result {
        Ok(contents) => json!({
            "kind": "tool_envelope",
            "payload": parse_tool_envelope(&contents),
        }),
        Err(err) => json!({
            "kind": "framework_error",
            "code": format!("{:?}", err.code),
            "message": err.message,
            "data": err.data,
        }),
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

fn assert_framework_invalid_params_response(response: &Value, message_substring: &str) {
    assert_eq!(response["kind"], "framework_error");
    assert_eq!(response["code"], "InvalidParams");
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains(message_substring))
    );
    assert!(response["data"].is_null());
}

/// Fences the TOOL-level invalid-args envelope contract for handlers that still
/// emit `FT-MCP-0001` from *inside* their own logic after the framework has
/// already accepted the request shape.
///
/// For `wa.search`, `wa.get_text`, `wa.send`, and `wa.wait_for`, the original
/// top-level `serde_json::from_value` guards were removed after repeated live
/// conformance probes showed that FastMCP's schema validator rejects the tested
/// wrong-type / out-of-bounds payloads before handler execution. Those tools now
/// surface framework `InvalidParams` on public bad-input paths, and a deserialize
/// mismatch at handler entry is treated as an internal schema/handler drift bug.
///
/// The helper remains valuable because other tools can still emit a tool-level
/// `FT-MCP-0001` envelope from intra-handler validation paths, and we want a
/// single assertion that pins that envelope shape.
fn assert_tool_invalid_args_envelope_shape(response: &Value, expected_hint_substring: &str) {
    assert_eq!(
        response["kind"], "tool_envelope",
        "expected tool-level envelope (FT-MCP-0001 path) but got framework_error: {response}"
    );
    let payload = response
        .get("payload")
        .and_then(Value::as_object)
        .expect("tool_envelope must carry a payload object");
    assert_eq!(payload.get("ok"), Some(&Value::Bool(false)));
    assert_eq!(
        payload.get("error_code"),
        Some(&Value::String("FT-MCP-0001".to_string())),
        "tool-level invalid-args envelope must pin error_code to FT-MCP-0001 \
         (see MCP_ERR_INVALID_ARGS at crates/frankenterm-core/src/mcp_error.rs:7)"
    );
    assert!(
        payload.get("error").and_then(Value::as_str).is_some(),
        "tool-level invalid-args envelope must carry a human-readable `error` message"
    );
    let hint = payload
        .get("hint")
        .and_then(Value::as_str)
        .expect("tool-level invalid-args envelope must carry a `hint` string");
    assert!(
        hint.contains(expected_hint_substring),
        "hint `{hint}` does not contain expected substring `{expected_hint_substring}`"
    );
    assert_eq!(
        payload.get("mcp_version"),
        Some(&Value::String("v1".into()))
    );
    assert!(payload.get("elapsed_ms").is_some_and(Value::is_number));
}

fn assert_json_number_field(data: &Map<String, Value>, field: &str, expected: f64) {
    let actual = data
        .get(field)
        .and_then(Value::as_f64)
        .unwrap_or_else(|| panic!("{field} should be numeric: {data:?}"));
    assert!(
        (actual - expected).abs() <= f64::EPSILON,
        "{field} should equal {expected} regardless of JSON integer/float encoding"
    );
}

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        pretty_canonical(actual_schema),
        pretty_canonical(&expected_schema),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

fn assert_search_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("search data object");
    assert_eq!(
        data.get("query"),
        Some(&Value::String("needle".to_string()))
    );
    assert_json_number_field(data, "pane_filter", 1.0);
    assert_json_number_field(data, "since_filter", 0.0);
    assert_json_number_field(data, "until_filter", 4_102_444_800_000.0);
    assert_eq!(
        data.get("mode"),
        Some(&Value::String("lexical".to_string()))
    );
    let metrics = data
        .get("metrics")
        .and_then(Value::as_object)
        .expect("search metrics object");
    assert_eq!(
        metrics.get("requested_mode"),
        Some(&Value::String("hybrid".to_string()))
    );
    assert_eq!(
        metrics.get("effective_mode"),
        Some(&Value::String("lexical".to_string()))
    );
    assert!(metrics.get("fallback_reason").is_some());
    assert!(metrics.get("semantic_latency_ms").is_some());
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .expect("search results array");
    assert_eq!(results.len(), 1, "expected one deterministic search hit");
    let hit = results.first().expect("search hit");
    assert_eq!(hit["pane_id"].as_f64(), Some(1.0));
    assert!(hit["segment_id"].is_number());
    assert!(hit["seq"].is_number());
    assert!(hit["captured_at"].is_number());
    assert!(hit["score"].is_number());
    assert_eq!(
        hit["content"],
        Value::String("conformance needle stable".to_string())
    );
}

fn assert_get_text_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("get_text data object");
    assert_json_number_field(data, "pane_id", 4_242.0);
    assert_json_number_field(data, "tail_lines", 2.0);
    assert_eq!(data.get("escapes_included"), Some(&Value::Bool(false)));
    assert_eq!(
        data.get("text"),
        Some(&Value::String("gamma\ndelta".to_string()))
    );
    assert_eq!(data.get("truncated"), Some(&Value::Bool(true)));
    assert!(
        data.get("truncation_info")
            .and_then(Value::as_object)
            .is_some_and(
                |info| info.contains_key("original_bytes") && info.contains_key("returned_lines")
            )
    );
}

fn assert_send_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("send data object");
    assert_json_number_field(data, "pane_id", 5_252.0);
    assert_eq!(data.get("dry_run"), Some(&Value::Bool(true)));
    let injection = data
        .get("injection")
        .and_then(Value::as_object)
        .expect("send injection object");
    assert!(injection.get("status").is_some_and(Value::is_string));
    assert_eq!(
        injection.get("summary"),
        Some(&Value::String("conformance-ok".to_string()))
    );
    assert!(data.get("wait_for").is_none());
    assert!(data.get("verification_error").is_none());
}

#[test]
fn mcp_policy_redaction_incident_drill_matrix_covers_core_read_surfaces() {
    let mut harness = TestHarness::new();

    let get_text_args = json!({
        "pane_id": INCIDENT_GET_TEXT_PANE_ID,
        "tail": 10,
        "format": "json"
    });
    let get_text_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool("wa.get_text", get_text_args.clone())
            .expect("call wa.get_text incident redaction case"),
    );
    assert_success_envelope_shape(&get_text_envelope);
    assert_response_does_not_leak_secret("mcp.wa_get_text.redaction", &get_text_envelope);
    let get_text = get_text_envelope["data"]["text"]
        .as_str()
        .expect("wa.get_text response text");
    assert_contains_redaction_marker("mcp.wa_get_text.redaction", get_text);
    log_incident_drill_case(json!({
        "id": "mcp.wa_get_text.redaction",
        "tool": "wa.get_text",
        "input": get_text_args,
        "redaction_tier": "secret-redactor",
        "policy_decision": "allow",
        "audit_row_id": Value::Null,
        "normalized_response": canonical_value(&get_text_envelope),
    }));

    let search_args = json!({
        "query": "incident",
        "limit": 5,
        "pane": INCIDENT_SEARCH_PANE_ID,
        "since": 0,
        "until": 4_102_444_800_000_i64,
        "snippets": false,
        "mode": "lexical",
        "format": "json"
    });
    let search_envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool("wa.search", search_args.clone())
            .expect("call wa.search incident redaction case"),
    );
    assert_success_envelope_shape(&search_envelope);
    assert_response_does_not_leak_secret("mcp.wa_search.redaction", &search_envelope);
    let hits = search_envelope["data"]["results"]
        .as_array()
        .expect("wa.search incident results array");
    assert!(
        !hits.is_empty(),
        "wa.search incident query should return at least one hit: {search_envelope}"
    );
    let rendered_hits = serde_json::to_string(hits).expect("serialize incident search hits");
    assert_contains_redaction_marker("mcp.wa_search.redaction", &rendered_hits);
    log_incident_drill_case(json!({
        "id": "mcp.wa_search.redaction",
        "tool": "wa.search",
        "input": search_args,
        "redaction_tier": "secret-redactor",
        "policy_decision": "allow",
        "audit_row_id": Value::Null,
        "normalized_response": canonical_value(&search_envelope),
    }));
}

#[test]
fn mcp_policy_redaction_incident_drill_matrix_records_typed_policy_audits() {
    let deny_message = "ft-hp70k denied get-text drill";
    let mut deny_harness = TestHarness::new_with_config(mcp_policy_test_config(
        "ft_hp70k_deny_get_text",
        "read_output",
        Some(INCIDENT_GET_TEXT_PANE_ID),
        PolicyRuleDecision::Deny,
        deny_message,
    ));
    let deny_args = json!({
        "pane_id": INCIDENT_GET_TEXT_PANE_ID,
        "tail": 10,
        "format": "json"
    });
    let deny_envelope = parse_tool_envelope(
        &deny_harness
            .client
            .call_tool("wa.get_text", deny_args.clone())
            .expect("call wa.get_text denied incident case"),
    );
    assert_mcp_policy_error(&deny_envelope, deny_message);
    assert_response_does_not_leak_secret("mcp.wa_get_text.policy_denied", &deny_envelope);
    let deny_rows = policy_denied_audit_rows(&deny_harness.db_path, "wa.get_text");
    assert_eq!(
        deny_rows.len(),
        1,
        "wa.get_text denial should persist one typed policy audit row"
    );
    let deny_row = deny_rows.first().expect("wa.get_text denial row");
    assert_eq!(deny_row.tool_name, "wa.get_text");
    assert_eq!(deny_row.decision, "denied");
    assert_eq!(deny_row.reason_code, "policy_denied");
    assert_eq!(deny_row.reason, deny_message);
    assert_eq!(
        deny_row.rule_id.as_deref(),
        Some("config.rule.ft_hp70k_deny_get_text")
    );
    log_incident_drill_case(json!({
        "id": "mcp.wa_get_text.policy_denied",
        "tool": "wa.get_text",
        "input": deny_args,
        "redaction_tier": "not-applicable-policy-denied-before-read",
        "policy_decision": "denied",
        "audit_row_id": deny_row.id,
        "normalized_response": canonical_value(&deny_envelope),
    }));
    drop(deny_harness);

    let require_message = "ft-hp70k require approval search drill";
    let mut approval_harness = TestHarness::new_with_config(mcp_policy_test_config(
        "ft_hp70k_require_search",
        "search_output",
        Some(INCIDENT_SEARCH_PANE_ID),
        PolicyRuleDecision::RequireApproval,
        require_message,
    ));
    let approval_args = json!({
        "query": "incident",
        "limit": 5,
        "pane": INCIDENT_SEARCH_PANE_ID,
        "snippets": false,
        "mode": "lexical",
        "format": "json"
    });
    let approval_envelope = parse_tool_envelope(
        &approval_harness
            .client
            .call_tool("wa.search", approval_args.clone())
            .expect("call wa.search require-approval incident case"),
    );
    assert_mcp_policy_error(&approval_envelope, require_message);
    assert_response_does_not_leak_secret("mcp.wa_search.require_approval", &approval_envelope);
    assert!(
        approval_envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("approve")),
        "require-approval envelope should include an approval hint: {approval_envelope}"
    );
    let approval_rows = policy_denied_audit_rows(&approval_harness.db_path, "wa.search");
    assert_eq!(
        approval_rows.len(),
        1,
        "wa.search require-approval should persist one typed policy audit row"
    );
    let approval_row = approval_rows.first().expect("wa.search approval row");
    assert_eq!(approval_row.tool_name, "wa.search");
    assert_eq!(approval_row.decision, "require_approval");
    assert_eq!(approval_row.reason_code, "require_approval");
    assert_eq!(approval_row.reason, require_message);
    assert_eq!(
        approval_row.rule_id.as_deref(),
        Some("config.rule.ft_hp70k_require_search")
    );
    log_incident_drill_case(json!({
        "id": "mcp.wa_search.require_approval",
        "tool": "wa.search",
        "input": approval_args,
        "redaction_tier": "not-applicable-policy-gated-before-search",
        "policy_decision": "require_approval",
        "audit_row_id": approval_row.id,
        "normalized_response": canonical_value(&approval_envelope),
    }));
}

fn assert_wait_for_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("wait_for data object");
    assert_json_number_field(data, "pane_id", 6_262.0);
    assert_eq!(
        data.get("pattern"),
        Some(&Value::String("ready$".to_string()))
    );
    assert_eq!(data.get("matched"), Some(&Value::Bool(true)));
    assert_eq!(data.get("is_regex"), Some(&Value::Bool(true)));
    assert!(
        data.get("polls")
            .and_then(Value::as_f64)
            .is_some_and(|polls| polls >= 1.0)
    );
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" | "captured_at" => *child = Value::from(0_u64),
                    // `polls` is an observed count that varies with scheduler timing under
                    // load (the fake-wezterm subprocess can be slow to come up, forcing a
                    // second poll). The semantic invariant `polls >= 1` is asserted in
                    // assert_wait_for_success_data; the golden only freezes structural
                    // presence, so canonicalize to 0 here.
                    "polls" => *child = Value::from(0_u64),
                    "score" | "semantic_score" if child.is_number() => {
                        *child = Value::from(0.0_f64);
                    }
                    // `lexical_weight` / `semantic_weight` come from
                    // effective_search_fusion_weights at crates/frankenterm-core/src/mcp.rs
                    // via an f32→f64 widening that surfaces bits like 0.30000001192092896.
                    // The operator-visible contract is the 30/70 split, not those bits.
                    // Round to 2 decimals so recompiles with a different f32 intermediate
                    // produce byte-identical goldens; the value in the JSON still reads
                    // like the intended weight (0.3 / 0.7) rather than ryu noise.
                    "lexical_weight" | "semantic_weight" if child.is_number() => {
                        if let Some(f) = child.as_f64() {
                            let rounded = (f * 100.0).round() / 100.0;
                            if let Some(n) = serde_json::Number::from_f64(rounded) {
                                *child = Value::Number(n);
                            }
                        }
                    }
                    _ if key.ends_with("_ms") => *child = Value::from(0_i64),
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
        Value::Number(number) => {
            if number.as_i64().is_none()
                && let Some(float) = number.as_f64()
                && float.is_finite()
                && float.fract() == 0.0
                && float >= i64::MIN as f64
                && float <= i64::MAX as f64
            {
                *value = Value::from(float as i64);
            }
        }
        _ => {}
    }
}

fn pretty_canonical(value: &Value) -> String {
    let mut cloned = value.clone();
    canonicalize(&mut cloned);
    format!(
        "{}\n",
        serde_json::to_string_pretty(&cloned).expect("serialize canonical JSON")
    )
}

fn canonical_value(value: &Value) -> Value {
    let mut cloned = value.clone();
    canonicalize(&mut cloned);
    cloned
}

fn assert_toon_success_matches_json(tool_name: &str, json_envelope: &Value, toon_envelope: &Value) {
    assert_success_envelope_shape(toon_envelope);
    assert_eq!(
        canonical_value(json_envelope),
        canonical_value(toon_envelope),
        "{tool_name} TOON envelope drifted from JSON success semantics"
    );
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden_robot_envelope")
        .join(format!("{name}.json"))
}

fn read_or_update_golden(path: &Path, actual: &str) -> String {
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create golden dir");
        }
        fs::write(path, actual).expect("write golden");
        return actual.to_string();
    }

    fs::read_to_string(path).unwrap_or_else(|err| {
        panic!(
            "missing MCP conformance golden at {}: {err}. Regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test mcp_conformance_core_tools \
             --features mcp,asupersync-runtime",
            path.display()
        )
    })
}

fn assert_matches_golden(name: &str, capture: &ToolGoldenCapture) {
    let actual_value = serde_json::to_value(capture).expect("serialize capture");
    let actual_text = pretty_canonical(&actual_value);
    let path = golden_path(name);
    let expected = read_or_update_golden(&path, &actual_text);

    if expected.trim_end_matches('\n') != actual_text.trim_end_matches('\n') {
        let actual_path = path.with_extension("actual.json");
        let _ = fs::write(&actual_path, &actual_text);
        panic!(
            "MCP core-tool golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test mcp_conformance_core_tools \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
}

/// Synthetic regression fence for `assert_tool_invalid_args_envelope_shape`.
///
/// FastMCP currently rejects the public bad-input probes for the four core tools
/// before handler execution, so this stays synthetic on purpose: build the shape
/// that an intra-handler `FT-MCP-0001` envelope would have, assert the helper
/// accepts it, and assert the helper still rejects the framework-error shape.
/// This keeps the assertion itself from silently drifting even though the former
/// top-level serde guards were removed from those handlers.
#[test]
fn assert_tool_invalid_args_envelope_shape_pins_ft_mcp_0001_contract() {
    // Good: the exact shape `parse_invalid_args_response` would produce if it
    // received a tool envelope for the FT-MCP-0001 serde-reject path.
    let good = json!({
        "kind": "tool_envelope",
        "payload": {
            "ok": false,
            "error": "Invalid params: missing field `query` at line 1 column 2",
            "error_code": "FT-MCP-0001",
            "hint": "Expected object with query (required), limit, pane, since, until, snippets, mode",
            "elapsed_ms": 0,
            "now": 0,
            "mcp_version": "v1",
            "version": "0.1.0",
        }
    });
    // Expect no panic — the hint substring is one of the concrete hint literals
    // emitted by mcp_tools.rs for wa.search at :1394.
    assert_tool_invalid_args_envelope_shape(
        &good,
        "Expected object with query (required), limit, pane, since, until, snippets, mode",
    );

    // Bad: framework_error shape must NOT satisfy the tool-level assertion.
    let framework_shape = json!({
        "kind": "framework_error",
        "code": "InvalidParams",
        "message": "root.query: required",
        "data": null,
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_tool_invalid_args_envelope_shape(&framework_shape, "anything");
    }));
    assert!(
        result.is_err(),
        "assert_tool_invalid_args_envelope_shape must reject framework_error shape"
    );

    // Bad: a tool envelope with wrong error_code must NOT satisfy the assertion.
    let wrong_code = json!({
        "kind": "tool_envelope",
        "payload": {
            "ok": false,
            "error": "something else",
            "error_code": "FT-MCP-9999",
            "hint": "any hint",
            "elapsed_ms": 0,
            "now": 0,
            "mcp_version": "v1",
            "version": "0.1.0",
        }
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_tool_invalid_args_envelope_shape(&wrong_code, "any hint");
    }));
    assert!(
        result.is_err(),
        "assert_tool_invalid_args_envelope_shape must reject non-FT-MCP-0001 error_code"
    );
}

#[test]
fn mcp_conformance_wa_search_contract_matches_golden() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.search".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.search"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.search",
                    json!({
                        "query": "needle",
                        "limit": 5,
                        "pane": 1,
                        "since": 0,
                        "until": 4_102_444_800_000_i64,
                        "snippets": false,
                        "mode": "hybrid",
                        "format": "json"
                    }),
                )
                .expect("call wa.search"),
        ),
        invalid_args_response: parse_invalid_args_response(harness.client.call_tool(
            "wa.search",
            json!({
                "query": "needle",
                "limit": 0,
                "format": "json"
            }),
        )),
    };

    assert_schema_matches_manifest("wa.search", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_search_success_data(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "root.limit: value must be >= 1",
    );
    let toon_envelope = parse_tool_envelope_with_format(
        &harness
            .client
            .call_tool(
                "wa.search",
                json!({
                    "query": "needle",
                    "limit": 5,
                    "pane": 1,
                    "since": 0,
                    "until": 4_102_444_800_000_i64,
                    "snippets": false,
                    "mode": "hybrid",
                    "format": "toon"
                }),
            )
            .expect("call wa.search toon"),
        "toon",
    );
    assert_search_success_data(&toon_envelope);
    assert_toon_success_matches_json("wa.search", &capture.success_envelope, &toon_envelope);
    assert_matches_golden("wa_search", &capture);
}

#[test]
fn mcp_conformance_wa_get_text_contract_matches_golden() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.get_text".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.get_text"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.get_text",
                    json!({
                        "pane_id": 4242,
                        "tail": 2,
                        "format": "json"
                    }),
                )
                .expect("call wa.get_text"),
        ),
        invalid_args_response: parse_invalid_args_response(harness.client.call_tool(
            "wa.get_text",
            json!({
                "pane_id": 4242,
                "tail": 0,
                "format": "json"
            }),
        )),
    };

    assert_schema_matches_manifest("wa.get_text", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_get_text_success_data(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "root.tail: value must be >= 1",
    );
    let toon_envelope = parse_tool_envelope_with_format(
        &harness
            .client
            .call_tool(
                "wa.get_text",
                json!({
                    "pane_id": 4242,
                    "tail": 2,
                    "format": "toon"
                }),
            )
            .expect("call wa.get_text toon"),
        "toon",
    );
    assert_get_text_success_data(&toon_envelope);
    assert_toon_success_matches_json("wa.get_text", &capture.success_envelope, &toon_envelope);
    assert_matches_golden("wa_get_text", &capture);
}

#[test]
// Enforces MCP-V1-003 (side-effect tools route via PolicyEngine) — see
// docs/mcp-api-spec-coverage.md. wa.send is the canonical side-effect tool.
fn mcp_conformance_wa_send_contract_matches_golden() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.send".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.send"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.send",
                    json!({
                        "pane_id": 5252,
                        "text": "conformance-ok",
                        "dry_run": true,
                        "format": "json"
                    }),
                )
                .expect("call wa.send"),
        ),
        invalid_args_response: parse_invalid_args_response(harness.client.call_tool(
            "wa.send",
            json!({
                "pane_id": 5252,
                "text": "echo nope",
                "timeout_secs": 0,
                "format": "json"
            }),
        )),
    };

    assert_schema_matches_manifest("wa.send", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_send_success_data(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "root.timeout_secs: value must be >= 1",
    );
    let toon_envelope = parse_tool_envelope_with_format(
        &harness
            .client
            .call_tool(
                "wa.send",
                json!({
                    "pane_id": 5252,
                    "text": "conformance-ok",
                    "dry_run": true,
                    "format": "toon"
                }),
            )
            .expect("call wa.send toon"),
        "toon",
    );
    assert_send_success_data(&toon_envelope);
    assert_toon_success_matches_json("wa.send", &capture.success_envelope, &toon_envelope);
    assert_matches_golden("wa_send", &capture);
}

#[test]
fn mcp_conformance_wa_wait_for_contract_matches_golden() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.wait_for".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.wait_for"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.wait_for",
                    json!({
                        "pane_id": 6262,
                        "pattern": "ready$",
                        "timeout_secs": 1,
                        "regex": true,
                        "format": "json"
                    }),
                )
                .expect("call wa.wait_for"),
        ),
        invalid_args_response: parse_invalid_args_response(harness.client.call_tool(
            "wa.wait_for",
            json!({
                "pane_id": 6262,
                "pattern": "ready$",
                "timeout_secs": 0,
                "format": "json"
            }),
        )),
    };

    assert_schema_matches_manifest("wa.wait_for", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_wait_for_success_data(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "root.timeout_secs: value must be >= 1",
    );
    let toon_envelope = parse_tool_envelope_with_format(
        &harness
            .client
            .call_tool(
                "wa.wait_for",
                json!({
                    "pane_id": 6262,
                    "pattern": "ready$",
                    "timeout_secs": 1,
                    "regex": true,
                    "format": "toon"
                }),
            )
            .expect("call wa.wait_for toon"),
        "toon",
    );
    assert_wait_for_success_data(&toon_envelope);
    assert_toon_success_matches_json("wa.wait_for", &capture.success_envelope, &toon_envelope);
    assert_matches_golden("wa_wait_for", &capture);
}

#[test]
fn mcp_conformance_core_tools_invalid_format_returns_documented_envelope() {
    let mut harness = TestHarness::new();
    let cases = [
        (
            "wa.search",
            json!({
                "query": "needle",
                "format": "yaml"
            }),
        ),
        (
            "wa.get_text",
            json!({
                "pane_id": 4242,
                "format": "yaml"
            }),
        ),
        (
            "wa.send",
            json!({
                "pane_id": 5252,
                "text": "conformance-ok",
                "format": "yaml"
            }),
        ),
        (
            "wa.wait_for",
            json!({
                "pane_id": 6262,
                "pattern": "ready$",
                "format": "yaml"
            }),
        ),
    ];

    for (tool_name, args) in cases {
        let response = parse_invalid_args_response(harness.client.call_tool(tool_name, args));
        assert_framework_invalid_params_response(&response, "root.format: value must be one of");
    }
}
