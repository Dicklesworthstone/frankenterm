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
pub use fastmcp::testing::lab::TestClient as FrameworkTestClient;
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
#[allow(unused_imports)]
pub use fastmcp::{
    Budget as FrameworkBudget, Content as FrameworkContent, Cx as FrameworkCx,
    McpContext as FrameworkMcpContext, McpError as FrameworkMcpError,
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
    JsonRpcMessage as FrameworkJsonRpcMessage, Prompt as FrameworkPrompt,
    Resource as FrameworkResource, ResourceContent as FrameworkResourceContent,
    ResourceHandler as FrameworkResourceHandler, ResourceTemplate as FrameworkResourceTemplate,
    ServerCapabilities as FrameworkServerCapabilities, ServerInfo as FrameworkServerInfo,
    StdioTransport as FrameworkStdioTransport, ToolHandler as FrameworkToolHandler,
    Transport as FrameworkTransport, TransportError as FrameworkTransportError,
};

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub use fastmcp_server::{Server as FrameworkServer, ServerBuilder as FrameworkServerBuilder};

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

/// Select the protocol this server already advertised before FastMCP's
/// dual-era dispatcher. The initialize version is an offer: MCP 2024 permits
/// the server to reply with another supported version, which the peer can
/// accept or disconnect from. Malformed offers remain malformed.
#[cfg(feature = "mcp")]
fn preserve_legacy_request_negotiation(message: &mut FrameworkJsonRpcMessage) {
    let FrameworkJsonRpcMessage::Request(request) = message else {
        return;
    };
    if request.method == "initialized" {
        // The previous server explicitly accepted this legacy alias.
        request.method = "notifications/initialized".to_owned();
    }
    let Some(params) = request
        .params
        .as_mut()
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if request.method == "initialize"
        && params
            .get("protocolVersion")
            .is_some_and(serde_json::Value::is_string)
    {
        params.insert(
            "protocolVersion".to_owned(),
            serde_json::Value::String("2024-11-05".to_owned()),
        );
    }
    // These six final-era fields were ignored extensions in the pinned
    // 2024-only server. Do not let newly recognized reserved names change
    // the request era or reject an otherwise accepted legacy request. Retain
    // other metadata, arguments, top-level capabilities/client information,
    // and the request ID.
    if let Some(metadata) = params
        .get_mut("_meta")
        .and_then(serde_json::Value::as_object_mut)
    {
        for key in [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/logLevel",
            "io.modelcontextprotocol/serverInfo",
            "io.modelcontextprotocol/subscriptionId",
        ] {
            metadata.remove(key);
        }
    }
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
            let mut message = self.inner.recv(cx)?;
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
            if let FrameworkJsonRpcMessage::Request(request) = &message
                && request.method == "initialize"
                && request.validate().is_ok()
                && request
                    .params
                    .as_ref()
                    .and_then(|params| params.get("protocolVersion"))
                    .is_none_or(|version| !version.is_string())
            {
                // The old parameter parser returned -32602 and kept reading.
                // Reject this parse failure before the new era classifier can
                // close the connection. Notifications still receive no reply.
                if request.id.is_some() {
                    let response =
                        FrameworkJsonRpcMessage::Response(fastmcp::JsonRpcResponse::error(
                            request.id.clone(),
                            fastmcp::JsonRpcError {
                                code: (-32602).into(),
                                message: "initialize requires a string protocolVersion".to_owned(),
                                data: None,
                            },
                        ));
                    self.inner.send_with_delivery_ack(cx, &response)?;
                }
                continue;
            }
            preserve_legacy_request_negotiation(&mut message);
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
        let cx = crate::cx::for_request();
        self.run_transport_with_cx(&cx, transport)
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
        let cx = crate::cx::for_request();
        self.run_transport_returning_with_cx(&cx, transport);
    }

    /// Run with an explicit context until the transport closes, then return.
    pub fn run_transport_returning_with_cx<T>(self, cx: &crate::cx::Cx, transport: T)
    where
        T: FrameworkDeliveryAcknowledgingTransport + Send + 'static,
    {
        if let Err(error) = self.inner.run_transport_returning_with_cx(
            cx,
            FrameworkDeliveryAwareTransport::new(transport, self.coordinator),
        ) {
            tracing::warn!(
                target: "ft::mcp_framework",
                code = ?error.code,
                "MCP server transport reported an error"
            );
        }
    }
}

#[cfg(feature = "mcp")]
#[allow(unused_imports)]
pub(crate) fn framework_server_builder(name: &str, version: &str) -> FrameworkServerBuilder {
    FrameworkServer::new(name, version)
        .protocol_policy(fastmcp::protocol_policy::ProtocolPolicy::LegacyOnly)
        .expect("the FastMCP legacy-2024-11-05 feature is enabled")
        .legacy_application_tool_content(true)
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
    /// This records the application's millisecond value before it becomes a
    /// framework response-timeout policy, so:
    ///
    ///   1. Operators inspecting an `OutboundFrameworkClient` instance
    ///      can see the timeout the wrapper is enforcing without
    ///      cross-referencing the originating `McpClientConfig`.
    ///   2. Future upstream support for caller-specific deadline propagation
    ///      (tracked under ft-bd3vr) has a stable wrapper-side field to bind
    ///      against.
    ///   3. Diagnostics do not mistake configuration for a proven wall-clock
    ///      bound across every supported platform and synchronous transport.
    configured_response_timeout_ms: u64,
    connection_cx: crate::cx::Cx,
    // Field order keeps the runtime alive while Client::drop settles its
    // transport and subprocess. A connection may outlive its connect caller.
    _runtime: asupersync::runtime::Runtime,
}

#[cfg(feature = "mcp-client")]
pub(crate) enum OutboundFrameworkError {
    Transport(FrameworkMcpError),
    Mapping(McpClientError),
}

// Retain the application's previous tool-result vocabulary. FastMCP's exact
// 2024 convenience decoder excludes audio and resources without a payload,
// both of which the previously pinned client accepted. These private shapes
// keep its serde field/optional/unknown-member behavior without changing the
// public DTO or the framework's connection/JSON-RPC admission machinery.
#[cfg(feature = "mcp-client")]
#[derive(serde::Deserialize)]
struct LegacyClientToolResult {
    content: Vec<LegacyClientContent>,
    #[serde(rename = "isError", default)]
    is_error: bool,
}

#[cfg(feature = "mcp-client")]
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum LegacyClientContent {
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: LegacyClientResourceContent,
    },
}

#[cfg(feature = "mcp-client")]
#[derive(serde::Deserialize)]
struct LegacyClientResourceContent {
    uri: String,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    text: Option<String>,
    blob: Option<String>,
}

#[cfg(feature = "mcp-client")]
fn map_legacy_client_content(
    content: LegacyClientContent,
) -> Result<McpClientContentItem, McpClientError> {
    let content = match content {
        LegacyClientContent::Text { text } => FrameworkContent::Text { text },
        LegacyClientContent::Image { data, mime_type } => {
            FrameworkContent::Image { data, mime_type }
        }
        LegacyClientContent::Audio { data, mime_type } => {
            FrameworkContent::Audio { data, mime_type }
        }
        LegacyClientContent::Resource { resource } => FrameworkContent::Resource {
            resource: fastmcp::ResourceContent {
                uri: resource.uri,
                mime_type: resource.mime_type,
                text: resource.text,
                blob: resource.blob,
            },
        },
    };
    McpClientContentItem::from_framework(content)
}

#[cfg(feature = "mcp-client")]
fn map_legacy_tool_result(
    result: serde_json::Value,
) -> Result<Vec<McpClientContentItem>, OutboundFrameworkError> {
    let result: LegacyClientToolResult = serde_json::from_value(result).map_err(|error| {
        OutboundFrameworkError::Transport(FrameworkMcpError::internal_error(format!(
            "Failed to deserialize response: {error}"
        )))
    })?;
    if result.is_error {
        let message = result
            .content
            .first()
            .and_then(|content| match content {
                LegacyClientContent::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "Tool execution failed".to_owned());
        return Err(OutboundFrameworkError::Transport(
            FrameworkMcpError::tool_error(message),
        ));
    }
    result
        .content
        .into_iter()
        .map(map_legacy_client_content)
        .collect::<Result<Vec<_>, _>>()
        .map_err(OutboundFrameworkError::Mapping)
}

#[cfg(feature = "mcp-client")]
impl OutboundFrameworkClient {
    pub(crate) fn connect_stdio(
        server: &ExternalServerConfig,
        settings: &McpClientConfig,
    ) -> Result<Self, FrameworkMcpError> {
        let mut builder = FrameworkClientBuilder::new()
            .client_info("frankenterm-mcp-client", env!("CARGO_PKG_VERSION"))
            // The prior client started with the 2024 initialize handshake.
            // Preserve that startup instead of probing modern MCP first.
            .protocol_plan(fastmcp::ClientProtocolPlan::stdio(
                fastmcp::protocol_policy::ProtocolPolicy::LegacyOnly,
            ))
            .request_timeout_policy(fastmcp::RequestTimeoutPolicy::from_application_timeout_ms(
                settings.timeout_ms,
            )?)
            .application_retry_config(settings.max_retries, settings.retry_delay_ms);

        if let Some(cwd) = server.cwd.as_ref() {
            builder = builder.working_dir(cwd);
        }
        if !server.env.is_empty() {
            builder = builder.envs(server.env.clone());
        }

        let args_ref: Vec<&str> = server.args.iter().map(String::as_str).collect();
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .build()
            .map_err(|_| FrameworkMcpError::internal_error("MCP client runtime creation failed"))?;
        // Raw Asupersync block_on restores both its runtime handle and Cx on
        // return. The application CompatRuntime intentionally retains its TLS
        // handle, so it cannot serve as a temporary nested connection driver.
        let (client, connection_cx) = runtime.block_on(async {
            let cx = crate::cx::Cx::current().ok_or_else(|| {
                FrameworkMcpError::internal_error("MCP client runtime context is unavailable")
            })?;
            let client = builder
                .connect_stdio_with_cx(&server.command, &args_ref, &cx)
                .await?;
            Ok::<_, FrameworkMcpError>((client, cx))
        })?;
        Ok(Self {
            inner: client,
            configured_response_timeout_ms: settings.timeout_ms,
            connection_cx,
            _runtime: runtime,
        })
    }

    /// Response-timeout value configured on the wrapped `FrameworkClient`.
    /// [ft-bd3vr]
    ///
    /// **CONTRACT**: this is diagnostic configuration, not a proven wall-clock
    /// upper bound. FastMCP's synchronous pipe-read deadline support differs
    /// by platform. This wrapper retains its synchronous list/call APIs and
    /// connection context; it does not yet pass each caller's budget through
    /// FastMCP's separate Cx-aware request methods.
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
    /// request-timeout setting. Per-call deadline propagation from a caller's
    /// `Cx` budget is not enforced at this layer (ft-bd3vr). The proxy layer at
    /// `mcp_proxy::RemoteProxyToolHandler::call` performs a Cx
    /// pre-flight checkpoint (br-ft-xhj38) so PRE-EXPIRED callers
    /// short-circuit before reaching this point. Wiring later cancellation
    /// requires a separate change to the application's synchronous boundary.
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
        // Keep the negotiated client as the sole ingress/correlation owner.
        // Only the application content decoder differs from the framework's
        // exact-2024 convenience method; initialization, timeout, cancellation,
        // JSON-RPC validation, and transport cleanup remain connection-owned.
        let mut execution = self
            .inner
            .start_yielding_stdio_request(
                "tools/call",
                Some(serde_json::json!({"name": name, "arguments": arguments})),
            )
            .map_err(OutboundFrameworkError::Transport)?;
        let response = self
            .inner
            .wait_multiplexed_request(&self.connection_cx, &mut execution)
            .map_err(OutboundFrameworkError::Transport)?;
        let result = response.result.ok_or_else(|| {
            OutboundFrameworkError::Transport(FrameworkMcpError::internal_error(
                "No result in response",
            ))
        })?;
        map_legacy_tool_result(result)
    }

    /// br-ft-dnzum: gracefully terminate the stdio connection.
    ///
    /// Consumes the application wrapper, closes the framework transport and
    /// reaps its subprocess while the connection's runtime is still alive.
    /// The existing unit-returning API is retained; newly reported framework
    /// cleanup errors are logged by code without exposing peer-controlled text.
    pub(crate) fn shutdown(mut self) {
        if let Err(error) = self.inner.close() {
            tracing::warn!(
                target: "ft::mcp_framework",
                code = ?error.code,
                "MCP client cleanup reported an error"
            );
        }
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
    // The previous loader ignored unreadable or malformed sources and kept
    // merging later valid files. Preserve that application contract instead
    // of discarding every server when the new fallible load_all encounters
    // one bad source.
    let mut merged = fastmcp::mcp_config::McpConfig::new();
    for path in loader.search_paths() {
        if path.exists() {
            match fastmcp::mcp_config::McpConfig::from_file(path) {
                Ok(config) => merged.merge(config),
                Err(_) => tracing::warn!(
                    target: "ft::mcp_framework",
                    event = "mcp_framework_discovery_source_skipped",
                    "MCP discovery skipped an unreadable or malformed configuration source"
                ),
            }
        }
    }

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

#[cfg(all(test, feature = "mcp"))]
mod server_compat_tests {
    use super::*;
    use serde_json::{Value, json};

    fn initialize_message(version: Value) -> FrameworkJsonRpcMessage {
        FrameworkJsonRpcMessage::Request(fastmcp::JsonRpcRequest::new(
            "initialize",
            Some(json!({
                "protocolVersion": version,
                "clientInfo": {"name": "legacy-offer-client", "version": "1", "extra": "kept"},
                "capabilities": {"roots": {"listChanged": true}},
                "_meta": {
                    "caller-note": "kept",
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {"name": "ignored"},
                    "io.modelcontextprotocol/logLevel": "debug",
                    "io.modelcontextprotocol/serverInfo": {},
                    "io.modelcontextprotocol/subscriptionId": "ignored"
                },
                "unknown-option": true
            })),
            "initialize-id",
        ))
    }

    #[test]
    fn legacy_offer_selection_preserves_request_identity_and_extensions() {
        for version in ["2024-11-05", "2025-03-26", "2023-01-01"] {
            let mut message = initialize_message(json!(version));
            let original = serde_json::to_value(&message).expect("original request");
            preserve_legacy_request_negotiation(&mut message);
            let selected = serde_json::to_value(&message).expect("selected request");
            assert_eq!(selected["params"]["protocolVersion"], "2024-11-05");
            for field in ["id", "method", "jsonrpc"] {
                assert_eq!(selected[field], original[field]);
            }
            for field in ["clientInfo", "capabilities", "unknown-option"] {
                assert_eq!(selected["params"][field], original["params"][field]);
            }
            assert_eq!(selected["params"]["_meta"], json!({"caller-note": "kept"}));
        }
    }

    #[test]
    fn malformed_initialize_offers_are_not_repaired() {
        for version in [Value::Null, json!(42), json!({}), json!([])] {
            let mut message = initialize_message(version.clone());
            preserve_legacy_request_negotiation(&mut message);
            let selected = serde_json::to_value(message).expect("malformed request");
            assert_eq!(selected["params"]["protocolVersion"], version);
        }
        let mut no_params =
            FrameworkJsonRpcMessage::Request(fastmcp::JsonRpcRequest::new("initialize", None, 3));
        let original = serde_json::to_value(&no_params).expect("request without params");
        preserve_legacy_request_negotiation(&mut no_params);
        assert_eq!(serde_json::to_value(no_params).unwrap(), original);
    }

    fn with_bounded_server(
        exercise: impl FnOnce(&mut fastmcp::memory::MemoryTransport, &crate::cx::Cx) + Send + 'static,
    ) {
        let inner = framework_server_builder("term-compat", "1").build();
        assert_eq!(
            inner.protocol_policy(),
            fastmcp::protocol_policy::ProtocolPolicy::LegacyOnly
        );
        let server = FrameworkDeliveryServer::new(
            inner,
            Arc::new(FrameworkResponseDeliveryCoordinator::default()),
        );
        let (mut client, transport) = framework_create_memory_transport_pair();
        let cx = crate::cx::for_testing();
        let server_cx = cx.clone();
        let server_worker = std::thread::spawn(move || {
            server.run_transport_returning_with_cx(&server_cx, transport);
        });
        let operation_cx = cx.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let operation = std::thread::spawn(move || {
            exercise(&mut client, &operation_cx);
            client.close().expect("close client transport");
            server_worker.join().expect("server loop returns on EOF");
            done_tx.send(()).expect("completion observer is present");
        });
        match done_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(()) => operation.join().expect("operation completes"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                operation.join().expect("operation must not panic");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                cx.set_cancel_requested(true);
                panic!("MCP operation/transport cleanup exceeded 15 seconds");
            }
        }
    }

    #[test]
    fn malformed_initialize_keeps_invalid_params_and_connection_recovery() {
        with_bounded_server(|client, cx| {
            for version in [Value::Null, json!(42), json!({}), json!([])] {
                client.send(cx, &initialize_message(version)).unwrap();
                let response = serde_json::to_value(client.recv(cx).expect("parse-error response"))
                    .expect("response JSON");
                assert_eq!(response["id"], "initialize-id");
                assert_eq!(response["error"]["code"], -32602);
                assert!(response.get("result").is_none_or(Value::is_null));
            }
            let missing_params = FrameworkJsonRpcMessage::Request(fastmcp::JsonRpcRequest::new(
                "initialize",
                None,
                9,
            ));
            client.send(cx, &missing_params).unwrap();
            let response = serde_json::to_value(client.recv(cx).expect("missing-params response"))
                .expect("response JSON");
            assert_eq!(response["id"], 9);
            assert_eq!(response["error"]["code"], -32602);

            let mut notification = initialize_message(Value::Null);
            if let FrameworkJsonRpcMessage::Request(request) = &mut notification {
                request.id = None;
            }
            client.send(cx, &notification).unwrap();
            client
                .send(cx, &initialize_message(json!("2025-03-26")))
                .unwrap();
            // No notification reply may precede the correlated valid response.
            let response = serde_json::to_value(client.recv(cx).expect("recovered initialization"))
                .expect("response JSON");
            assert_eq!(response["id"], "initialize-id");
            assert!(response.get("error").is_none_or(Value::is_null));
            assert_eq!(response["result"]["protocolVersion"], "2024-11-05");

            let mut initialized = fastmcp::JsonRpcRequest::initialized_notification();
            initialized.method = "initialized".to_owned();
            client
                .send(cx, &FrameworkJsonRpcMessage::Request(initialized))
                .unwrap();
            client
                .send(
                    cx,
                    &FrameworkJsonRpcMessage::Request(fastmcp::JsonRpcRequest::new(
                        "ping", None, 10,
                    )),
                )
                .unwrap();
            let response = serde_json::to_value(client.recv(cx).expect("ping after legacy alias"))
                .expect("response JSON");
            assert_eq!(response["id"], 10);
            assert!(response.get("error").is_none_or(Value::is_null));
            assert_eq!(response["result"], json!({}));
        });
    }

    #[test]
    fn server_negotiates_old_and_new_offers_and_processes_legacy_requests_in_order() {
        for version in ["2024-11-05", "2025-03-26", "2023-01-01"] {
            with_bounded_server(move |client, cx| {
                client
                    .send(cx, &initialize_message(json!(version)))
                    .unwrap();
                let response = serde_json::to_value(client.recv(cx).expect("initialize response"))
                    .expect("response JSON");
                assert_eq!(response["id"], "initialize-id");
                assert!(
                    response.get("error").is_none_or(Value::is_null),
                    "{response}"
                );
                assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
                client
                    .send(
                        cx,
                        &FrameworkJsonRpcMessage::Request(
                            fastmcp::JsonRpcRequest::initialized_notification(),
                        ),
                    )
                    .unwrap();
                for id in [11, 12] {
                    client
                        .send(
                            cx,
                            &FrameworkJsonRpcMessage::Request(fastmcp::JsonRpcRequest::new(
                                "ping",
                                Some(json!({"_meta": {
                                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                                    "caller-note": "kept"
                                }})),
                                id,
                            )),
                        )
                        .unwrap();
                }
                for id in [11, 12] {
                    let response = serde_json::to_value(client.recv(cx).expect("ping response"))
                        .expect("response JSON");
                    assert_eq!(response["id"], id);
                    assert!(
                        response.get("error").is_none_or(Value::is_null),
                        "{response}"
                    );
                    assert_eq!(response["result"], json!({}));
                }
            });
        }
    }
}

#[cfg(all(test, feature = "mcp-client"))]
mod tests {
    use super::{
        McpClientContentItem, McpClientToolDefinition, discover_server_configs,
        map_legacy_client_content, map_legacy_tool_result,
    };
    use crate::config::McpClientConfig;
    use proptest::prelude::*;

    #[test]
    fn legacy_content_preserves_existing_client_and_proxy_projection() {
        use serde_json::json;

        for expected in [
            json!({"type": "text", "text": "tool result"}),
            json!({"type": "image", "data": "aW1hZ2U=", "mimeType": "image/png"}),
            json!({"type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav"}),
            json!({"type": "resource", "resource": {
                "uri": "file:///text", "text": "resource text", "mimeType": "text/plain"
            }}),
            json!({"type": "resource", "resource": {
                "uri": "file:///bytes", "blob": "Ynl0ZXM=", "mimeType": "application/octet-stream"
            }}),
            json!({"type": "resource", "resource": {
                "uri": "file:///both", "text": "resource text", "blob": "Ynl0ZXM="
            }}),
            json!({"type": "resource", "resource": {"uri": "file:///empty"}}),
        ] {
            let mut legacy_wire = expected.clone();
            legacy_wire["annotations"] = json!({"audience": ["user"], "priority": 0.3});
            legacy_wire["_meta"] = json!({"previously-ignored": true});
            if let Some(resource) = legacy_wire.get_mut("resource") {
                resource["_meta"] = json!({"previously-ignored-resource": true});
            }
            let result = map_legacy_tool_result(json!({
                "content": [legacy_wire], "previously-ignored-result": true
            }))
            .unwrap_or_else(|_| panic!("previously accepted tool-result shape"));
            let neutral = result.into_iter().next().expect("one content item");
            assert_eq!(neutral.0, expected);
            let proxy = neutral.into_framework().expect("existing proxy conversion");
            assert_eq!(serde_json::to_value(proxy).unwrap(), expected);
        }
    }

    #[test]
    fn previous_content_shapes_survive_framework_result_admission() {
        use fastmcp::Transport;
        use serde_json::json;

        // Exercise the same source-frame/result admission used by the live
        // Client's sole ingress reader. This preselected in-memory fixture
        // covers result bodies; the CLI test covers the real stdio handshake.
        let (transport, mut peer) = fastmcp::memory::create_memory_transport_pair();
        let executor = fastmcp::RequestExecutor::with_protocol_era(
            transport,
            fastmcp::ProtocolEra::Legacy2024,
        );
        let cx = crate::cx::for_testing();
        let mut execution = executor
            .execute(
                &cx,
                fastmcp::JsonRpcRequest::new(
                    "tools/call",
                    Some(json!({"name": "content", "arguments": {}})),
                    7,
                ),
            )
            .expect("commit owned request");
        let request = serde_json::to_value(peer.recv(&cx).expect("committed request")).unwrap();
        assert_eq!(request["id"], 7);
        assert_eq!(request["method"], "tools/call");
        let content = json!([
            {"type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav"},
            {"type": "resource", "resource": {"uri": "file:///empty"}},
            {"type": "resource", "resource": {
                "uri": "file:///both", "text": "text", "blob": "Ynl0ZXM="
            }}
        ]);
        let frame = fastmcp::ReceivedTransportFrame::admit(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "id": request["id"], "result": {"content": content}
            }))
            .unwrap(),
        )
        .expect("admit source frame");
        executor
            .drive_frame(&cx, frame)
            .expect("route owned result");
        let response = executor
            .try_take_response(&mut execution)
            .expect("response admission")
            .expect("committed response completes immediately");
        let mapped = map_legacy_tool_result(response.result.expect("result body"))
            .unwrap_or_else(|_| panic!("previously accepted content survives ingress"));
        assert_eq!(
            mapped.into_iter().map(|item| item.0).collect::<Vec<_>>(),
            content.as_array().unwrap().clone()
        );
        peer.close().expect("close peer");
    }

    #[test]
    fn legacy_resource_projection_keeps_optional_payload_validation() {
        use serde_json::json;

        for (field, ordinary) in [
            ("blob", json!({"uri": "file:///text", "text": "text"})),
            ("text", json!({"uri": "file:///blob", "blob": "Ynl0ZXM="})),
        ] {
            for invalid in [json!(42), json!({}), json!([])] {
                let mut resource = ordinary.clone();
                resource[field] = invalid;
                assert!(matches!(
                    map_legacy_tool_result(json!({"content": [{
                        "type": "resource", "resource": resource
                    }]})),
                    Err(super::OutboundFrameworkError::Transport(error))
                        if error.code == super::FrameworkMcpErrorCode::InternalError
                ));
            }
            let mut resource = ordinary.clone();
            resource[field] = serde_json::Value::Null;
            let legacy = serde_json::from_value(json!({
                "type": "resource", "resource": resource
            }))
            .expect("legacy optional null field");
            let mapped = map_legacy_client_content(legacy).expect("null remains absent");
            assert_eq!(mapped.0, json!({"type": "resource", "resource": ordinary}));
        }
    }

    #[test]
    fn legacy_tool_result_preserves_required_fields_and_tool_errors() {
        use serde_json::json;

        for result in [
            json!({}),
            json!({"content": null}),
            json!({"content": [], "isError": null}),
            json!({"content": [{"type": "text", "text": 42}]}),
            json!({"content": [{"type": "image", "data": "aW1hZ2U="}]}),
            json!({"content": [{"type": "audio", "data": "YXVkaW8=", "mimeType": 42}]}),
            json!({"content": [{"type": "resource", "resource": {"text": "missing URI"}}]}),
            json!({"content": [{"type": "new-unknown-content"}]}),
        ] {
            assert!(matches!(
                map_legacy_tool_result(result),
                Err(super::OutboundFrameworkError::Transport(error))
                    if error.code == super::FrameworkMcpErrorCode::InternalError
            ));
        }
        for (content, expected) in [
            (
                json!([{ "type": "text", "text": "tool failed" }]),
                "tool failed",
            ),
            (json!([]), "Tool execution failed"),
            (
                json!([{ "type": "audio", "data": "YXVkaW8=", "mimeType": "audio/wav" }]),
                "Tool execution failed",
            ),
        ] {
            assert!(matches!(
                map_legacy_tool_result(json!({"content": content, "isError": true})),
                Err(super::OutboundFrameworkError::Transport(error))
                    if error.code == super::FrameworkMcpErrorCode::ToolExecutionError
                        && error.message == expected
            ));
        }
    }

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
    fn discovery_keeps_valid_sources_and_later_file_precedence() {
        let root = tempfile::tempdir()
            .expect("discovery fixture directory")
            .keep();
        eprintln!("retained MCP discovery fixture: {}", root.display());
        let first = root.join("first.json");
        let malformed = root.join("malformed.json");
        let missing = root.join("missing.json");
        let last = root.join("last.json");
        std::fs::write(
            &first,
            r#"{"mcpServers":{"shared":{"command":"first"},"unique":{"command":"keep"}}}"#,
        )
        .expect("first valid source");
        std::fs::write(&malformed, "{invalid JSON").expect("malformed source");
        std::fs::write(
            &last,
            r#"{"mcpServers":{"shared":{"command":"last","disabled":true},"added":{"command":"new"}}}"#,
        )
        .expect("later valid source");
        let paths = vec![first, malformed, missing, last];
        let settings = McpClientConfig {
            include_default_paths: false,
            discovery_paths: paths.clone(),
            ..Default::default()
        };

        let discovered = discover_server_configs(&settings);
        assert_eq!(
            discovered.search_paths,
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            discovered
                .servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["added", "shared", "unique"]
        );
        let shared = discovered
            .servers
            .iter()
            .find(|server| server.name == "shared")
            .expect("shared server retained");
        assert_eq!(shared.command, "last");
        assert!(shared.disabled);
        let unique = discovered
            .servers
            .iter()
            .find(|server| server.name == "unique")
            .expect("earlier unique server retained");
        assert_eq!(unique.command, "keep");
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
