#![cfg(all(feature = "mcp", feature = "mcp-client"))]

use fastmcp::testing::TestClient;
use frankenterm_core::config::Config;
use frankenterm_core::mcp::build_server_with_db;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;
use tempfile::tempdir;

fn init_test_logging() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

fn write_mock_proxy_server_script(dir: &Path) -> PathBuf {
    let script_path = dir.join("mock_proxy_server.py");
    let script = r#"#!/usr/bin/env python3
import json
import sys

def send(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue

    request = json.loads(raw)
    method = request.get("method")
    req_id = request.get("id")

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-proxy", "version": "1.0.0"}
            }
        })
    elif method == "initialized":
        continue
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [
                    {
                        "name": "echo",
                        "description": "Echo input text",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"]
                        },
                        "annotations": {"destructive": False}
                    },
                    {
                        "name": "drop_db",
                        "description": "Dangerous mutating operation",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        },
                        "annotations": {"destructive": True}
                    }
                ]
            }
        })
    elif method == "tools/call":
        params = request.get("params") or {}
        tool_name = params.get("name")
        arguments = params.get("arguments") or {}
        if tool_name == "echo":
            send({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": str(arguments.get("text", ""))}],
                    "isError": False
                }
            })
        else:
            send({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "content": [{"type": "text", "text": "unsupported"}],
                    "isError": True
                }
            })
    else:
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": "Method not found"}
        })
"#;
    std::fs::write(&script_path, script).expect("write mock proxy server script");
    script_path
}

fn write_discovery_config(
    dir: &Path,
    server_name: &str,
    command: &str,
    args: &[String],
) -> PathBuf {
    let config_path = dir.join("mcp-proxy-config.json");
    let payload = json!({
        "mcpServers": {
            server_name: {
                "command": command,
                "args": args,
            }
        }
    });
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&payload).expect("serialize discovery config"),
    )
    .expect("write discovery config");
    config_path
}

fn make_proxy_config(discovery_path: &Path, server_name: &str) -> Config {
    let mut config = Config::default();
    config.mcp_client.enabled = true;
    config.mcp_client.discovery_enabled = true;
    config.mcp_client.include_default_paths = false;
    config.mcp_client.discovery_paths = vec![discovery_path.display().to_string()];
    config.mcp_client.proxy_enabled = true;
    config.mcp_client.proxy_prefix = "remote".to_string();
    config.mcp_client.proxy_mount_all_discovered = false;
    config.mcp_client.proxy_servers = vec![server_name.to_string()];
    config.mcp_client.proxy_fallback_to_local = true;
    config.mcp_client.proxy_strict = false;
    config
}

#[test]
fn proxy_mounts_remote_tools_with_prefixed_routes() {
    init_test_logging();
    if !python3_available() {
        eprintln!("Skipping proxy integration test: python3 is not available");
        return;
    }

    let temp_dir = tempdir().expect("temp dir");
    let script_path = write_mock_proxy_server_script(temp_dir.path());
    let discovery_path = write_discovery_config(
        temp_dir.path(),
        "mock",
        "python3",
        &["-u".to_string(), script_path.display().to_string()],
    );
    let config = make_proxy_config(&discovery_path, "mock");

    eprintln!(
        "Building MCP server with proxy enabled: discovery={} script={}",
        discovery_path.display(),
        script_path.display()
    );
    let server = build_server_with_db(&config, None).expect("build proxy-enabled server");
    let tool_names: BTreeSet<String> = server.tools().into_iter().map(|tool| tool.name).collect();

    eprintln!("Registered tool names: {tool_names:?}");
    assert!(tool_names.contains("wa.state"));
    assert!(tool_names.contains("remote/mock/echo"));
    assert!(
        !tool_names.contains("remote/mock/drop_db"),
        "destructive remote tool should be filtered by default"
    );
}

#[test]
fn proxy_routes_calls_to_remote_tools() {
    init_test_logging();
    if !python3_available() {
        eprintln!("Skipping proxy routing call test: python3 is not available");
        return;
    }

    let temp_dir = tempdir().expect("temp dir");
    let script_path = write_mock_proxy_server_script(temp_dir.path());
    let discovery_path = write_discovery_config(
        temp_dir.path(),
        "mock",
        "python3",
        &["-u".to_string(), script_path.display().to_string()],
    );
    let config = make_proxy_config(&discovery_path, "mock");
    let server = build_server_with_db(&config, None).expect("build proxy-enabled server");

    let (client_transport, server_transport) = fastmcp::memory::create_memory_transport_pair();
    std::thread::spawn(move || {
        let _ = server.run_transport(server_transport);
    });

    let mut client = TestClient::new(client_transport);
    client.initialize().expect("initialize in-memory client");

    let reply = client
        .call_tool("remote/mock/echo", json!({"text": "proxy-route-check"}))
        .expect("invoke proxied remote tool");

    eprintln!("Proxied tool reply: {reply:?}");
    assert!(matches!(
        reply.first(),
        Some(fastmcp::Content::Text { text }) if text == "proxy-route-check"
    ));
}

#[test]
fn proxy_fallback_preserves_local_tools_when_remote_is_unavailable() {
    init_test_logging();
    let temp_dir = tempdir().expect("temp dir");
    let missing_command = "nonexistent_proxy_command_for_ft";
    let discovery_path = write_discovery_config(
        temp_dir.path(),
        "broken",
        missing_command,
        &Vec::<String>::new(),
    );
    let mut config = make_proxy_config(&discovery_path, "broken");
    config.mcp_client.proxy_fallback_to_local = true;
    config.mcp_client.proxy_strict = false;

    eprintln!(
        "Building MCP server with fallback mode and missing command: {}",
        missing_command
    );
    let server = build_server_with_db(&config, None).expect("fallback mode should keep local MCP");
    let tool_names: BTreeSet<String> = server.tools().into_iter().map(|tool| tool.name).collect();

    eprintln!("Tool names in fallback mode: {tool_names:?}");
    assert!(tool_names.contains("wa.state"));
    assert!(
        !tool_names
            .iter()
            .any(|name| name.starts_with("remote/broken/")),
        "no proxied tools should be mounted when remote command fails"
    );
}

#[test]
fn proxy_strict_mode_fails_startup_when_remote_is_unavailable() {
    init_test_logging();
    let temp_dir = tempdir().expect("temp dir");
    let discovery_path = write_discovery_config(
        temp_dir.path(),
        "broken",
        "nonexistent_proxy_command_for_ft",
        &Vec::<String>::new(),
    );
    let mut config = make_proxy_config(&discovery_path, "broken");
    config.mcp_client.proxy_strict = true;
    config.mcp_client.proxy_fallback_to_local = false;

    let err = match build_server_with_db(&config, None) {
        Ok(_) => panic!("strict mode must fail on connect"),
        Err(err) => err,
    };
    let message = err.to_string();
    eprintln!("Strict-mode startup error: {message}");
    assert!(
        message.contains("mcp proxy connect failed for server"),
        "unexpected strict-mode error message: {message}"
    );
}
