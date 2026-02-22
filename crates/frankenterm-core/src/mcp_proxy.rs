//! MCP proxy composition helpers (feature: `mcp-client`).
//!
//! This module mounts remote MCP tools into the local server namespace using an
//! explicit routing policy:
//! - local tools keep existing names (`wa.*`),
//! - remote tools are mounted under `<proxy_prefix>/<server>/<tool>`.

use super::*;
use crate::config::McpClientConfig;
use crate::mcp_client::{ExternalServerConfig, FtMcpClient, discover_servers};
use fastmcp::ServerBuilder;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const LOG_TARGET: &str = "ft::mcp_proxy";

pub(super) fn compose_proxy_tools(
    mut builder: ServerBuilder,
    config: &Config,
    db_path: Option<Arc<PathBuf>>,
) -> Result<ServerBuilder> {
    let settings = &config.mcp_client;
    if !settings.proxy_enabled {
        return Ok(builder);
    }

    let fail_fast = settings.proxy_strict || !settings.proxy_fallback_to_local;
    if !settings.enabled {
        let message = "mcp_client.proxy_enabled requires mcp_client.enabled=true";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_disabled_client",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message}; continuing with local-only MCP server"
        );
        return Ok(builder);
    }

    let discovered = match discover_servers(config) {
        Ok(servers) => servers,
        Err(err) => {
            if fail_fast {
                return Err(crate::error::ConfigError::ValidationError(format!(
                    "mcp proxy discovery failed: {}",
                    err.message
                ))
                .into());
            }
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_discovery_failed",
                code = err.code,
                message = %err.message,
                fallback_to_local = settings.proxy_fallback_to_local,
                strict = settings.proxy_strict,
                "Remote MCP discovery failed; continuing with local-only MCP server"
            );
            return Ok(builder);
        }
    };

    let selected = match select_proxy_servers(settings, &discovered) {
        Ok(selected) => selected,
        Err(message) => {
            let wrapped = format!("mcp proxy server selection failed: {message}");
            if fail_fast {
                return Err(crate::error::ConfigError::ValidationError(wrapped).into());
            }
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_selection_failed",
                message = %message,
                fallback_to_local = settings.proxy_fallback_to_local,
                strict = settings.proxy_strict,
                "Remote MCP server selection failed; continuing with local-only MCP server"
            );
            return Ok(builder);
        }
    };

    if selected.is_empty() {
        let message = "no remote MCP servers selected for proxy composition";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_no_servers",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message}; continuing with local-only MCP server"
        );
        return Ok(builder);
    }

    let mut mounted_tools = 0usize;
    let mut mounted_servers = 0usize;
    let base_prefix = settings.proxy_prefix.trim();

    for server in selected {
        let server_name = server.name.clone();
        let route_prefix = format!("{base_prefix}/{}", sanitize_prefix_segment(&server_name));
        let remote = match FtMcpClient::connect_external(server, settings) {
            Ok(client) => client,
            Err(err) => {
                if fail_fast {
                    return Err(crate::error::ConfigError::ValidationError(format!(
                        "mcp proxy connect failed for server '{server_name}': {}",
                        err.message
                    ))
                    .into());
                }
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_connect_failed",
                    server = %server_name,
                    code = err.code,
                    message = %err.message,
                    fallback_to_local = settings.proxy_fallback_to_local,
                    strict = settings.proxy_strict,
                    "Remote MCP connect failed; skipping server"
                );
                continue;
            }
        };

        let shared_client = Arc::new(Mutex::new(remote));
        let tools = match list_remote_tools(&shared_client, &server_name) {
            Ok(tools) => tools,
            Err(err) => {
                if fail_fast {
                    return Err(crate::error::ConfigError::ValidationError(format!(
                        "mcp proxy tool catalog failed for server '{server_name}': {}",
                        err.message
                    ))
                    .into());
                }
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_list_tools_failed",
                    server = %server_name,
                    code = err.code,
                    message = %err.message,
                    fallback_to_local = settings.proxy_fallback_to_local,
                    strict = settings.proxy_strict,
                    "Failed to fetch remote tool catalog; skipping server"
                );
                continue;
            }
        };

        let filtered = filter_remote_tools(settings, tools);
        if filtered.is_empty() {
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_no_tools_after_filter",
                server = %server_name,
                allow_mutating = settings.proxy_allow_mutating_tools,
                "No tools remained after proxy safety filtering; skipping server"
            );
            continue;
        }

        let mut server_tools = 0usize;
        for tool in filtered {
            let external_name = tool.name.clone();
            let exposed_name = format!("{route_prefix}/{}", external_name);
            let handler = RemoteProxyToolHandler::new(
                tool,
                exposed_name.clone(),
                external_name.clone(),
                server_name.clone(),
                Arc::clone(&shared_client),
            );

            builder = if let Some(path) = db_path.as_ref() {
                builder.tool(FormatAwareToolHandler::new(AuditedToolHandler::new(
                    handler,
                    exposed_name,
                    Arc::clone(path),
                )))
            } else {
                builder.tool(FormatAwareToolHandler::new(handler))
            };
            server_tools += 1;
            mounted_tools += 1;
        }

        mounted_servers += 1;
        tracing::info!(
            target: LOG_TARGET,
            event = "mcp_proxy_mounted_server",
            server = %server_name,
            route_prefix = %route_prefix,
            mounted_tools = server_tools,
            "Mounted remote MCP tools"
        );
    }

    if mounted_servers == 0 {
        let message = "mcp proxy composition produced zero mounted remote servers";
        if fail_fast {
            return Err(crate::error::ConfigError::ValidationError(message.to_string()).into());
        }
        tracing::warn!(
            target: LOG_TARGET,
            event = "mcp_proxy_mount_none",
            fallback_to_local = settings.proxy_fallback_to_local,
            strict = settings.proxy_strict,
            "{message}; continuing with local-only MCP server"
        );
    } else {
        tracing::info!(
            target: LOG_TARGET,
            event = "mcp_proxy_compose_complete",
            mounted_servers,
            mounted_tools,
            route_policy = "prefix",
            allow_mutating = settings.proxy_allow_mutating_tools,
            "MCP proxy composition complete"
        );
    }

    Ok(builder)
}

fn list_remote_tools(
    client: &Arc<Mutex<FtMcpClient>>,
    server_name: &str,
) -> crate::mcp_client::McpClientResult<Vec<Tool>> {
    let mut guard = client.lock().map_err(|_| {
        crate::mcp_client::McpClientError::new(
            "mcp_proxy.client_lock_poisoned",
            format!("server '{server_name}': proxy client lock poisoned"),
        )
    })?;
    guard.list_tools()
}

fn filter_remote_tools(settings: &McpClientConfig, tools: Vec<Tool>) -> Vec<Tool> {
    if settings.proxy_allow_mutating_tools {
        return tools;
    }

    let mut filtered = Vec::with_capacity(tools.len());
    for tool in tools {
        let destructive = tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.destructive)
            .unwrap_or(false);
        if destructive {
            tracing::warn!(
                target: LOG_TARGET,
                event = "mcp_proxy_tool_filtered",
                tool = %tool.name,
                reason = "destructive_tool_blocked",
                "Skipping destructive remote tool due to proxy safety policy"
            );
            continue;
        }
        filtered.push(tool);
    }
    filtered
}

fn select_proxy_servers(
    settings: &McpClientConfig,
    discovered: &[ExternalServerConfig],
) -> std::result::Result<Vec<ExternalServerConfig>, String> {
    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    let mut push_server = |name: &str| -> std::result::Result<(), String> {
        let name = name.trim();
        let server = discovered
            .iter()
            .find(|item| item.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| format!("configured proxy server not found: {name}"))?;
        if server.disabled {
            return Err(format!(
                "configured proxy server is disabled: {}",
                server.name
            ));
        }

        let canonical = server.name.to_ascii_lowercase();
        if seen.insert(canonical) {
            selected.push(server.clone());
        }
        Ok(())
    };

    if !settings.proxy_servers.is_empty() {
        for server in &settings.proxy_servers {
            push_server(server)?;
        }
        return Ok(selected);
    }

    if settings.proxy_mount_all_discovered {
        for server in discovered {
            if server.disabled {
                continue;
            }
            let canonical = server.name.to_ascii_lowercase();
            if seen.insert(canonical) {
                selected.push(server.clone());
            }
        }
        return Ok(selected);
    }

    if settings.preferred_servers.is_empty() {
        return Err(
            "proxy_mount_all_discovered=false requires proxy_servers or preferred_servers"
                .to_string(),
        );
    }

    for server in &settings.preferred_servers {
        push_server(server)?;
    }

    Ok(selected)
}

fn sanitize_prefix_segment(name: &str) -> String {
    let mut value = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            value.push(ch.to_ascii_lowercase());
        } else {
            value.push('-');
        }
    }
    let value = value.trim_matches('-');
    if value.is_empty() {
        "server".to_string()
    } else {
        value.to_string()
    }
}

struct RemoteProxyToolHandler {
    definition: Tool,
    exposed_name: String,
    external_name: String,
    server_name: String,
    client: Arc<Mutex<FtMcpClient>>,
}

impl RemoteProxyToolHandler {
    fn new(
        mut definition: Tool,
        exposed_name: String,
        external_name: String,
        server_name: String,
        client: Arc<Mutex<FtMcpClient>>,
    ) -> Self {
        definition.name = exposed_name.clone();
        Self {
            definition,
            exposed_name,
            external_name,
            server_name,
            client,
        }
    }
}

impl ToolHandler for RemoteProxyToolHandler {
    fn definition(&self) -> Tool {
        self.definition.clone()
    }

    fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let start = Instant::now();
        let mut guard = self.client.lock().map_err(|_| {
            McpError::internal_error(format!(
                "proxy route '{}' failed: remote client lock poisoned",
                self.exposed_name
            ))
        })?;

        match guard.call_tool(&self.external_name, arguments) {
            Ok(content) => {
                tracing::info!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_route",
                    route = "remote",
                    server = %self.server_name,
                    tool = %self.exposed_name,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Executed proxied remote MCP tool"
                );
                Ok(content)
            }
            Err(err) => {
                tracing::warn!(
                    target: LOG_TARGET,
                    event = "mcp_proxy_route_failed",
                    route = "remote",
                    server = %self.server_name,
                    tool = %self.exposed_name,
                    code = err.code,
                    message = %err.message,
                    elapsed_ms = start.elapsed().as_millis(),
                    "Remote MCP proxy tool failed"
                );
                Err(McpError::tool_error(format!(
                    "[{}] {}",
                    err.code, err.message
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_server(name: &str, disabled: bool) -> ExternalServerConfig {
        ExternalServerConfig {
            name: name.to_string(),
            command: "python3".to_string(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            disabled,
        }
    }

    #[test]
    fn select_proxy_servers_mount_all_filters_disabled() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: true,
            ..McpClientConfig::default()
        };
        let discovered = vec![
            make_server("alpha", false),
            make_server("beta", true),
            make_server("gamma", false),
        ];

        let selected = select_proxy_servers(&settings, &discovered).expect("select servers");
        let names: Vec<String> = selected.into_iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["alpha".to_string(), "gamma".to_string()]);
    }

    #[test]
    fn select_proxy_servers_uses_explicit_order() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["gamma".to_string(), "alpha".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![
            make_server("alpha", false),
            make_server("gamma", false),
            make_server("zeta", false),
        ];

        let selected = select_proxy_servers(&settings, &discovered).expect("select servers");
        let names: Vec<String> = selected.into_iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["gamma".to_string(), "alpha".to_string()]);
    }

    #[test]
    fn select_proxy_servers_trims_explicit_names() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["  gamma  ".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![make_server("gamma", false)];

        let selected = select_proxy_servers(&settings, &discovered).expect("select servers");
        let names: Vec<String> = selected.into_iter().map(|item| item.name).collect();
        assert_eq!(names, vec!["gamma".to_string()]);
    }

    #[test]
    fn select_proxy_servers_rejects_missing_explicit_server() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["delta".to_string()],
            ..McpClientConfig::default()
        };
        let discovered = vec![make_server("alpha", false)];

        let err = select_proxy_servers(&settings, &discovered).unwrap_err();
        assert!(err.contains("configured proxy server not found"));
    }

    #[test]
    fn sanitize_prefix_segment_normalizes_symbols() {
        assert_eq!(sanitize_prefix_segment("GitHub Copilot"), "github-copilot");
        assert_eq!(sanitize_prefix_segment("___"), "___");
        assert_eq!(sanitize_prefix_segment(" / "), "server");
    }

    #[test]
    fn filter_remote_tools_blocks_destructive_by_default() {
        let settings = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_allow_mutating_tools: false,
            ..McpClientConfig::default()
        };
        let safe = Tool {
            name: "safe".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(
                serde_json::from_value(serde_json::json!({"destructive": false}))
                    .expect("deserialize safe tool annotations"),
            ),
        };
        let destructive = Tool {
            name: "drop_db".to_string(),
            description: None,
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: Some(
                serde_json::from_value(serde_json::json!({"destructive": true}))
                    .expect("deserialize destructive tool annotations"),
            ),
        };

        let filtered = filter_remote_tools(&settings, vec![safe, destructive]);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "safe");
    }

    #[test]
    fn compose_proxy_tools_selection_error_falls_back_when_non_strict() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_strict = false;
        config.mcp_client.proxy_fallback_to_local = true;
        config.mcp_client.include_default_paths = false;
        config.mcp_client.proxy_mount_all_discovered = false;
        config.mcp_client.proxy_servers = vec!["missing".to_string()];

        let builder = Server::new("test", "0.0.0");
        let result = compose_proxy_tools(builder, &config, None);
        assert!(result.is_ok());
    }

    #[test]
    fn compose_proxy_tools_selection_error_fails_when_strict() {
        let mut config = Config::default();
        config.mcp_client.enabled = true;
        config.mcp_client.proxy_enabled = true;
        config.mcp_client.proxy_strict = true;
        config.mcp_client.proxy_fallback_to_local = false;
        config.mcp_client.include_default_paths = false;
        config.mcp_client.proxy_mount_all_discovered = false;
        config.mcp_client.proxy_servers = vec!["missing".to_string()];

        let builder = Server::new("test", "0.0.0");
        let result = compose_proxy_tools(builder, &config, None);
        assert!(result.is_err());
    }
}
