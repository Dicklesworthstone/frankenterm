//! Outbound MCP client support (feature: `mcp-client`).
//!
//! This module provides a minimal abstraction over `fastmcp` client APIs for:
//! - configuration-driven MCP server discovery,
//! - deterministic server selection,
//! - outbound tool invocation with mapped errors.

use crate::config::{Config, McpClientConfig};
use crate::mcp_framework::{
    DiscoveredFrameworkServers, FrameworkMcpError, FrameworkMcpErrorCode, OutboundFrameworkClient,
    OutboundFrameworkError, discover_server_configs,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

const LOG_TARGET: &str = "ft::mcp_client";

const ERR_DISABLED: &str = "mcp_client.disabled";
const ERR_DISCOVERY_DISABLED: &str = "mcp_client.discovery_disabled";
const ERR_SERVER_NOT_FOUND: &str = "mcp_client.server_not_found";
const ERR_SERVER_DISABLED: &str = "mcp_client.server_disabled";
const ERR_SPAWN: &str = "mcp_client.spawn_failed";
const ERR_TIMEOUT: &str = "mcp_client.timeout";
const ERR_METHOD_NOT_FOUND: &str = "mcp_client.method_not_found";
const ERR_INVALID_PARAMS: &str = "mcp_client.invalid_params";
const ERR_TOOL_EXECUTION: &str = "mcp_client.tool_execution";
const ERR_REQUEST_CANCELLED: &str = "mcp_client.request_cancelled";
const ERR_PROTOCOL: &str = "mcp_client.protocol";

/// Lightweight external MCP server definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalServerConfig {
    /// Logical server name (config key).
    pub name: String,
    /// Executable command used for stdio transport.
    pub command: String,
    /// Command arguments.
    pub args: Vec<String>,
    /// Environment overrides.
    pub env: HashMap<String, String>,
    /// Optional working directory.
    pub cwd: Option<String>,
    /// Whether the server entry is disabled.
    pub disabled: bool,
}

/// Mapped outbound MCP client error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("[{code}] {message}")]
pub struct McpClientError {
    /// Stable machine-readable error code.
    pub code: &'static str,
    /// Human-readable error message.
    pub message: String,
    /// Optional remediation hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl McpClientError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Convenience result alias for outbound MCP client operations.
pub type McpClientResult<T> = std::result::Result<T, McpClientError>;

/// Framework-neutral outbound MCP tool definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpClientToolDefinition {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
}

impl McpClientToolDefinition {
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        self.annotations
            .as_ref()
            .and_then(|annotations| annotations.get("destructive"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }
}

/// Framework-neutral outbound MCP content item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct McpClientContentItem(pub serde_json::Value);

impl McpClientContentItem {
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        self.0
            .get("type")
            .and_then(serde_json::Value::as_str)
            .filter(|value| *value == "text")
            .and_then(|_| self.0.get("text"))
            .and_then(serde_json::Value::as_str)
    }
}

/// Outbound MCP client wrapper.
pub struct FtMcpClient {
    client: OutboundFrameworkClient,
    server: ExternalServerConfig,
}

impl FtMcpClient {
    /// Connect to an external MCP server via stdio subprocess.
    pub fn connect_external(
        server: ExternalServerConfig,
        settings: &McpClientConfig,
    ) -> McpClientResult<Self> {
        if !settings.enabled {
            return Err(client_disabled_error());
        }

        if server.disabled {
            return Err(server_disabled_error(&server.name));
        }

        let start = Instant::now();
        let client = OutboundFrameworkClient::connect_stdio(&server, settings)
            .map_err(|err| map_mcp_error(&server.name, err))?;

        tracing::info!(
            target: LOG_TARGET,
            event = "mcp_client_connect",
            server = %server.name,
            command = %server.command,
            elapsed_ms = start.elapsed().as_millis(),
            "Connected outbound MCP client"
        );

        Ok(Self { client, server })
    }

    /// Discover and connect to a configured server.
    ///
    /// Selection order:
    /// 1. `requested_server` if provided,
    /// 2. first enabled entry in `mcp_client.preferred_servers`,
    /// 3. first enabled discovered server (alphabetical by name).
    pub fn connect_from_config(
        config: &Config,
        requested_server: Option<&str>,
    ) -> McpClientResult<Self> {
        let discovered = discover_servers(config)?;
        let selected = select_server(config, &discovered, requested_server)?;
        Self::connect_external(selected, &config.mcp_client)
    }

    /// List tools from the connected server.
    pub fn list_tools(&mut self) -> McpClientResult<Vec<McpClientToolDefinition>> {
        let start = Instant::now();
        match self.client.list_tool_definitions() {
            Ok(tools) => {
                tracing::info!(
                    target: LOG_TARGET,
                    event = "mcp_client_list_tools",
                    server = %self.server.name,
                    tool_count = tools.len(),
                    elapsed_ms = start.elapsed().as_millis(),
                    "Listed outbound MCP tools"
                );
                Ok(tools)
            }
            Err(err) => {
                let mapped = match err {
                    OutboundFrameworkError::Transport(err) => map_mcp_error(&self.server.name, err),
                    OutboundFrameworkError::Mapping(err) => err,
                };
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_client_list_tools_failed",
                    server = %self.server.name,
                    code = mapped.code,
                    message = %mapped.message,
                    "Failed to list outbound MCP tools"
                );
                Err(mapped)
            }
        }
    }

    /// Call a remote tool.
    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> McpClientResult<Vec<McpClientContentItem>> {
        let start = Instant::now();
        match self.client.call_tool_content(name, arguments) {
            Ok(content) => {
                tracing::info!(
                    target: LOG_TARGET,
                    event = "mcp_client_call_tool",
                    server = %self.server.name,
                    tool = name,
                    content_items = content.len(),
                    elapsed_ms = start.elapsed().as_millis(),
                    "Executed outbound MCP tool"
                );
                Ok(content)
            }
            Err(err) => {
                let mapped = match err {
                    OutboundFrameworkError::Transport(err) => map_mcp_error(&self.server.name, err),
                    OutboundFrameworkError::Mapping(err) => err,
                };
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_client_call_tool_failed",
                    server = %self.server.name,
                    tool = name,
                    code = mapped.code,
                    message = %mapped.message,
                    "Failed outbound MCP tool execution"
                );
                Err(mapped)
            }
        }
    }

    /// Connected server name.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server.name
    }
}

/// Discover external MCP servers from configured search paths.
pub fn discover_servers(config: &Config) -> McpClientResult<Vec<ExternalServerConfig>> {
    let settings = &config.mcp_client;
    if !settings.enabled {
        return Err(client_disabled_error());
    }
    if !settings.discovery_enabled {
        return Err(discovery_disabled_error());
    }
    if !settings.include_default_paths && settings.discovery_paths.is_empty() {
        tracing::info!(
            target: LOG_TARGET,
            event = "mcp_client_discover_empty",
            include_default_paths = settings.include_default_paths,
            "Outbound MCP discovery disabled for default paths and no explicit discovery paths configured"
        );
        return Ok(Vec::new());
    }

    let DiscoveredFrameworkServers {
        search_paths,
        servers: discovered,
    } = discover_server_configs(settings);

    tracing::info!(
        target: LOG_TARGET,
        event = "mcp_client_discover",
        discovered_count = discovered.len(),
        search_path_count = search_paths.len(),
        "Discovered outbound MCP servers"
    );
    tracing::debug!(
        target: LOG_TARGET,
        event = "mcp_client_discover_paths",
        paths = ?search_paths,
        "Outbound MCP client discovery search paths"
    );

    Ok(discovered)
}

/// Select a server from discovered entries.
pub fn select_server(
    config: &Config,
    discovered: &[ExternalServerConfig],
    requested_server: Option<&str>,
) -> McpClientResult<ExternalServerConfig> {
    if let Some(requested) = requested_server {
        let requested_trimmed = requested.trim();
        let selected = discovered
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(requested_trimmed))
            .cloned()
            .ok_or_else(|| server_not_found_error(requested_trimmed))?;
        if selected.disabled {
            return Err(server_disabled_error(&selected.name));
        }
        return Ok(selected);
    }

    for preferred in &config.mcp_client.preferred_servers {
        let preferred = preferred.trim();
        if preferred.is_empty() {
            continue;
        }
        if let Some(found) = discovered
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(preferred))
        {
            if !found.disabled {
                return Ok(found.clone());
            }
        }
    }

    discovered
        .iter()
        .find(|item| !item.disabled)
        .cloned()
        .ok_or_else(|| {
            server_not_found_error(
                "no enabled outbound MCP servers discovered (check mcp_client discovery paths)",
            )
        })
}

fn client_disabled_error() -> McpClientError {
    McpClientError::new(ERR_DISABLED, "mcp_client is disabled in configuration")
        .with_hint("Enable [mcp_client].enabled=true to use outbound MCP client features.")
}

fn discovery_disabled_error() -> McpClientError {
    McpClientError::new(
        ERR_DISCOVERY_DISABLED,
        "mcp_client discovery is disabled in configuration",
    )
    .with_hint("Set [mcp_client].discovery_enabled=true or provide explicit server config.")
}

fn server_not_found_error(server: &str) -> McpClientError {
    McpClientError::new(
        ERR_SERVER_NOT_FOUND,
        format!("Outbound MCP server not found: {server}"),
    )
    .with_hint("Check mcp_client.discovery_paths and mcp_client.include_default_paths, then retry.")
}

fn server_disabled_error(server: &str) -> McpClientError {
    McpClientError::new(
        ERR_SERVER_DISABLED,
        format!("Outbound MCP server is disabled: {server}"),
    )
    .with_hint("Enable the server entry in its mcpServers config before connecting.")
}

fn map_mcp_error(server: &str, err: FrameworkMcpError) -> McpClientError {
    let base = format!("server '{server}': {}", err.message);
    let message_lower = err.message.to_ascii_lowercase();

    match err.code {
        FrameworkMcpErrorCode::MethodNotFound => McpClientError::new(ERR_METHOD_NOT_FOUND, base)
            .with_hint("Verify method compatibility between FrankenTerm and the external server."),
        FrameworkMcpErrorCode::InvalidParams => McpClientError::new(ERR_INVALID_PARAMS, base)
            .with_hint("Check tool arguments and request schema."),
        FrameworkMcpErrorCode::ToolExecutionError => McpClientError::new(ERR_TOOL_EXECUTION, base)
            .with_hint("Inspect remote tool logs and retry with validated arguments."),
        FrameworkMcpErrorCode::RequestCancelled => McpClientError::new(ERR_REQUEST_CANCELLED, base)
            .with_hint("The request was cancelled; retry or increase timeout settings."),
        FrameworkMcpErrorCode::InternalError if message_lower.contains("timed out") => {
            McpClientError::new(ERR_TIMEOUT, base).with_hint(
                "Increase mcp_client.timeout_ms or inspect remote server responsiveness.",
            )
        }
        FrameworkMcpErrorCode::InternalError
            if message_lower.contains("failed to spawn")
                || message_lower.contains("spawn subprocess") =>
        {
            McpClientError::new(ERR_SPAWN, base).with_hint(
                "Verify command path, execute permissions, and environment requirements.",
            )
        }
        FrameworkMcpErrorCode::ParseError
        | FrameworkMcpErrorCode::InvalidRequest
        | FrameworkMcpErrorCode::InternalError
        | FrameworkMcpErrorCode::ResourceNotFound
        | FrameworkMcpErrorCode::ResourceForbidden
        | FrameworkMcpErrorCode::PromptNotFound
        | FrameworkMcpErrorCode::Custom(_) => McpClientError::new(ERR_PROTOCOL, base),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, ERR_METHOD_NOT_FOUND, ERR_SERVER_DISABLED, ERR_SPAWN, ERR_TOOL_EXECUTION,
        ExternalServerConfig, FrameworkMcpError, FtMcpClient, LOG_TARGET, McpClientConfig,
        McpClientContentItem, McpClientToolDefinition, discover_servers, map_mcp_error,
        select_server,
    };
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tracing::field::{Field, Visit};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    #[test]
    fn map_mcp_error_method_not_found() {
        let err = map_mcp_error("mock", FrameworkMcpError::method_not_found("tools/list"));
        assert_eq!(err.code, ERR_METHOD_NOT_FOUND);
        assert!(err.message.contains("server 'mock'"));
        assert!(err.hint.is_some());
    }

    #[test]
    fn map_mcp_error_tool_execution() {
        let err = map_mcp_error("mock", FrameworkMcpError::tool_error("boom"));
        assert_eq!(err.code, ERR_TOOL_EXECUTION);
        assert!(err.message.contains("boom"));
    }

    #[test]
    fn map_mcp_error_spawn_failure() {
        let err = map_mcp_error(
            "mock",
            FrameworkMcpError::internal_error("Failed to spawn subprocess: No such file"),
        );
        assert_eq!(err.code, ERR_SPAWN);
        assert!(err.hint.is_some());
    }

    #[test]
    fn discover_servers_from_custom_path() {
        let temp_dir = tempdir().expect("temp dir");
        let config_path = temp_dir.path().join("mcp-config.json");
        std::fs::write(
            &config_path,
            r#"{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
    },
    "disabled-server": {
      "command": "python3",
      "args": ["-m", "example_server"],
      "disabled": true
    }
  }
}"#,
        )
        .expect("write config");

        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.include_default_paths = false;
        config.mcp_client.discovery_paths = vec![config_path.display().to_string()];

        let discovered = discover_servers(&config).expect("discover servers");
        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered[0].name, "disabled-server");
        assert!(discovered[0].disabled);
        assert_eq!(discovered[1].name, "filesystem");
    }

    #[test]
    fn discover_servers_no_defaults_and_no_paths_returns_empty() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.include_default_paths = false;
        config.mcp_client.discovery_paths = Vec::new();

        let discovered = discover_servers(&config).expect("discover servers");
        assert!(discovered.is_empty());
    }

    #[test]
    fn select_server_prefers_preferred_order() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.preferred_servers = vec!["beta".to_string(), "alpha".to_string()];

        let discovered = vec![
            ExternalServerConfig {
                name: "alpha".to_string(),
                command: "a".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                disabled: false,
            },
            ExternalServerConfig {
                name: "beta".to_string(),
                command: "b".to_string(),
                args: Vec::new(),
                env: HashMap::new(),
                cwd: None,
                disabled: false,
            },
        ];

        let selected = select_server(&config, &discovered, None).expect("select preferred server");
        assert_eq!(selected.name, "beta");
    }

    #[test]
    fn select_server_trims_preferred_names() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.preferred_servers = vec!["  beta  ".to_string()];

        let discovered = vec![ExternalServerConfig {
            name: "beta".to_string(),
            command: "b".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled: false,
        }];

        let selected = select_server(&config, &discovered, None).expect("select trimmed preferred");
        assert_eq!(selected.name, "beta");
    }

    #[test]
    fn select_server_rejects_disabled_requested_server() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;

        let discovered = vec![ExternalServerConfig {
            name: "filesystem".to_string(),
            command: "npx".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled: true,
        }];

        let err = select_server(&config, &discovered, Some("filesystem")).unwrap_err();
        assert_eq!(err.code, ERR_SERVER_DISABLED);
    }

    #[test]
    fn outbound_mcp_roundtrip_with_mock_stdio_server_emits_logs() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }

        let temp_dir = tempdir().expect("temp dir");
        let script_path = temp_dir.path().join("mock_mcp_server.py");
        std::fs::write(&script_path, mock_server_script()).expect("write mock script");

        let server = ExternalServerConfig {
            name: "mock".to_string(),
            command: "python3".to_string(),
            args: vec!["-u".to_string(), script_path.display().to_string()],
            env: HashMap::new(),
            cwd: None,
            disabled: false,
        };
        let settings = McpClientConfig {
            enabled: true,
            timeout_ms: 5_000,
            ..McpClientConfig::default()
        };

        let (_guard, events) = install_capture();
        let mut client =
            FtMcpClient::connect_external(server, &settings).expect("connect to mock server");
        let tools = client.list_tools().expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");

        let output = client
            .call_tool("echo", serde_json::json!({"text": "hello"}))
            .expect("call tool");
        assert_eq!(
            output.first().and_then(McpClientContentItem::as_text),
            Some("hello")
        );

        let captured = events.lock().expect("lock logs").clone();
        assert!(captured.iter().any(|event| {
            event.target == LOG_TARGET
                && event
                    .fields
                    .get("event")
                    .is_some_and(|value| field_matches(value, "mcp_client_connect"))
                && event
                    .fields
                    .get("server")
                    .is_some_and(|value| field_matches(value, "mock"))
        }));
        assert!(captured.iter().any(|event| {
            event.target == LOG_TARGET
                && event
                    .fields
                    .get("event")
                    .is_some_and(|value| field_matches(value, "mcp_client_list_tools"))
        }));
        assert!(captured.iter().any(|event| {
            event.target == LOG_TARGET
                && event
                    .fields
                    .get("event")
                    .is_some_and(|value| field_matches(value, "mcp_client_call_tool"))
                && event
                    .fields
                    .get("tool")
                    .is_some_and(|value| field_matches(value, "echo"))
        }));
    }

    #[test]
    fn tool_definition_reads_destructive_annotation() {
        let safe = McpClientToolDefinition {
            name: "safe".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: Some(serde_json::json!({"destructive": false})),
        };
        let destructive = McpClientToolDefinition {
            name: "drop_db".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: Some(serde_json::json!({"destructive": true})),
        };

        assert!(!safe.is_destructive());
        assert!(destructive.is_destructive());
    }

    fn field_matches(value: &str, expected: &str) -> bool {
        value == expected || value == format!("{expected:?}")
    }

    fn mock_server_script() -> &'static str {
        r#"#!/usr/bin/env python3
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
                "serverInfo": {"name": "mock-server", "version": "1.0.0"}
            }
        })
    elif method == "initialized":
        continue
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo input text",
                    "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
                }]
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
                    "content": [{"type": "text", "text": "tool not found"}],
                    "isError": True
                }
            })
    else:
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": f"Method not found: {method}"}
        })
"#
    }

    #[derive(Debug, Clone)]
    struct CapturedEvent {
        target: String,
        fields: HashMap<String, String>,
    }

    struct LogCapture {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl LogCapture {
        fn new() -> (Self, Arc<Mutex<Vec<CapturedEvent>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                },
                events,
            )
        }
    }

    struct FieldVisitor {
        fields: HashMap<String, String>,
    }

    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    impl<S: Subscriber> Layer<S> for LogCapture {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor {
                fields: HashMap::new(),
            };
            event.record(&mut visitor);
            let target = event.metadata().target().to_string();
            self.events
                .lock()
                .expect("lock log capture")
                .push(CapturedEvent {
                    target,
                    fields: visitor.fields,
                });
        }
    }

    fn install_capture() -> (
        tracing::dispatcher::DefaultGuard,
        Arc<Mutex<Vec<CapturedEvent>>>,
    ) {
        let (layer, events) = LogCapture::new();
        let subscriber = tracing_subscriber::registry().with(layer);
        let dispatch = tracing::Dispatch::new(subscriber);
        let guard = tracing::dispatcher::set_default(&dispatch);
        (guard, events)
    }
}
