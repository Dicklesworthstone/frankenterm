#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkMcpError, FrameworkTestClient, FrameworkTool,
    framework_create_memory_transport_pair,
};
use frankenterm_core::runtime_compat::{CompatRuntime, RuntimeBuilder};
use frankenterm_core::storage::{PaneRecord, StorageHandle};
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
    _env_lock: MutexGuard<'static, ()>,
    _override_guard: WeztermCliOverrideGuard,
    _fake_wezterm: FakeWezterm,
    _workspace: TempDir,
    client: FrameworkTestClient,
}

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

        let cli_path = workspace.path().join("fake-wezterm.py");
        fs::write(&cli_path, fake_wezterm_script(&state_dir)).expect("write fake wezterm cli");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&cli_path).expect("fake cli metadata").permissions();
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
        let env_lock = wezterm_env_lock();
        let fake_wezterm = FakeWezterm::new();
        let override_guard = fake_wezterm.install();
        let workspace = tempfile::tempdir().expect("create conformance workspace");
        let db_path = workspace.path().join("mcp.sqlite3");
        seed_search_db(&db_path);
        let client = spawn_client(Some(db_path));
        Self {
            _env_lock: env_lock,
            _override_guard: override_guard,
            _fake_wezterm: fake_wezterm,
            _workspace: workspace,
            client,
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
        }
    ])
}

fn fake_wezterm_script(state_dir: &Path) -> String {
    format!(
        r#"#!/usr/bin/env python3
import json
import sys
from pathlib import Path

state_dir = Path({state_dir:?})
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

fn runtime() -> frankenterm_core::runtime_compat::Runtime {
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
            .append_segment(1, "conformance needle stable", None)
            .await
            .expect("append search segment");
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

fn parse_invalid_args_response(
    result: Result<Vec<FrameworkContent>, FrameworkMcpError>,
) -> Value {
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
    assert_eq!(payload.get("mcp_version"), Some(&Value::String("v1".into())));
    assert!(payload.get("elapsed_ms").is_some_and(Value::is_number));
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
    assert_eq!(data.get("query"), Some(&Value::String("needle".to_string())));
    assert_eq!(data.get("pane_filter"), Some(&Value::from(1_u64)));
    assert_eq!(data.get("since_filter"), Some(&Value::from(0_i64)));
    assert_eq!(
        data.get("until_filter"),
        Some(&Value::from(4_102_444_800_000_i64))
    );
    assert_eq!(data.get("mode"), Some(&Value::String("lexical".to_string())));
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
    assert_eq!(hit["pane_id"], Value::from(1_u64));
    assert!(hit["segment_id"].is_number());
    assert!(hit["seq"].is_number());
    assert!(hit["captured_at"].is_number());
    assert!(hit["score"].is_number());
    assert_eq!(hit["content"], Value::String("conformance needle stable".to_string()));
}

fn assert_get_text_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("get_text data object");
    assert_eq!(data.get("pane_id"), Some(&Value::from(4_242_u64)));
    assert_eq!(data.get("tail_lines"), Some(&Value::from(2_u64)));
    assert_eq!(data.get("escapes_included"), Some(&Value::Bool(false)));
    assert_eq!(data.get("text"), Some(&Value::String("gamma\ndelta".to_string())));
    assert_eq!(data.get("truncated"), Some(&Value::Bool(true)));
    assert!(data
        .get("truncation_info")
        .and_then(Value::as_object)
        .is_some_and(|info| info.contains_key("original_bytes") && info.contains_key("returned_lines")));
}

fn assert_send_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("send data object");
    assert_eq!(data.get("pane_id"), Some(&Value::from(5_252_u64)));
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

fn assert_wait_for_success_data(envelope: &Value) {
    let data = envelope["data"].as_object().expect("wait_for data object");
    assert_eq!(data.get("pane_id"), Some(&Value::from(6_262_u64)));
    assert_eq!(data.get("pattern"), Some(&Value::String("ready$".to_string())));
    assert_eq!(data.get("matched"), Some(&Value::Bool(true)));
    assert_eq!(data.get("is_regex"), Some(&Value::Bool(true)));
    assert!(data
        .get("polls")
        .and_then(Value::as_u64)
        .is_some_and(|polls| polls >= 1));
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
                        *child = Value::from(0.0_f64)
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
    assert_matches_golden("wa_get_text", &capture);
}

#[test]
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
    assert_matches_golden("wa_wait_for", &capture);
}
