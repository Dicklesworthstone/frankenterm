#![cfg(feature = "mcp")]

use frankenterm_core::accounts::AccountRecord;
use frankenterm_core::cass::set_cass_cli_override;
use frankenterm_core::caut::set_caut_cli_override;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkMcpError, FrameworkTestClient, FrameworkTool,
    framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_async::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle};
use frankenterm_core::wezterm::set_wezterm_cli_override;
use insta::assert_json_snapshot;
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
    _tool_override_guard: TestToolOverrideGuard,
    _workspace: TempDir,
    _tool_override_lock: MutexGuard<'static, ()>,
}

struct TestToolOverrideGuard;

#[derive(Serialize)]
struct ToolGoldenCapture {
    tool: String,
    input_schema: Value,
    success_envelope: Value,
    invalid_args_response: Value,
}

fn tool_override_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Drop for TestToolOverrideGuard {
    fn drop(&mut self) {
        set_wezterm_cli_override(None);
        set_cass_cli_override(None);
        set_caut_cli_override(None);
    }
}

impl TestHarness {
    fn new() -> Self {
        let lock = tool_override_lock();
        let workspace = tempfile::tempdir().expect("create conformance workspace");
        let fake_bin_dir = workspace.path().join("bin");
        fs::create_dir_all(&fake_bin_dir).expect("create fake bin dir");

        let wezterm_path = fake_bin_dir.join("wezterm");
        fs::write(&wezterm_path, fake_wezterm_script()).expect("write fake wezterm");
        make_executable(&wezterm_path);
        set_wezterm_cli_override(Some(wezterm_path.to_string_lossy().into_owned()));

        let cass_path = fake_bin_dir.join("cass");
        fs::write(&cass_path, fake_cass_script()).expect("write fake cass");
        make_executable(&cass_path);
        set_cass_cli_override(Some(cass_path.to_string_lossy().into_owned()));

        let caut_path = fake_bin_dir.join("caut");
        fs::write(&caut_path, fake_caut_script()).expect("write fake caut");
        make_executable(&caut_path);
        set_caut_cli_override(Some(caut_path.to_string_lossy().into_owned()));

        let db_path = workspace.path().join("mcp.sqlite3");
        seed_db(&db_path);
        let client = spawn_client(Some(db_path));
        let tool_override_guard = TestToolOverrideGuard;

        Self {
            client,
            _tool_override_guard: tool_override_guard,
            _workspace: workspace,
            _tool_override_lock: lock,
        }
    }
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(path)
            .expect("metadata for executable")
            .permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod executable");
    }
}

fn fake_wezterm_script() -> &'static str {
    r#"#!/usr/bin/env python3
import json
import sys

PANES = [
    {
        "pane_id": 1,
        "tab_id": 1,
        "window_id": 1,
        "domain_name": "local",
        "title": "workflow-pane",
        "cwd": "file:///tmp/ft-workflow",
    }
]

args = sys.argv[1:]
if len(args) >= 4 and args[0] == "cli" and args[1] == "list" and args[2] == "--format" and args[3] == "json":
    print(json.dumps(PANES))
    sys.exit(0)

print(f"unsupported args: {args}", file=sys.stderr)
sys.exit(2)
"#
}

fn fake_cass_script() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail

cmd="${1:-}"
shift || true

case "$cmd" in
  search)
    cat <<'EOF'
{"query":"agent context","count":1,"hits":[{"source_path":"/tmp/session.md","line_number":42,"agent":"codex","content":"needle hit"}]}
EOF
    ;;
  view)
    cat <<'EOF'
{"source_path":"/tmp/session.md","line_number":42,"match_line":{"line_number":42,"content":"needle hit","role":"assistant"},"context_before":[{"line_number":41,"content":"before","role":"user"}],"context_after":[{"line_number":43,"content":"after","role":"assistant"}]}
EOF
    ;;
  status)
    cat <<'EOF'
{"healthy":true,"index_path":"/tmp/.cass/index","total_sessions":150,"stale":false}
EOF
    ;;
  *)
    echo "unsupported cass stub command: $cmd" >&2
    exit 1
    ;;
esac
"#
}

fn fake_caut_script() -> &'static str {
    r#"#!/usr/bin/env bash
set -euo pipefail

cmd="${1:-}"
shift || true

if [[ "$cmd" != "usage" ]]; then
  echo "unsupported caut command: $cmd" >&2
  exit 1
fi

cat <<'EOF'
{"schemaVersion":"caut.v1","generatedAt":"2026-02-27T21:05:00Z","command":"usage","data":[{"provider":"codex","account":"acc-refresh","usage":{"primary":{"percentRemaining":50.0,"tokensUsed":5000,"tokensRemaining":5000,"tokensLimit":10000,"resetAt":"2026-03-01T00:00:00Z"},"updatedAt":"2026-02-27T21:04:59Z","identity":{"accountEmail":"refresh@example.com","accountOrganization":"Refresh Org"}}}],"errors":[]}
EOF
"#
}

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
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

fn seed_db(db_path: &Path) {
    let db_path_string = db_path.to_string_lossy().to_string();
    runtime().block_on(async move {
        let storage = StorageHandle::new(&db_path_string)
            .await
            .expect("open storage");
        let pane = PaneRecord {
            pane_id: 1,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("workflow-pane".to_string()),
            cwd: Some("file:///tmp/ft-workflow".to_string()),
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.expect("upsert pane");

        let account = AccountRecord {
            id: 0,
            account_id: "acc-primary".to_string(),
            service: "openai".to_string(),
            name: Some("Primary".to_string()),
            percent_remaining: 85.5,
            reset_at: Some("2026-03-01T00:00:00Z".to_string()),
            tokens_used: Some(1_450),
            tokens_remaining: Some(8_550),
            tokens_limit: Some(10_000),
            last_refreshed_at: 1_700_000_000_100,
            last_used_at: Some(1_700_000_000_050),
            created_at: 1_700_000_000_100,
            updated_at: 1_700_000_000_100,
        };
        storage
            .upsert_account(account)
            .await
            .expect("upsert account");
        storage.shutdown().await.expect("shutdown storage");
    });
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

fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" | "last_refreshed_at" => *child = Value::from(0_u64),
                    // execution_id is `format!("mcp-{name}-{now_ms}")` (mcp_tools.rs:3038)
                    // — the now_ms suffix changes every test run. Replace with a stable
                    // placeholder so the snapshot captures the SHAPE of the field
                    // ("non-empty string when execute path runs") without the
                    // timestamp churn.
                    "execution_id" if child.is_string() => {
                        *child = Value::String("[execution_id]".to_string());
                    }
                    _ if key.ends_with("_ms") => *child = Value::from(0_u64),
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

fn canonical_value<T: Serialize>(value: &T) -> Value {
    let mut value = serde_json::to_value(value).expect("serialize value");
    canonicalize(&mut value);
    value
}

fn snapshot_capture(capture: &ToolGoldenCapture) -> Value {
    canonical_value(&json!({
        "tool": capture.tool,
        "ok": capture.success_envelope["ok"].clone(),
        "data": capture.success_envelope["data"].clone(),
    }))
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

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    assert_eq!(
        canonical_value(actual_schema),
        canonical_value(&manifest_tool_schema(tool_name)),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
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

fn assert_accounts_success(envelope: &Value) {
    let data = envelope["data"].as_object().expect("accounts data object");
    assert_eq!(
        data.get("service"),
        Some(&Value::String("openai".to_string()))
    );
    assert_eq!(data.get("total"), Some(&Value::from(1_u64)));
    let accounts = data
        .get("accounts")
        .and_then(Value::as_array)
        .expect("accounts array");
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        accounts[0]["account_id"],
        Value::String("acc-primary".to_string())
    );
    assert_eq!(accounts[0]["percent_remaining"], Value::from(85.5_f64));
}

fn assert_accounts_refresh_success(envelope: &Value) {
    let data = envelope["data"]
        .as_object()
        .expect("accounts_refresh data object");
    assert_eq!(
        data.get("service"),
        Some(&Value::String("openai".to_string()))
    );
    assert_eq!(data.get("refreshed_count"), Some(&Value::from(1_u64)));
    assert_eq!(
        data.get("refreshed_at"),
        Some(&Value::String("2026-02-27T21:05:00Z".to_string()))
    );
    let accounts = data
        .get("accounts")
        .and_then(Value::as_array)
        .expect("accounts array");
    assert_eq!(accounts.len(), 1);
    assert_eq!(
        accounts[0]["account_id"],
        Value::String("acc-refresh".to_string())
    );
}

fn assert_cass_search_success(envelope: &Value) {
    let data = envelope["data"]
        .as_object()
        .expect("cass_search data object");
    assert_eq!(
        data.get("query"),
        Some(&Value::String("agent context".to_string()))
    );
    assert_eq!(data.get("count"), Some(&Value::from(1_u64)));
    let hits = data
        .get("hits")
        .and_then(Value::as_array)
        .expect("hits array");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0]["source_path"],
        Value::String("/tmp/session.md".to_string())
    );
    assert_eq!(hits[0]["line_number"], Value::from(42_u64));
}

fn assert_cass_view_success(envelope: &Value) {
    let data = envelope["data"].as_object().expect("cass_view data object");
    assert_eq!(
        data.get("source_path"),
        Some(&Value::String("/tmp/session.md".to_string()))
    );
    assert_eq!(data.get("line_number"), Some(&Value::from(42_u64)));
    assert_eq!(
        data["match_line"]["content"],
        Value::String("needle hit".to_string())
    );
}

fn assert_cass_status_success(envelope: &Value) {
    let data = envelope["data"]
        .as_object()
        .expect("cass_status data object");
    assert_eq!(data.get("healthy"), Some(&Value::Bool(true)));
    assert_eq!(data.get("total_sessions"), Some(&Value::from(150_u64)));
    assert_eq!(data.get("stale"), Some(&Value::Bool(false)));
}

fn assert_workflow_run_success(envelope: &Value) {
    let data = envelope["data"]
        .as_object()
        .expect("workflow_run data object");
    assert_eq!(
        data.get("workflow_name"),
        Some(&Value::String("handle_usage_limits".to_string()))
    );
    assert_eq!(data.get("pane_id"), Some(&Value::from(1_u64)));
    assert_eq!(
        data.get("status"),
        Some(&Value::String("dry_run".to_string()))
    );
    assert_eq!(
        data.get("message"),
        Some(&Value::String("Dry-run: workflow not executed".to_string()))
    );
}

#[test]
fn mcp_conformance_wa_accounts_matches_snapshot() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.accounts".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.accounts"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool("wa.accounts", json!({ "service": "openai" }))
                .expect("call wa.accounts"),
        ),
        invalid_args_response: parse_invalid_args_response(
            harness.client.call_tool("wa.accounts", json!({})),
        ),
    };

    assert_schema_matches_manifest("wa.accounts", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_accounts_success(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "missing required field: service",
    );

    assert_json_snapshot!("wa_accounts_conformance", snapshot_capture(&capture));
}

#[test]
fn mcp_conformance_wa_accounts_refresh_matches_snapshot() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.accounts_refresh".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.accounts_refresh"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool("wa.accounts_refresh", json!({ "service": "openai" }))
                .expect("call wa.accounts_refresh"),
        ),
        invalid_args_response: parse_invalid_args_response(
            harness
                .client
                .call_tool("wa.accounts_refresh", json!({ "service": 7 })),
        ),
    };

    assert_schema_matches_manifest("wa.accounts_refresh", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_accounts_refresh_success(&capture.success_envelope);
    assert_framework_invalid_params_response(&capture.invalid_args_response, "root.service");

    assert_json_snapshot!(
        "wa_accounts_refresh_conformance",
        snapshot_capture(&capture)
    );
}

#[test]
fn mcp_conformance_wa_cass_search_matches_snapshot() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.cass_search".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.cass_search"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool("wa.cass_search", json!({ "query": "agent context" }))
                .expect("call wa.cass_search"),
        ),
        invalid_args_response: parse_invalid_args_response(
            harness.client.call_tool("wa.cass_search", json!({})),
        ),
    };

    assert_schema_matches_manifest("wa.cass_search", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_cass_search_success(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "missing required field: query",
    );

    assert_json_snapshot!("wa_cass_search_conformance", snapshot_capture(&capture));
}

#[test]
fn mcp_conformance_wa_cass_status_matches_snapshot() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.cass_status".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.cass_status"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool("wa.cass_status", json!({}))
                .expect("call wa.cass_status"),
        ),
        invalid_args_response: parse_invalid_args_response(
            harness
                .client
                .call_tool("wa.cass_status", json!({ "timeout_secs": 0 })),
        ),
    };

    assert_schema_matches_manifest("wa.cass_status", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_cass_status_success(&capture.success_envelope);
    assert_framework_invalid_params_response(&capture.invalid_args_response, "root.timeout_secs");

    assert_json_snapshot!("wa_cass_status_conformance", snapshot_capture(&capture));
}

#[test]
fn mcp_conformance_wa_cass_view_matches_snapshot() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.cass_view".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.cass_view"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.cass_view",
                    json!({ "source_path": "/tmp/session.md", "line_number": 42 }),
                )
                .expect("call wa.cass_view"),
        ),
        invalid_args_response: parse_invalid_args_response(
            harness
                .client
                .call_tool("wa.cass_view", json!({ "source_path": "/tmp/session.md" })),
        ),
    };

    assert_schema_matches_manifest("wa.cass_view", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_cass_view_success(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "missing required field: line_number",
    );

    assert_json_snapshot!("wa_cass_view_conformance", snapshot_capture(&capture));
}

#[test]
fn mcp_conformance_wa_workflow_run_matches_snapshot() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.workflow_run".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.workflow_run"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.workflow_run",
                    json!({
                        "name": "handle_usage_limits",
                        "pane_id": 1,
                        "dry_run": true
                    }),
                )
                .expect("call wa.workflow_run"),
        ),
        invalid_args_response: parse_invalid_args_response(
            harness
                .client
                .call_tool("wa.workflow_run", json!({ "pane_id": 1, "dry_run": true })),
        ),
    };

    assert_schema_matches_manifest("wa.workflow_run", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_workflow_run_success(&capture.success_envelope);
    assert_framework_invalid_params_response(
        &capture.invalid_args_response,
        "missing required field: name",
    );

    assert_json_snapshot!("wa_workflow_run_conformance", snapshot_capture(&capture));
}

/// Build a capture struct for the execute-path snapshots — same shape as
/// `ToolGoldenCapture` minus `invalid_args_response` (already pinned by the
/// dry-run test above) plus a label slot that carries which scenario the
/// envelope came from.
#[derive(Serialize)]
struct WorkflowRunExecuteCapture {
    tool: String,
    scenario: &'static str,
    envelope: Value,
}

fn workflow_execute_snapshot(capture: &WorkflowRunExecuteCapture) -> Value {
    canonical_value(&json!({
        "tool": capture.tool,
        "scenario": capture.scenario,
        "ok": capture.envelope["ok"].clone(),
        "data": capture.envelope.get("data").cloned().unwrap_or(Value::Null),
        "error": capture.envelope.get("error").cloned().unwrap_or(Value::Null),
        "error_code": capture.envelope.get("error_code").cloned().unwrap_or(Value::Null),
        "hint": capture.envelope.get("hint").cloned().unwrap_or(Value::Null),
    }))
}

#[test]
fn mcp_conformance_wa_workflow_run_execute_path_matches_snapshot() {
    // ft-l0am8: 01f690bf only pinned the dry_run path of wa.workflow_run.
    // Real execute path (dry_run=false) goes through find_workflow_by_name +
    // runner.run_workflow, which can return Completed / Aborted /
    // PolicyDenied / Error. With the test harness's default config and a
    // real built-in workflow name on a pane that has no usage-limit
    // anchors, the runner cannot satisfy preconditions and the response
    // status lands on a non-success branch with execution_id populated.
    //
    // Scrub: execution_id (format!("mcp-{name}-{now_ms}"), volatile),
    // elapsed_ms / *_ms timers (canonicalize handles these). The
    // workflow_name and pane_id are deterministic.
    let mut harness = TestHarness::new();
    let envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.workflow_run",
                json!({
                    "name": "handle_usage_limits",
                    "pane_id": 1,
                    "dry_run": false,
                }),
            )
            .expect("call wa.workflow_run execute path"),
    );

    // Whatever the response, it must satisfy the FT-MCP-0001 envelope
    // contract: ok is bool, version is string, mcp_version is "v1".
    assert!(envelope["ok"].is_boolean());
    assert!(envelope["version"].is_string());
    assert_eq!(envelope["mcp_version"], "v1");
    assert!(envelope["elapsed_ms"].is_number());
    assert!(envelope["now"].is_number());

    let capture = WorkflowRunExecuteCapture {
        tool: "wa.workflow_run".to_string(),
        scenario: "execute_real_workflow_no_preconditions",
        envelope,
    };
    assert_json_snapshot!(
        "wa_workflow_run_execute_path",
        workflow_execute_snapshot(&capture)
    );
}

#[test]
fn mcp_conformance_wa_workflow_run_unknown_name_matches_snapshot() {
    // ft-l0am8: post-policy / post-dry_run runner-error branch. Calling
    // wa.workflow_run with a name that doesn't resolve must surface the
    // structured MCP_ERR_WORKFLOW envelope at mcp_tools.rs:3027-3036
    // (`"Workflow 'X' not found"` with the watch-mode hint). This
    // branch is fully deterministic — no execution_id, no timing —
    // so the snapshot pins the operator-visible error contract
    // verbatim.
    let mut harness = TestHarness::new();
    let envelope = parse_tool_envelope(
        &harness
            .client
            .call_tool(
                "wa.workflow_run",
                json!({
                    "name": "this_workflow_does_not_exist_in_any_registry",
                    "pane_id": 1,
                    "dry_run": false,
                }),
            )
            .expect("call wa.workflow_run with unknown workflow name"),
    );

    // The unknown-workflow path returns an error envelope (ok=false).
    assert_eq!(envelope["ok"], Value::Bool(false));
    assert_eq!(envelope["mcp_version"], "v1");
    assert!(envelope["error"].as_str().is_some());

    let capture = WorkflowRunExecuteCapture {
        tool: "wa.workflow_run".to_string(),
        scenario: "execute_unknown_workflow_name",
        envelope,
    };
    assert_json_snapshot!(
        "wa_workflow_run_execute_unknown_name",
        workflow_execute_snapshot(&capture)
    );
}
