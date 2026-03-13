//! Shared `fastmcp` alias surface for MCP server/client modules.
//!
//! This centralizes framework-type seams so migration away from `fastmcp`
//! can be done in one place. Re-exports consumed by mcp.rs, mcp_bridge.rs,
//! mcp_tools.rs, and mcp_client.rs during strangler-fig migration.

#[cfg(feature = "mcp-client")]
use crate::config::McpClientConfig;
#[cfg(feature = "mcp-client")]
use crate::mcp_client::{
    ExternalServerConfig, McpClientContentItem, McpClientError, McpClientToolDefinition,
};
#[cfg(feature = "mcp-client")]
use std::fmt::Display;

#[cfg(any(feature = "mcp", feature = "mcp-client"))]
#[allow(unused_imports)]
pub use fastmcp::memory::create_memory_transport_pair as framework_create_memory_transport_pair;
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
#[allow(unused_imports)]
pub use fastmcp::testing::TestClient as FrameworkTestClient;
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
#[allow(unused_imports)]
pub use fastmcp::{
    Content as FrameworkContent, McpContext as FrameworkMcpContext, McpError as FrameworkMcpError,
    McpResult as FrameworkMcpResult, Tool as FrameworkTool,
};

#[cfg(feature = "mcp-client")]
#[allow(unused_imports)]
pub use fastmcp::mcp_config::{
    ConfigLoader as FrameworkConfigLoader, ServerConfig as FrameworkServerConfig,
};

#[cfg(feature = "mcp-client")]
#[allow(unused_imports)]
pub use fastmcp::{
    Client as FrameworkClient, ClientBuilder as FrameworkClientBuilder,
    McpErrorCode as FrameworkMcpErrorCode,
};

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub use fastmcp::{
    Resource as FrameworkResource, ResourceContent as FrameworkResourceContent,
    ResourceHandler as FrameworkResourceHandler, ResourceTemplate as FrameworkResourceTemplate,
    Server as FrameworkServer, ServerBuilder as FrameworkServerBuilder,
    StdioTransport as FrameworkStdioTransport, ToolHandler as FrameworkToolHandler,
};

#[cfg(feature = "mcp-client")]
#[derive(Debug)]
pub(crate) struct DiscoveredFrameworkServers {
    pub(crate) search_paths: Vec<String>,
    pub(crate) servers: Vec<ExternalServerConfig>,
}

#[cfg(feature = "mcp-client")]
pub(crate) struct OutboundFrameworkClient {
    inner: FrameworkClient,
}

#[cfg(feature = "mcp-client")]
pub(crate) enum OutboundFrameworkError {
    Transport(FrameworkMcpError),
    Mapping(McpClientError),
}

#[cfg(feature = "mcp-client")]
impl OutboundFrameworkClient {
    pub(crate) fn connect_stdio(
        server: &ExternalServerConfig,
        settings: &McpClientConfig,
    ) -> Result<Self, FrameworkMcpError> {
        let mut builder = FrameworkClientBuilder::new()
            .client_info("frankenterm-mcp-client", env!("CARGO_PKG_VERSION"))
            .timeout_ms(settings.timeout_ms)
            .max_retries(settings.max_retries)
            .retry_delay_ms(settings.retry_delay_ms);

        if let Some(cwd) = server.cwd.as_ref() {
            builder = builder.working_dir(cwd);
        }
        if !server.env.is_empty() {
            builder = builder.envs(server.env.clone());
        }

        let args_ref: Vec<&str> = server.args.iter().map(String::as_str).collect();
        let client = builder.connect_stdio(&server.command, &args_ref)?;
        Ok(Self { inner: client })
    }

    pub(crate) fn list_tool_definitions(
        &mut self,
    ) -> std::result::Result<Vec<McpClientToolDefinition>, OutboundFrameworkError> {
        self.inner
            .list_tools()
            .map_err(OutboundFrameworkError::Transport)?
            .into_iter()
            .map(McpClientToolDefinition::from_framework)
            .collect::<Result<Vec<_>, _>>()
            .map_err(OutboundFrameworkError::Mapping)
    }

    pub(crate) fn call_tool_content(
        &mut self,
        name: &str,
        arguments: serde_json::Value,
    ) -> std::result::Result<Vec<McpClientContentItem>, OutboundFrameworkError> {
        self.inner
            .call_tool(name, arguments)
            .map_err(OutboundFrameworkError::Transport)?
            .into_iter()
            .map(McpClientContentItem::from_framework)
            .collect::<Result<Vec<_>, _>>()
            .map_err(OutboundFrameworkError::Mapping)
    }
}

#[cfg(feature = "mcp-client")]
pub(crate) fn discover_server_configs(settings: &McpClientConfig) -> DiscoveredFrameworkServers {
    let loader = build_loader(settings);
    let search_paths = loader
        .search_paths()
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let merged = loader.load_all();

    let mut servers: Vec<ExternalServerConfig> = merged
        .mcp_servers
        .into_iter()
        .map(|(name, cfg)| ExternalServerConfig {
            name,
            command: cfg.command,
            args: cfg.args,
            env: cfg.env,
            cwd: cfg.cwd,
            disabled: cfg.disabled,
        })
        .collect();
    servers.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });

    DiscoveredFrameworkServers {
        search_paths,
        servers,
    }
}

#[cfg(feature = "mcp-client")]
impl McpClientToolDefinition {
    fn from_framework(tool: FrameworkTool) -> Result<Self, McpClientError> {
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

    pub(crate) fn into_framework(self) -> Result<FrameworkTool, McpClientError> {
        Ok(FrameworkTool {
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
    fn from_framework(content: FrameworkContent) -> Result<Self, McpClientError> {
        roundtrip_framework_payload("remote tool content", content)
    }

    pub(crate) fn into_framework(self) -> Result<FrameworkContent, McpClientError> {
        roundtrip_framework_payload("remote tool content", self)
    }
}

#[cfg(feature = "mcp-client")]
fn build_loader(settings: &McpClientConfig) -> FrameworkConfigLoader {
    let mut loader = if settings.include_default_paths {
        FrameworkConfigLoader::new()
    } else {
        let mut paths = settings.discovery_paths.iter();
        let first = paths
            .next()
            .expect("discovery_paths must be non-empty when default paths are disabled");
        let mut loader = FrameworkConfigLoader::from_path(first.clone());
        for path in paths {
            loader = loader.with_path(path.clone());
        }
        return loader;
    };

    for path in settings.discovery_paths.iter().rev() {
        loader = loader.with_priority_path(path.clone());
    }

    loader
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
fn framework_payload_error(label: &str, err: impl Display) -> McpClientError {
    McpClientError::new(
        "mcp_client.protocol",
        format!("Failed to map {label} across the MCP client seam: {err}"),
    )
}

#[cfg(all(test, feature = "mcp-client"))]
mod tests {
    use super::{McpClientContentItem, McpClientToolDefinition};

    #[test]
    fn tool_definition_roundtrips_across_framework_seam() {
        let definition = McpClientToolDefinition {
            name: "echo".to_string(),
            description: Some("Echo input text".to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"]
            }),
            output_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {"type": "array"}
                }
            })),
            icon: None,
            version: Some("1.2.3".to_string()),
            tags: vec!["utility".to_string(), "safe".to_string()],
            annotations: None,
        };

        let framework = definition
            .clone()
            .into_framework()
            .expect("map tool definition into framework type");
        let recovered = McpClientToolDefinition::from_framework(framework)
            .expect("map tool definition back out of framework type");

        assert_eq!(recovered, definition);
    }

    #[test]
    fn tool_definition_rejects_invalid_icon_payload() {
        let err = McpClientToolDefinition {
            name: "echo".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: Some(serde_json::json!("not-a-valid-framework-icon")),
            version: None,
            tags: Vec::new(),
            annotations: None,
        }
        .into_framework()
        .expect_err("invalid icon payload should fail framework mapping");

        assert_eq!(err.code, "mcp_client.protocol");
        assert!(err.message.contains("remote tool icon"));
    }

    #[test]
    fn content_item_roundtrips_across_framework_seam() {
        let content = McpClientContentItem(serde_json::json!({
            "type": "text",
            "text": "hello from seam test"
        }));

        let framework = content
            .clone()
            .into_framework()
            .expect("map content into framework type");
        let recovered = McpClientContentItem::from_framework(framework)
            .expect("map content back out of framework type");

        assert_eq!(recovered, content);
        assert_eq!(recovered.as_text(), Some("hello from seam test"));
    }
}
