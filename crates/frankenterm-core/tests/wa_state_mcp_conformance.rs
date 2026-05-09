#![cfg(feature = "mcp")]

use frankenterm_core::config::Config;
use frankenterm_core::mcp::{build_server_degraded, build_server_with_db};
use frankenterm_core::mcp_framework::{
    FrameworkContent, FrameworkMcpError, FrameworkTestClient, FrameworkTool,
    framework_create_memory_transport_pair,
};
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
        fs::create_dir_all(&state_dir).expect("create fake wezterm state dir");
        fs::write(
            state_dir.join("panes.json"),
            serde_json::to_string_pretty(&fake_panes()).expect("serialize fake panes"),
        )
        .expect("write fake panes");

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
        let env_lock = wezterm_env_lock();
        let fake_wezterm = FakeWezterm::new();
        let override_guard = fake_wezterm.install();
        let client = spawn_client(None);
        Self {
            _env_lock: env_lock,
            _override_guard: override_guard,
            _fake_wezterm: fake_wezterm,
            client,
        }
    }
}

fn fake_panes() -> Value {
    json!([
        {
            "pane_id": 4242,
            "tab_id": 7,
            "window_id": 3,
            "domain_name": "local",
            "title": "codex sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890",
            "cwd": "file:///tmp/ft-state/sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890"
        },
        {
            "pane_id": 9999,
            "tab_id": 8,
            "window_id": 4,
            "domain_name": "local",
            "title": "claude-helper",
            "cwd": "file:///tmp/ft-other"
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

args = sys.argv[1:]
if len(args) < 2 or args[0] != "cli":
    print(f"unsupported args: {{args}}", file=sys.stderr)
    sys.exit(2)

if args[1] != "list":
    print(f"unsupported command: {{args[1]}}", file=sys.stderr)
    sys.exit(2)

if args[2:] != ["--format", "json"]:
    print(f"unsupported list args: {{args[2:]}}", file=sys.stderr)
    sys.exit(2)

print(json.dumps(panes))
"#
    )
}

fn spawn_client(db_path: Option<PathBuf>) -> FrameworkTestClient {
    let mut config = Config::default();
    config.safety.require_prompt_active = false;
    let server = match db_path {
        Some(db_path) => build_server_with_db(&config, Some(db_path)),
        None => build_server_degraded(&config),
    }
    .expect("build MCP server");
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

fn assert_common_envelope_fields(envelope: &Value, ok: bool) {
    assert_eq!(envelope["ok"], ok);
    assert!(envelope["elapsed_ms"].is_number());
    assert!(envelope["now"].is_number());
    assert_eq!(envelope["mcp_version"], "v1");
    assert!(envelope["version"].is_string());
}

fn assert_success_envelope_shape(envelope: &Value) {
    assert_common_envelope_fields(envelope, true);
    assert!(envelope["data"].is_array());
    assert!(envelope.get("error").is_none());
    assert!(envelope.get("error_code").is_none());
    assert!(envelope.get("hint").is_none());
}

fn assert_schema_matches_manifest(tool_name: &str, actual_schema: &Value) {
    let expected_schema = manifest_tool_schema(tool_name);
    assert_eq!(
        pretty_canonical(actual_schema),
        pretty_canonical(&expected_schema),
        "schema drift vs tests/fixtures/mcp_manifest.json for {tool_name}"
    );
}

fn assert_state_success_data(envelope: &Value) {
    let raw_secret = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890";
    let states = envelope["data"].as_array().expect("wa.state data array");
    assert_eq!(states.len(), 1, "expected one filtered pane state");
    let pane = states.first().expect("state entry");
    assert_eq!(pane["pane_id"], Value::from(4242_u64));
    assert_eq!(pane["tab_id"], Value::from(7_u64));
    assert_eq!(pane["window_id"], Value::from(3_u64));
    assert_eq!(pane["domain"], Value::String("local".to_string()));
    let title = pane["title"].as_str().expect("title string");
    let cwd = pane["cwd"].as_str().expect("cwd string");
    assert!(title.contains("codex"));
    assert!(title.contains("[REDACTED]"));
    assert!(!title.contains(raw_secret));
    assert!(cwd.contains("file:///tmp/ft-state/"));
    assert!(cwd.contains("[REDACTED]"));
    assert!(!cwd.contains(raw_secret));
    assert_eq!(pane["observed"], Value::Bool(true));
    assert!(pane.get("ignore_reason").is_none());
    assert!(pane.get("pane_uuid").is_none() || pane["pane_uuid"].is_null());
}

fn assert_framework_invalid_args_response(response: &Value) {
    assert_eq!(response["kind"], "framework_error");
    assert_eq!(response["code"], "InvalidParams");
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains("root.agent"))
    );
    assert!(
        response["message"]
            .as_str()
            .is_some_and(|message| message.contains("expected type string"))
    );
    assert!(response["data"].is_null());
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "now" | "elapsed_ms" => *child = Value::from(0_u64),
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
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_state_mcp_conformance \
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
            "MCP wa.state golden drift detected. Review the diff between:\n  \
             expected: {}\n  actual:   {}\n\n\
             If intentional, regenerate with:\n  \
             UPDATE_GOLDEN=1 cargo test -p frankenterm-core --test wa_state_mcp_conformance \
             --features mcp,asupersync-runtime",
            path.display(),
            actual_path.display()
        );
    }
}

#[test]
fn mcp_conformance_wa_state_contract_matches_golden() {
    let mut harness = TestHarness::new();
    let capture = ToolGoldenCapture {
        tool: "wa.state".to_string(),
        input_schema: tool_input_schema(&mut harness.client, "wa.state"),
        success_envelope: parse_tool_envelope(
            &harness
                .client
                .call_tool(
                    "wa.state",
                    json!({
                        "pane_id": 4242,
                        "agent": "codex",
                        "domain": "local",
                        "format": "json"
                    }),
                )
                .expect("call wa.state"),
        ),
        invalid_args_response: parse_invalid_args_response(harness.client.call_tool(
            "wa.state",
            json!({
                "agent": 7,
                "format": "json"
            }),
        )),
    };

    assert_schema_matches_manifest("wa.state", &capture.input_schema);
    assert_success_envelope_shape(&capture.success_envelope);
    assert_state_success_data(&capture.success_envelope);
    assert_framework_invalid_args_response(&capture.invalid_args_response);
    assert_matches_golden("wa_state", &capture);
}
