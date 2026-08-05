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
    ToolAnnotations as FrameworkToolAnnotations,
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
    JsonRpcMessage as FrameworkJsonRpcMessage,
    Prompt as FrameworkPrompt,
    Resource as FrameworkResource, ResourceContent as FrameworkResourceContent,
    ResourceHandler as FrameworkResourceHandler, ResourceTemplate as FrameworkResourceTemplate,
    ServerCapabilities as FrameworkServerCapabilities, ServerInfo as FrameworkServerInfo,
    Server as FrameworkServer, ServerBuilder as FrameworkServerBuilder,
    StdioTransport as FrameworkStdioTransport, ToolHandler as FrameworkToolHandler,
    Transport as FrameworkTransport, TransportError as FrameworkTransportError,
};

#[cfg(feature = "mcp")]
use std::sync::{Arc, Mutex};

/// Outcome at the concrete MCP response-transport boundary.
///
/// `DeliveryAcknowledged` means the complete response crossed the transport's
/// sender-side ownership boundary: production stdio wrote and flushed it, or
/// an in-memory transport atomically handed it to the peer queue. It
/// deliberately does **not** mean that the client process parsed, acknowledged,
/// or acted on the response.
#[cfg(feature = "mcp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameworkResponseDeliveryOutcome {
    DeliveryAcknowledged,
    Failed,
}

#[cfg(feature = "mcp")]
pub(crate) type FrameworkResponseDeliveryAction =
    Box<dyn FnOnce(FrameworkResponseDeliveryOutcome) + Send + 'static>;

#[cfg(feature = "mcp")]
#[derive(Default)]
enum FrameworkResponseDeliveryState {
    #[default]
    Idle,
    /// The tool's canonical JSON envelope exists, but outer requested-format
    /// serialization has not completed yet.
    Prepared(FrameworkResponseDeliveryAction),
    /// Final requested-format serialization succeeded; the next outgoing
    /// response owns this completion action.
    Armed(FrameworkResponseDeliveryAction),
}

/// Single-flight handoff between an MCP tool and the sequential response loop.
///
/// FastMCP's handler result does not carry a post-write callback and its
/// `Middleware::on_response` hook runs before transport serialization.
/// FrankenTerm exposes only FastMCP's sequential transport request loops, so
/// exactly one tool response can be awaiting transport completion.
/// This coordinator makes that invariant explicit and fails closed if it is
/// ever violated rather than associating a delivery action with the wrong
/// response.
#[cfg(feature = "mcp")]
#[derive(Default)]
pub(crate) struct FrameworkResponseDeliveryCoordinator {
    state: Mutex<FrameworkResponseDeliveryState>,
}

#[cfg(feature = "mcp")]
impl FrameworkResponseDeliveryCoordinator {
    /// Prepare an action after the tool's canonical JSON envelope exists.
    ///
    /// On collision the caller receives its action back and is responsible for
    /// invoking it with `Failed` so any durable leases are released.
    pub(crate) fn try_prepare(
        &self,
        action: FrameworkResponseDeliveryAction,
    ) -> Result<(), FrameworkResponseDeliveryAction> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*state {
            FrameworkResponseDeliveryState::Idle => {
                *state = FrameworkResponseDeliveryState::Prepared(action);
                Ok(())
            }
            FrameworkResponseDeliveryState::Prepared(_)
            | FrameworkResponseDeliveryState::Armed(_) => Err(action),
        }
    }

    /// Arm a prepared action only after final requested-format serialization.
    /// Returns `false` when this call had no claimed delivery to arm.
    #[must_use]
    pub(crate) fn activate_prepared(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::mem::take(&mut *state);
        match previous {
            FrameworkResponseDeliveryState::Prepared(action) => {
                *state = FrameworkResponseDeliveryState::Armed(action);
                true
            }
            other => {
                *state = other;
                false
            }
        }
    }

    /// Release a prepared action when final-format serialization cannot
    /// faithfully produce the tool payload.
    pub(crate) fn fail_prepared(&self) {
        let action = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = std::mem::take(&mut *state);
            match previous {
                FrameworkResponseDeliveryState::Prepared(action) => Some(action),
                other => {
                    *state = other;
                    None
                }
            }
        };
        if let Some(action) = action {
            action(FrameworkResponseDeliveryOutcome::Failed);
        }
    }

    /// Fail whichever single-flight action is present. This is used only when
    /// an invariant violation makes response/action association unknowable.
    pub(crate) fn fail_all(&self) {
        self.complete_next(FrameworkResponseDeliveryOutcome::Failed);
    }

    fn complete_next(&self, outcome: FrameworkResponseDeliveryOutcome) {
        let completion = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::take(&mut *state) {
                FrameworkResponseDeliveryState::Idle => None,
                FrameworkResponseDeliveryState::Prepared(action) => {
                    // A response reached the transport without the formatter
                    // arming it. Never finalize an incompletely serialized
                    // payload.
                    Some((action, FrameworkResponseDeliveryOutcome::Failed))
                }
                FrameworkResponseDeliveryState::Armed(action) => Some((action, outcome)),
            }
        };
        if let Some((action, completion_outcome)) = completion {
            action(completion_outcome);
        }
    }
}

/// Transport adapter that acknowledges the next armed tool delivery only
/// after the concrete transport crosses its sender-side delivery boundary.
#[cfg(feature = "mcp")]
struct FrameworkDeliveryAwareTransport<T> {
    inner: T,
    coordinator: Arc<FrameworkResponseDeliveryCoordinator>,
}

#[cfg(feature = "mcp")]
impl<T> FrameworkDeliveryAwareTransport<T> {
    fn new(inner: T, coordinator: Arc<FrameworkResponseDeliveryCoordinator>) -> Self {
        Self { inner, coordinator }
    }
}

/// Transport contract required by claim-capable MCP server entrypoints.
///
/// A successful return from [`Self::send_with_delivery_ack`] must mean that
/// the sender no longer owns buffered message bytes: they were either flushed
/// to the concrete transport or atomically accepted by an in-memory receiving
/// endpoint. The base FastMCP [`FrameworkTransport`] trait does not make this
/// guarantee, so accepting arbitrary implementations would allow durable event
/// claims to finalize while a response was still sitting in a sender-side
/// buffer.
///
/// Custom transports must implement this trait explicitly and provide the
/// stronger acknowledgment boundary. Merely implementing
/// [`FrameworkTransport`] is intentionally insufficient.
#[cfg(feature = "mcp")]
pub trait FrameworkDeliveryAcknowledgingTransport: FrameworkTransport {
    /// Send one message and return only after its delivery boundary is durable
    /// from the sender's perspective.
    ///
    /// # Errors
    ///
    /// Returns a transport error when the complete message cannot cross the
    /// implementation's documented sender-side ownership boundary.
    fn send_with_delivery_ack(
        &mut self,
        cx: &crate::cx::Cx,
        message: &FrameworkJsonRpcMessage,
    ) -> Result<(), FrameworkTransportError>;
}

#[cfg(feature = "mcp")]
impl<R, W> FrameworkDeliveryAcknowledgingTransport for fastmcp::StdioTransport<R, W>
where
    R: std::io::Read,
    W: std::io::Write,
{
    fn send_with_delivery_ack(
        &mut self,
        cx: &crate::cx::Cx,
        message: &FrameworkJsonRpcMessage,
    ) -> Result<(), FrameworkTransportError> {
        // FastMCP's StdioTransport::send writes the complete NDJSON record and
        // explicitly flushes its writer before returning Ok.
        FrameworkTransport::send(self, cx, message)
    }
}

#[cfg(feature = "mcp")]
impl FrameworkDeliveryAcknowledgingTransport for fastmcp::memory::MemoryTransport {
    fn send_with_delivery_ack(
        &mut self,
        cx: &crate::cx::Cx,
        message: &FrameworkJsonRpcMessage,
    ) -> Result<(), FrameworkTransportError> {
        // Channel send atomically transfers ownership to the peer's queue; the
        // sender retains no private buffer after Ok.
        FrameworkTransport::send(self, cx, message)
    }
}

#[cfg(feature = "mcp")]
impl<T: FrameworkDeliveryAcknowledgingTransport> FrameworkTransport
    for FrameworkDeliveryAwareTransport<T>
{
    fn send(
        &mut self,
        cx: &crate::cx::Cx,
        message: &FrameworkJsonRpcMessage,
    ) -> Result<(), FrameworkTransportError> {
        let response_is_error = matches!(
            message,
            FrameworkJsonRpcMessage::Response(response) if response.error.is_some()
        );
        let is_response = matches!(message, FrameworkJsonRpcMessage::Response(_));
        let result = self.inner.send_with_delivery_ack(cx, message);

        // Notifications and server-initiated requests are represented as
        // JsonRpcMessage::Request and must not consume the staged action. In the
        // sequential server loop, the next Response is the response produced by
        // the tool that staged it.
        if is_response {
            let outcome = if result.is_ok() && !response_is_error {
                FrameworkResponseDeliveryOutcome::DeliveryAcknowledged
            } else {
                FrameworkResponseDeliveryOutcome::Failed
            };
            self.coordinator.complete_next(outcome);
        }

        result
    }

    fn recv(
        &mut self,
        cx: &crate::cx::Cx,
    ) -> Result<FrameworkJsonRpcMessage, FrameworkTransportError> {
        loop {
            // FastMCP dispatches JSON-RPC notifications but correctly emits no
            // response for them. Fail any unresolved action before accepting
            // the next inbound message so a later unrelated response can never
            // consume an undelivered action.
            self.coordinator
                .complete_next(FrameworkResponseDeliveryOutcome::Failed);
            let message = self.inner.recv(cx)?;
            if matches!(
                &message,
                FrameworkJsonRpcMessage::Request(request)
                    if request.id.is_none() && request.method == "tools/call"
            ) {
                // A tool invocation needs a response boundary for results,
                // errors, audit truth, and claim completion. FastMCP assigns
                // notifications the unbudgeted parent Cx, so dispatching an
                // id-less long poll could also monopolize the sequential server
                // indefinitely. Drop such invalid calls before handler dispatch.
                tracing::warn!(
                    method = "tools/call",
                    "Ignoring id-less MCP tool invocation without a response boundary"
                );
                continue;
            }
            return Ok(message);
        }
    }

    fn close(&mut self) -> Result<(), FrameworkTransportError> {
        // A graceful close with an unsent response is a known delivery failure.
        // A hard process death cannot run this path; the durable lease expiry is
        // the recovery authority in that case.
        self.coordinator
            .complete_next(FrameworkResponseDeliveryOutcome::Failed);
        self.inner.close()
    }
}

#[cfg(feature = "mcp")]
impl<T> Drop for FrameworkDeliveryAwareTransport<T> {
    fn drop(&mut self) {
        // FastMCP's returning loop exits directly when `recv` observes a closed
        // transport and does not call `Transport::close`. Releasing from Drop
        // covers that orderly unwind/drop path. An abrupt process death still
        // relies on the durable lease expiry.
        self.coordinator
            .complete_next(FrameworkResponseDeliveryOutcome::Failed);
    }
}

/// FrankenTerm's FastMCP server plus its response-delivery coordinator.
///
/// Only read-only catalog inspection is forwarded. In particular, this type
/// deliberately does not expose FastMCP's `dispatch_request`: direct dispatch
/// has no concrete write/flush boundary and therefore cannot safely complete a
/// `wa.await_event --claim` delivery. Consuming transport entrypoints always
/// install the acknowledgment-aware adapter.
#[cfg(feature = "mcp")]
pub struct FrameworkDeliveryServer {
    inner: FrameworkServer,
    coordinator: Arc<FrameworkResponseDeliveryCoordinator>,
}

#[cfg(feature = "mcp")]
impl FrameworkDeliveryServer {
    pub(crate) fn new(
        inner: FrameworkServer,
        coordinator: Arc<FrameworkResponseDeliveryCoordinator>,
    ) -> Self {
        Self { inner, coordinator }
    }

    /// Returns the immutable server identity advertised during initialization.
    #[must_use]
    pub fn info(&self) -> &FrameworkServerInfo {
        self.inner.info()
    }

    /// Returns the immutable server capabilities advertised during initialization.
    #[must_use]
    pub fn capabilities(&self) -> &FrameworkServerCapabilities {
        self.inner.capabilities()
    }

    /// Lists registered tool definitions without exposing handler dispatch.
    #[must_use]
    pub fn tools(&self) -> Vec<FrameworkTool> {
        self.inner.tools()
    }

    /// Lists registered static resources without exposing resource dispatch.
    #[must_use]
    pub fn resources(&self) -> Vec<FrameworkResource> {
        self.inner.resources()
    }

    /// Lists registered resource templates without exposing resource dispatch.
    #[must_use]
    pub fn resource_templates(&self) -> Vec<FrameworkResourceTemplate> {
        self.inner.resource_templates()
    }

    /// Lists registered prompt definitions without exposing prompt dispatch.
    #[must_use]
    pub fn prompts(&self) -> Vec<FrameworkPrompt> {
        self.inner.prompts()
    }

    /// Run forever on an acknowledgment-capable transport using a root request context.
    pub fn run_transport<T>(self, transport: T) -> !
    where
        T: FrameworkDeliveryAcknowledgingTransport + Send + 'static,
    {
        self.inner.run_transport(FrameworkDeliveryAwareTransport::new(
            transport,
            self.coordinator,
        ))
    }

    /// Run forever on an acknowledgment-capable transport with an explicit context.
    pub fn run_transport_with_cx<T>(self, cx: &crate::cx::Cx, transport: T) -> !
    where
        T: FrameworkDeliveryAcknowledgingTransport + Send + 'static,
    {
        self.inner.run_transport_with_cx(
            cx,
            FrameworkDeliveryAwareTransport::new(transport, self.coordinator),
        )
    }

    /// Run until the acknowledgment-capable transport closes, then return.
    pub fn run_transport_returning<T>(self, transport: T)
    where
        T: FrameworkDeliveryAcknowledgingTransport + Send + 'static,
    {
        self.inner
            .run_transport_returning(FrameworkDeliveryAwareTransport::new(
                transport,
                self.coordinator,
            ));
    }

    /// Run with an explicit context until the transport closes, then return.
    pub fn run_transport_returning_with_cx<T>(self, cx: &crate::cx::Cx, transport: T)
    where
        T: FrameworkDeliveryAcknowledgingTransport + Send + 'static,
    {
        self.inner.run_transport_returning_with_cx(
            cx,
            FrameworkDeliveryAwareTransport::new(transport, self.coordinator),
        );
    }
}

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub(crate) fn framework_server_builder(name: &str, version: &str) -> FrameworkServerBuilder {
    fastmcp::Server::new(name, version)
}

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub(crate) fn run_framework_stdio_server(
    server: FrameworkDeliveryServer,
) -> FrameworkMcpResult<()> {
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
    /// Configured FastMCP response-timeout value, cached for forensic
    /// visibility. [ft-bd3vr]
    ///
    /// `FrameworkClientBuilder::timeout_ms` is consumed when the
    /// `FrameworkClient` is built; the constructed client offers no
    /// public accessor for the value it locked in. We mirror it here
    /// so:
    ///
    ///   1. Operators inspecting an `OutboundFrameworkClient` instance
    ///      can see the timeout the wrapper is enforcing without
    ///      cross-referencing the originating `McpClientConfig`.
    ///   2. Future upstream support for caller-specific deadline propagation
    ///      (tracked under ft-bd3vr) has a stable wrapper-side field to bind
    ///      against.
    ///   3. Diagnostics do not mistake configuration for enforcement. At the
    ///      pinned FastMCP revision, the deadline is checked only between
    ///      receive attempts; synchronous stdio `read_line` can block past it.
    configured_response_timeout_ms: u64,
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
            configured_response_timeout_ms: settings.timeout_ms,
        })
    }

    /// Response-timeout value configured on the wrapped `FrameworkClient`.
    /// [ft-bd3vr]
    ///
    /// **CONTRACT**: this is diagnostic configuration, not a proven wall-clock
    /// upper bound. The pinned FastMCP client checks the deadline only before
    /// each synchronous transport receive; its stdio `read_line` can remain
    /// blocked after the configured duration. It also cannot be overridden by
    /// an individual caller until FastMCP exposes a Cx/deadline-aware call API.
    ///
    /// Forensic / diagnostic tooling can read this to verify which
    /// timeout an operator-configured config actually settled on
    /// after defaults / merges.
    #[must_use]
    pub(crate) fn configured_response_timeout_ms(&self) -> u64 {
        self.configured_response_timeout_ms
    }

    /// List tools from the connected server.
    ///
    /// FastMCP receives [`Self::configured_response_timeout_ms`] as its
    /// request-timeout setting, but the synchronous stdio receive can block
    /// beyond that value. Per-call deadline propagation from a caller's `Cx`
    /// budget is not enforced at this layer (ft-bd3vr — blocked on a
    /// cancellation-safe FastMCP transport/call API). The proxy layer at
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
    /// Uses the configured FastMCP response-timeout policy but is not a hard
    /// wall-clock-bounded operation. See [`Self::list_tool_definitions`] for
    /// the exact deadline limitation (ft-bd3vr).
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
    let Some(loader) = build_loader(settings) else {
        tracing::warn!(
            target: "ft::mcp_framework",
            event = "mcp_framework_loader_unconfigured",
            "mcp_client.include_default_paths=false with empty discovery_paths; \
             no MCP configuration sources are enabled. Set discovery_paths or \
             enable include_default_paths to discover remote MCP servers."
        );
        return DiscoveredFrameworkServers {
            search_paths: Vec::new(),
            servers: Vec::new(),
        };
    };
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
fn build_loader(settings: &McpClientConfig) -> Option<FrameworkConfigLoader> {
    let mut loader = if settings.include_default_paths {
        FrameworkConfigLoader::new()
    } else {
        let mut paths = settings.discovery_paths.iter();
        let first = paths.next()?;
        let mut loader = FrameworkConfigLoader::from_path(first.clone());
        for path in paths {
            loader = loader.with_path(path.clone());
        }
        return Some(loader);
    };

    for path in settings.discovery_paths.iter().rev() {
        loader = loader.with_priority_path(path.clone());
    }

    Some(loader)
}

#[cfg(all(test, feature = "mcp-client"))]
mod tests {
    use super::{McpClientContentItem, McpClientToolDefinition, discover_server_configs};
    use crate::config::McpClientConfig;
    use proptest::prelude::*;

    /// [ft-zfbqo] When mcp_client.include_default_paths=false AND
    /// discovery_paths is empty, discovery must:
    ///   1. Return zero discovered servers (no panic, no error).
    ///   2. Return zero search paths without consulting a placeholder.
    ///
    /// A predictable fake pathname is not equivalent to an empty source set:
    /// another local process could create that pathname between construction
    /// and loading and inject a server into explicitly disabled discovery.
    #[test]
    fn ft_zfbqo_unconfigured_loader_has_no_sources_on_any_platform() {
        let mut settings = McpClientConfig {
            include_default_paths: false,
            ..Default::default()
        };
        settings.discovery_paths.clear();

        let discovered = discover_server_configs(&settings);

        assert!(
            discovered.servers.is_empty(),
            "unconfigured loader must discover zero servers, got {:?}",
            discovered.servers
        );
        assert!(
            discovered.search_paths.is_empty(),
            "disabled discovery with no configured paths must expose no \
             filesystem source, got {:?}",
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
            // MCP-spec annotation shape: the framework boundary is typed to
            // the spec (`*Hint` keys, boolean `openWorldHint`), so the seam
            // guarantees lossless roundtrips for spec-conforming payloads
            // only. Non-spec payloads fail closed — see
            // tool_definition_rejects_non_spec_annotation_types.
            annotations: Some(serde_json::json!({
                "destructiveHint": true,
                "idempotentHint": false,
                "readOnlyHint": false,
                "openWorldHint": true
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
    fn tool_definition_rejects_non_spec_annotation_types() {
        // Per the MCP spec `openWorldHint` is a boolean. The framework seam
        // must not forward spec-violating annotation payloads to downstream
        // MCP participants; it fails closed with a typed protocol error.
        let err = McpClientToolDefinition {
            name: "echo".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: Some(serde_json::json!({
                "openWorldHint": "accepts arbitrary text"
            })),
        }
        .into_framework()
        .expect_err("non-spec annotation types should fail framework mapping");

        assert_eq!(err.code, "mcp_client.protocol");
        assert!(err.message.contains("remote tool annotations"));
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

    #[test]
    fn content_item_rejects_non_spec_image_shape() {
        // MCP-spec image content carries base64 `data` + `mimeType`; a
        // URL-style image is not a spec shape and must fail closed at the
        // framework boundary rather than being silently dropped or mis-mapped.
        let err = McpClientContentItem(serde_json::json!({
            "type": "image",
            "url": "https://example.com/picture.png",
        }))
        .into_framework()
        .expect_err("non-spec image content should fail framework mapping");

        assert_eq!(err.code, "mcp_client.protocol");
        assert!(err.message.contains("remote tool content"));
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
                    "destructiveHint": destructive,
                    "idempotentHint": !destructive,
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
            payload in "[A-Za-z0-9+/=]{1,32}",
        ) {
            let content = McpClientContentItem(serde_json::json!({
                "type": "image",
                "data": payload.clone(),
                "mimeType": "image/png",
            }));

            let framework = content.clone().into_framework().expect("into framework");
            let recovered = McpClientContentItem::from_framework(framework).expect("from framework");

            prop_assert_eq!(&recovered, &content);
            prop_assert_eq!(recovered.as_text(), None);
        }
    }
}
