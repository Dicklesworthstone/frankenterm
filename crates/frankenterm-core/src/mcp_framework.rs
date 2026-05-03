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

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub(crate) fn framework_server_builder(name: &str, version: &str) -> FrameworkServerBuilder {
    fastmcp::Server::new(name, version)
}

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub(crate) fn run_framework_stdio_server(server: FrameworkServer) -> FrameworkMcpResult<()> {
    let transport = FrameworkStdioTransport::stdio();
    server.run_transport(transport)
}

#[cfg(feature = "mcp-client")]
#[derive(Debug)]
pub(crate) struct DiscoveredFrameworkServers {
    pub(crate) search_paths: Vec<String>,
    pub(crate) servers: Vec<ExternalServerConfig>,
}

#[cfg(feature = "mcp-client")]
pub(crate) struct OutboundFrameworkClient {
    inner: FrameworkClient,
    /// Connect-time timeout (ms) cached for forensic visibility. [ft-bd3vr]
    ///
    /// `FrameworkClientBuilder::timeout_ms` is consumed when the
    /// `FrameworkClient` is built; the constructed client offers no
    /// public accessor for the value it locked in. We mirror it here
    /// so:
    ///
    ///   1. Operators inspecting an `OutboundFrameworkClient` instance
    ///      can see the timeout the wrapper is enforcing without
    ///      cross-referencing the originating `McpClientConfig`.
    ///   2. Future upstream support for per-call deadline propagation
    ///      (tracked under ft-bd3vr) has a stable wrapper-side field
    ///      to bind against.
    ///   3. The field name itself documents the contract — this is a
    ///      CONNECT-time timeout, not per-call. Per-call deadline
    ///      enforcement requires `fastmcp::Client::call_tool` to
    ///      acquire a per-call timeout parameter; that's a fastmcp
    ///      upstream change.
    connect_timeout_ms: u64,
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
        Ok(Self {
            inner: client,
            connect_timeout_ms: settings.timeout_ms,
        })
    }

    /// Connect-time timeout (ms) the wrapped `FrameworkClient` is
    /// enforcing on each call. [ft-bd3vr]
    ///
    /// **CONTRACT**: this is the timeout-per-call as locked in at
    /// `connect_stdio` time from `McpClientConfig::timeout_ms`. It
    /// applies to every `list_tool_definitions` / `call_tool_content`
    /// invocation but cannot be overridden per call until fastmcp
    /// upstream exposes a per-call timeout parameter on
    /// `Client::call_tool`. Callers needing a tighter per-call
    /// budget must construct a separate `OutboundFrameworkClient`
    /// with a different `McpClientConfig.timeout_ms` — there is no
    /// runtime override.
    ///
    /// Forensic / diagnostic tooling can read this to verify which
    /// timeout an operator-configured config actually settled on
    /// after defaults / merges.
    #[must_use]
    pub(crate) fn connect_timeout_ms(&self) -> u64 {
        self.connect_timeout_ms
    }

    /// List tools from the connected server.
    ///
    /// Bounded by [`Self::connect_timeout_ms`] (the connect-time
    /// timeout); per-call deadline propagation from a caller's `Cx`
    /// budget is not enforced at this layer (ft-bd3vr — blocked on
    /// fastmcp upstream API). The proxy layer at
    /// `mcp_proxy::RemoteProxyToolHandler::call` performs a Cx
    /// pre-flight checkpoint (br-ft-xhj38) so PRE-EXPIRED callers
    /// short-circuit before reaching this point; that's the
    /// available defense-in-depth until fastmcp adds a per-call
    /// timeout parameter.
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

    /// Call a remote tool.
    ///
    /// Bounded by [`Self::connect_timeout_ms`]. See
    /// [`Self::list_tool_definitions`] for the per-call deadline
    /// propagation contract (ft-bd3vr, blocked on fastmcp upstream).
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

    /// br-ft-dnzum: gracefully terminate the stdio connection.
    ///
    /// Delegates to `fastmcp::Client::close` (lib.rs:1138) which:
    /// 1. Closes the underlying transport (best-effort).
    /// 2. Sends SIGKILL to the spawned subprocess.
    /// 3. Reaps the subprocess via `child.wait()`.
    ///
    /// This is a **deterministic** teardown — callers don't have
    /// to rely on `Drop` running at scope-end (which Rust gives
    /// no scheduling guarantees about, especially across panic
    /// boundaries). Drop also runs after the wrapping `Mutex` /
    /// `Arc` cycle drains, which can be later than the caller
    /// expects.
    ///
    /// **Consumes self** — the client is unusable after this
    /// call. Mirrors `fastmcp::Client::close`'s by-value
    /// signature; matches the bead's proposed `shutdown(self)`
    /// shape.
    ///
    /// **Not yet wired**: `is_alive()` requires upstream
    /// fastmcp API support (`fastmcp::Client` has no
    /// `is_connected` / `is_alive` getter as of pin
    /// 884d45b1). Tracked as a follow-up — until then, callers
    /// must treat the wrapper as "alive until the next failed
    /// `call_tool_content`", matching the existing implicit
    /// contract.
    pub(crate) fn shutdown(self) {
        // fastmcp::Client::close consumes self; the inner field
        // here is the wrapped Client, so unwrap the wrapper and
        // hand the live Client to close().
        self.inner.close();
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
fn build_loader(settings: &McpClientConfig) -> FrameworkConfigLoader {
    let mut loader = if settings.include_default_paths {
        FrameworkConfigLoader::new()
    } else {
        let mut paths = settings.discovery_paths.iter();
        if let Some(first) = paths.next() {
            let mut loader = FrameworkConfigLoader::from_path(first.clone());
            for path in paths {
                loader = loader.with_path(path.clone());
            }
            return loader;
        }

        // [ft-zfbqo] Default paths disabled AND no custom discovery_paths
        // configured. The loader needs SOME path; return a guaranteed-
        // nonexistent placeholder under the platform's temp dir so the
        // loader finds no servers without panicking.
        //
        // Pre-fix this used the literal "/dev/null", which:
        //   1. Doesn't exist on Windows (future portability landmine).
        //   2. Was silent — operators with proxy_enabled=true who
        //      forgot to set discovery_paths got a zero-discovery
        //      proxy with no signal that misconfiguration was at
        //      fault. The downstream "no remote MCP servers selected"
        //      warn at compose_proxy_tools fired but couldn't
        //      distinguish "genuinely no servers" from "loader was
        //      misconfigured upstream".
        //
        // Post-fix:
        //   - Path is platform-portable (lives under temp_dir).
        //   - Suffix is unique enough that an operator inspecting
        //     /tmp can't mistake it for a real config.
        //   - Emits a tracing::warn so the misconfiguration is
        //     visible without log scraping the downstream call site.
        let placeholder = std::env::temp_dir().join("__ft_mcp_unconfigured_loader_no_op__");
        tracing::warn!(
            target: "ft::mcp_framework",
            event = "mcp_framework_loader_unconfigured",
            placeholder = %placeholder.display(),
            "mcp_client.include_default_paths=false with empty discovery_paths; \
             loader will discover zero MCP servers. Set discovery_paths or \
             enable include_default_paths to fix."
        );
        return FrameworkConfigLoader::from_path(placeholder);
    };

    for path in settings.discovery_paths.iter().rev() {
        loader = loader.with_priority_path(path.clone());
    }

    loader
}

#[cfg(all(test, feature = "mcp-client"))]
mod tests {
    use super::{McpClientContentItem, McpClientToolDefinition, discover_server_configs};
    use crate::config::McpClientConfig;
    use proptest::prelude::*;

    /// [ft-zfbqo] When mcp_client.include_default_paths=false AND
    /// discovery_paths is empty, the loader must:
    ///   1. Return zero discovered servers (no panic, no error).
    ///   2. Use a platform-portable placeholder path (no /dev/null).
    ///
    /// Pre-fix the placeholder was the Unix-specific literal "/dev/null"
    /// which would fail on Windows. Post-fix the placeholder lives under
    /// the platform's temp_dir so the same code works on any target.
    #[test]
    fn ft_zfbqo_unconfigured_loader_returns_empty_on_any_platform() {
        let mut settings = McpClientConfig::default();
        settings.include_default_paths = false;
        settings.discovery_paths.clear();

        let discovered = discover_server_configs(&settings);

        assert!(
            discovered.servers.is_empty(),
            "unconfigured loader must discover zero servers, got {:?}",
            discovered.servers
        );

        // Platform-portable placeholder: must NOT be the legacy
        // /dev/null path (would fail on Windows).
        for path in &discovered.search_paths {
            assert!(
                !path.starts_with("/dev/"),
                "ft-zfbqo: placeholder must not live under /dev/ \
                 (Unix-specific, fails on Windows); got {path}"
            );
        }

        // The placeholder we use lives under std::env::temp_dir() and
        // contains a unique sentinel suffix so operators inspecting
        // discovery state can spot the misconfiguration immediately.
        let temp_marker = std::env::temp_dir().display().to_string();
        let has_temp_placeholder = discovered.search_paths.iter().any(|p| {
            p.starts_with(&temp_marker) && p.contains("__ft_mcp_unconfigured_loader_no_op__")
        });
        assert!(
            has_temp_placeholder,
            "ft-zfbqo: search_paths must include the temp_dir-based \
             unconfigured placeholder; got {:?}",
            discovered.search_paths
        );
    }

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
            icon: Some(serde_json::json!({
                "src": "https://example.com/icon.png",
                "mimeType": "image/png",
                "sizes": "32x32"
            })),
            version: Some("1.2.3".to_string()),
            tags: vec!["utility".to_string(), "safe".to_string()],
            annotations: Some(serde_json::json!({
                "destructive": true,
                "idempotent": false,
                "readOnly": false,
                "openWorldHint": "accepts arbitrary text"
            })),
        };

        let framework = definition
            .clone()
            .into_framework()
            .expect("map tool definition into framework type");
        let recovered = McpClientToolDefinition::from_framework(framework)
            .expect("map tool definition back out of framework type");

        assert_eq!(recovered, definition);
        assert!(recovered.is_destructive());
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

    fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
        prop::option::of("[A-Za-z0-9 _.:/-]{1,32}")
    }

    fn arb_tags() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec("[A-Za-z0-9_.-]{1,16}", 0..4)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn prop_tool_definition_roundtrips_across_framework_seam(
            name in "[A-Za-z0-9_.-]{1,24}",
            description in arb_opt_string(),
            version in arb_opt_string(),
            tags in arb_tags(),
            destructive in any::<bool>(),
            has_icon in any::<bool>(),
        ) {
            let definition = McpClientToolDefinition {
                name: name.clone(),
                description: description.clone(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "content": { "type": "array" } },
                })),
                icon: has_icon.then(|| serde_json::json!({
                    "src": "https://example.com/icon.png",
                    "mimeType": "image/png",
                    "sizes": "32x32"
                })),
                version: version.clone(),
                tags: tags.clone(),
                annotations: Some(serde_json::json!({
                    "destructive": destructive,
                    "idempotent": !destructive,
                })),
            };

            let framework = definition.clone().into_framework().expect("into framework");
            let recovered = McpClientToolDefinition::from_framework(framework).expect("from framework");

            prop_assert_eq!(&recovered, &definition);
            prop_assert_eq!(recovered.is_destructive(), destructive);
        }

        #[test]
        fn prop_content_item_text_roundtrips_and_as_text(
            text in "[A-Za-z0-9 _.,:/-]{1,64}",
        ) {
            let content = McpClientContentItem(serde_json::json!({
                "type": "text",
                "text": text.clone(),
            }));

            let framework = content.clone().into_framework().expect("into framework");
            let recovered = McpClientContentItem::from_framework(framework).expect("from framework");

            prop_assert_eq!(&recovered, &content);
            prop_assert_eq!(recovered.as_text(), Some(text.as_str()));
        }

        #[test]
        fn prop_content_item_non_text_has_no_as_text(
            payload in "[A-Za-z0-9 _.,:/-]{1,32}",
        ) {
            let content = McpClientContentItem(serde_json::json!({
                "type": "image",
                "url": format!("https://example.com/{payload}.png"),
            }));

            let framework = content.clone().into_framework().expect("into framework");
            let recovered = McpClientContentItem::from_framework(framework).expect("from framework");

            prop_assert_eq!(&recovered, &content);
            prop_assert_eq!(recovered.as_text(), None);
        }
    }
}
