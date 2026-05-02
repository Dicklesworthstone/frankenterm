//! Core-neutral MCP type boundary for FrankenTerm.
//!
//! This crate owns the MCP DTOs and framework payload conversions that do not
//! need `frankenterm-core`. Runtime orchestration, config loading, redaction,
//! and policy enforcement still live in `frankenterm-core` while the tier-2
//! extraction continues.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Outbound MCP client configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpClientConfig {
    /// Enable outbound MCP client capabilities.
    pub enabled: bool,
    /// Enable config-driven server discovery.
    pub discovery_enabled: bool,
    /// Include fastmcp's default platform search paths.
    pub include_default_paths: bool,
    /// Additional discovery paths (JSON files using `mcpServers` schema).
    pub discovery_paths: Vec<String>,
    /// Preferred server names in selection order.
    pub preferred_servers: Vec<String>,
    /// Request timeout in milliseconds for outbound MCP calls.
    pub timeout_ms: u64,
    /// Maximum retries for outbound MCP subprocess connection.
    pub max_retries: u32,
    /// Retry delay in milliseconds between connection attempts.
    pub retry_delay_ms: u64,
    /// Enable MCP proxy composition.
    pub proxy_enabled: bool,
    /// Prefix root used for mounted remote tools/resources/prompts.
    pub proxy_prefix: String,
    /// Mount every enabled discovered server, or only configured allowlists.
    pub proxy_mount_all_discovered: bool,
    /// Explicit remote server names to mount when proxying.
    pub proxy_servers: Vec<String>,
    /// If true, proxy setup failures fail server startup.
    pub proxy_strict: bool,
    /// If true, continue with local-only server when proxy setup fails.
    pub proxy_fallback_to_local: bool,
    /// If true, include remote tools marked as destructive/mutating.
    pub proxy_allow_mutating_tools: bool,
}

impl Default for McpClientConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discovery_enabled: true,
            include_default_paths: true,
            discovery_paths: Vec::new(),
            preferred_servers: Vec::new(),
            timeout_ms: 30_000,
            max_retries: 0,
            retry_delay_ms: 1_000,
            proxy_enabled: false,
            proxy_prefix: "remote".to_string(),
            proxy_mount_all_discovered: true,
            proxy_servers: Vec::new(),
            proxy_strict: false,
            proxy_fallback_to_local: true,
            proxy_allow_mutating_tools: false,
        }
    }
}

impl McpClientConfig {
    /// Validate the MCP client config invariants before runtime use.
    pub fn validate(&self) -> Result<(), String> {
        if self.timeout_ms == 0 {
            return Err("mcp_client.timeout_ms must be >= 1".to_string());
        }

        if self.max_retries > 0 && self.retry_delay_ms == 0 {
            return Err(
                "mcp_client.retry_delay_ms must be >= 1 when mcp_client.max_retries > 0"
                    .to_string(),
            );
        }

        if self.proxy_enabled && !self.enabled {
            return Err("mcp_client.proxy_enabled requires mcp_client.enabled=true".to_string());
        }

        for (idx, path) in self.discovery_paths.iter().enumerate() {
            if path.trim().is_empty() {
                return Err(format!(
                    "mcp_client.discovery_paths[{idx}] must not be empty"
                ));
            }
        }

        let mut seen_servers = HashSet::new();
        for (idx, server) in self.preferred_servers.iter().enumerate() {
            let trimmed = server.trim();
            if trimmed.is_empty() {
                return Err(format!(
                    "mcp_client.preferred_servers[{idx}] must not be empty"
                ));
            }
            let canonical = trimmed.to_ascii_lowercase();
            if !seen_servers.insert(canonical) {
                return Err(format!(
                    "mcp_client.preferred_servers contains duplicate server name: {trimmed}"
                ));
            }
        }

        if self.proxy_enabled {
            let prefix = self.proxy_prefix.trim();
            if prefix.is_empty() {
                return Err(
                    "mcp_client.proxy_prefix must not be empty when proxy is enabled".to_string(),
                );
            }
            if prefix.contains('/') {
                return Err(
                    "mcp_client.proxy_prefix must not contain '/' (server segment is appended automatically)"
                        .to_string(),
                );
            }

            let mut seen_proxy_servers = HashSet::new();
            for (idx, server) in self.proxy_servers.iter().enumerate() {
                let trimmed = server.trim();
                if trimmed.is_empty() {
                    return Err(format!("mcp_client.proxy_servers[{idx}] must not be empty"));
                }
                let canonical = trimmed.to_ascii_lowercase();
                if !seen_proxy_servers.insert(canonical) {
                    return Err(format!(
                        "mcp_client.proxy_servers contains duplicate server name: {trimmed}"
                    ));
                }
            }

            if !self.proxy_mount_all_discovered
                && self.proxy_servers.is_empty()
                && self.preferred_servers.is_empty()
            {
                return Err(
                    "mcp_client.proxy_mount_all_discovered=false requires proxy_servers or preferred_servers"
                        .to_string(),
                );
            }
        }

        Ok(())
    }
}

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
    /// Build a mapped MCP client error.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    /// Attach an operator-facing remediation hint.
    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Convenience result alias for outbound MCP client operations.
pub type McpClientResult<T> = std::result::Result<T, McpClientError>;

/// Framework-neutral outbound MCP tool definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(feature = "mcp-client")]
impl McpClientToolDefinition {
    /// Convert a framework tool payload into the core-neutral DTO.
    pub fn from_framework(tool: fastmcp::Tool) -> Result<Self, McpClientError> {
        Ok(Self {
            name: tool.name,
            description: tool.description,
            input_schema: tool.input_schema,
            output_schema: tool.output_schema,
            icon: tool
                .icon
                .map(|icon| {
                    serde_json::to_value(icon)
                        .map_err(|err| framework_payload_error("remote tool icon", err))
                })
                .transpose()?,
            version: tool.version,
            tags: tool.tags,
            annotations: tool
                .annotations
                .map(|annotations| {
                    serde_json::to_value(annotations)
                        .map_err(|err| framework_payload_error("remote tool annotations", err))
                })
                .transpose()?,
        })
    }

    /// Convert the core-neutral DTO into the framework tool payload.
    pub fn into_framework(self) -> Result<fastmcp::Tool, McpClientError> {
        Ok(fastmcp::Tool {
            name: self.name,
            description: self.description,
            input_schema: self.input_schema,
            output_schema: self.output_schema,
            icon: self
                .icon
                .map(|value| {
                    serde_json::from_value(value)
                        .map_err(|err| framework_payload_error("remote tool icon", err))
                })
                .transpose()?,
            version: self.version,
            tags: self.tags,
            annotations: self
                .annotations
                .map(|value| {
                    serde_json::from_value(value)
                        .map_err(|err| framework_payload_error("remote tool annotations", err))
                })
                .transpose()?,
        })
    }
}

#[cfg(feature = "mcp-client")]
impl McpClientContentItem {
    /// Convert framework content into a core-neutral content item.
    pub fn from_framework(content: fastmcp::Content) -> Result<Self, McpClientError> {
        roundtrip_framework_payload("remote tool content", content)
    }

    /// Convert a core-neutral content item into framework content.
    pub fn into_framework(self) -> Result<fastmcp::Content, McpClientError> {
        roundtrip_framework_payload("remote tool content", self)
    }
}

#[cfg(feature = "mcp-client")]
fn roundtrip_framework_payload<T, U>(label: &str, payload: T) -> Result<U, McpClientError>
where
    T: serde::Serialize,
    U: for<'de> serde::Deserialize<'de>,
{
    let value = serde_json::to_value(payload).map_err(|err| framework_payload_error(label, err))?;
    serde_json::from_value(value).map_err(|err| framework_payload_error(label, err))
}

#[cfg(feature = "mcp-client")]
fn framework_payload_error(label: &str, err: impl std::fmt::Display) -> McpClientError {
    McpClientError::new(
        "mcp_client.protocol",
        format!("Failed to map {label} across the MCP client boundary: {err}"),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalServerConfig, McpClientConfig, McpClientContentItem, McpClientError,
        McpClientToolDefinition,
    };
    use std::collections::HashMap;

    #[test]
    fn external_server_config_roundtrips() {
        let config = ExternalServerConfig {
            name: "filesystem".to_string(),
            command: "npx".to_string(),
            args: vec!["-y".to_string(), "server".to_string()],
            env: HashMap::from([("TOKEN".to_string(), "redacted".to_string())]),
            cwd: Some("/tmp".to_string()),
            disabled: false,
        };

        let json = serde_json::to_string(&config).expect("serialize server config");
        let back: ExternalServerConfig =
            serde_json::from_str(&json).expect("deserialize server config");
        assert_eq!(back, config);
    }

    #[test]
    fn mcp_client_config_defaults_match_core_contract() {
        let config = McpClientConfig::default();

        assert!(!config.enabled);
        assert!(config.discovery_enabled);
        assert!(config.include_default_paths);
        assert_eq!(config.timeout_ms, 30_000);
        assert_eq!(config.max_retries, 0);
        assert_eq!(config.retry_delay_ms, 1_000);
        assert!(!config.proxy_enabled);
        assert_eq!(config.proxy_prefix, "remote");
        assert!(config.proxy_mount_all_discovered);
        assert!(!config.proxy_strict);
        assert!(config.proxy_fallback_to_local);
        assert!(!config.proxy_allow_mutating_tools);
        config.validate().expect("default config validates");
    }

    #[test]
    fn mcp_client_config_roundtrips() {
        let config = McpClientConfig {
            enabled: true,
            include_default_paths: false,
            discovery_paths: vec!["/tmp/mcp.json".to_string()],
            preferred_servers: vec!["filesystem".to_string()],
            timeout_ms: 45_000,
            max_retries: 2,
            retry_delay_ms: 250,
            proxy_enabled: true,
            proxy_mount_all_discovered: false,
            proxy_servers: vec!["filesystem".to_string()],
            proxy_allow_mutating_tools: true,
            ..McpClientConfig::default()
        };

        let json = serde_json::to_string(&config).expect("serialize mcp client config");
        let back: McpClientConfig =
            serde_json::from_str(&json).expect("deserialize mcp client config");

        assert_eq!(back, config);
        back.validate().expect("round-tripped config validates");
    }

    #[test]
    fn mcp_client_config_rejects_invalid_proxy_shape() {
        let config = McpClientConfig {
            enabled: true,
            proxy_enabled: true,
            proxy_prefix: "remote/prod".to_string(),
            ..McpClientConfig::default()
        };

        let err = config.validate().expect_err("proxy prefix with slash");

        assert!(err.contains("mcp_client.proxy_prefix"));
    }

    #[test]
    fn mcp_client_config_rejects_duplicate_servers_case_insensitively() {
        let config = McpClientConfig {
            preferred_servers: vec!["alpha".to_string(), "ALPHA".to_string()],
            ..McpClientConfig::default()
        };

        let err = config.validate().expect_err("duplicate preferred server");

        assert!(err.contains("duplicate server name"));
    }

    #[test]
    fn destructive_annotation_is_detected() {
        let tool = McpClientToolDefinition {
            name: "write_file".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: Some(serde_json::json!({"destructive": true})),
        };

        assert!(tool.is_destructive());
    }

    #[test]
    fn content_text_extraction_is_shape_sensitive() {
        let item = McpClientContentItem(serde_json::json!({
            "type": "text",
            "text": "hello"
        }));

        assert_eq!(item.as_text(), Some("hello"));
        assert_eq!(
            McpClientContentItem(serde_json::json!({"type": "image"})).as_text(),
            None
        );
    }

    #[test]
    fn error_display_omits_hint() {
        let err = McpClientError::new("mcp_client.timeout", "server timed out")
            .with_hint("increase timeout");

        assert_eq!(err.to_string(), "[mcp_client.timeout] server timed out");
        assert_eq!(err.hint.as_deref(), Some("increase timeout"));
    }
}
