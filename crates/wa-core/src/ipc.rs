//! IPC module for watcher daemon communication.
//!
//! Provides Unix domain socket communication between CLI commands and the
//! watcher daemon. Used primarily for delivering user-var events from
//! shell hooks to the running watcher.
//!
//! # Protocol
//!
//! The protocol uses JSON lines (newline-delimited JSON):
//! - Client sends: `{"type":"user_var","pane_id":1,"name":"WA_EVENT","value":"base64..."}\n`
//! - Server responds: `{"ok":true}\n` or `{"ok":false,"error":"..."}\n`

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, mpsc};

use crate::config::{IpcAuthToken, IpcScope};
use crate::crash::HealthSnapshot;
use crate::events::{Event, EventBus, UserVarError, UserVarPayload};
use crate::ingest::PaneRegistry;

/// Default IPC socket filename relative to workspace .wa directory.
pub const IPC_SOCKET_NAME: &str = "ipc.sock";

/// Maximum message size in bytes (128KB).
pub const MAX_MESSAGE_SIZE: usize = 131_072;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// NOTE: StatusUpdate types (CursorPosition, PaneDimensions, StatusUpdate, StatusUpdateRateLimiter)
// were removed in v0.2.0 to eliminate Lua performance bottleneck.
// Alt-screen detection is now handled via escape sequence parsing (see screen_state.rs).
// Pane metadata (title, dimensions, cursor) is obtained via `wezterm cli list`.

/// Request message from client to server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    /// User-var event from shell hook
    UserVar {
        /// Pane ID that emitted the user-var
        pane_id: u64,
        /// Variable name (e.g., "WA_EVENT")
        name: String,
        /// Raw value (typically base64-encoded JSON)
        value: String,
    },
    // NOTE: StatusUpdate variant was removed in v0.2.0 (Lua performance optimization)
    /// Ping to check if watcher is alive
    Ping,
    /// Request current watcher status
    Status,
    /// Request pane state from watcher registry
    PaneState {
        /// Pane ID to inspect
        pane_id: u64,
    },
    /// Set a runtime pane capture priority override (watcher only).
    SetPanePriority {
        /// Pane ID to modify
        pane_id: u64,
        /// Priority value (lower = higher priority)
        priority: u32,
        /// Optional TTL in milliseconds (0 or None = until cleared)
        ttl_ms: Option<u64>,
    },
    /// Clear any runtime pane capture priority override (watcher only).
    ClearPanePriority {
        /// Pane ID to modify
        pane_id: u64,
    },
    /// RPC request forwarded to robot handlers.
    Rpc {
        /// Robot command arguments (e.g., ["state"] or ["send", "1", "ls"]).
        args: Vec<String>,
    },
}

impl IpcRequest {
    #[must_use]
    fn required_scope(&self) -> IpcScope {
        match self {
            Self::UserVar { .. } => IpcScope::Write,
            Self::Ping | Self::Status | Self::PaneState { .. } => IpcScope::Read,
            Self::SetPanePriority { .. } | Self::ClearPanePriority { .. } => IpcScope::Write,
            Self::Rpc { args } => rpc_required_scope(args),
        }
    }
}

fn rpc_required_scope(args: &[String]) -> IpcScope {
    let Some(cmd) = args.first().map(String::as_str) else {
        return IpcScope::Write;
    };

    match cmd {
        "send" | "approve" => IpcScope::Write,
        "workflow" => match args.get(1).map(String::as_str) {
            Some("run" | "abort") => IpcScope::Write,
            _ => IpcScope::Read,
        },
        "accounts" => match args.get(1).map(String::as_str) {
            Some("refresh") => IpcScope::Write,
            _ => IpcScope::Read,
        },
        "reservations" => match args.get(1).map(String::as_str) {
            Some("reserve" | "release") => IpcScope::Write,
            _ => IpcScope::Read,
        },
        _ => IpcScope::Read,
    }
}

/// Response message from server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    /// Whether the request succeeded
    pub ok: bool,
    /// Error message if failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Stable error code for machine parsing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Optional hint for recovery
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Additional data (for status requests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Elapsed time to handle the request (ms)
    pub elapsed_ms: u64,
    /// wa version
    pub version: String,
    /// Server timestamp (epoch ms)
    pub now: u64,
}

impl IpcResponse {
    /// Create a success response.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            error_code: None,
            hint: None,
            data: None,
            elapsed_ms: 0,
            version: crate::VERSION.to_string(),
            now: now_ms(),
        }
    }

    /// Create a success response with data.
    #[must_use]
    pub fn ok_with_data(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            error_code: None,
            hint: None,
            data: Some(data),
            elapsed_ms: 0,
            version: crate::VERSION.to_string(),
            now: now_ms(),
        }
    }

    /// Create an error response.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            error_code: None,
            hint: None,
            data: None,
            elapsed_ms: 0,
            version: crate::VERSION.to_string(),
            now: now_ms(),
        }
    }

    /// Create an error response with a stable error code.
    #[must_use]
    pub fn error_with_code(
        code: impl Into<String>,
        message: impl Into<String>,
        hint: Option<String>,
    ) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            error_code: Some(code.into()),
            hint,
            data: None,
            elapsed_ms: 0,
            version: crate::VERSION.to_string(),
            now: now_ms(),
        }
    }

    fn with_timing(mut self, start: Instant) -> Self {
        self.elapsed_ms = elapsed_ms(start);
        self.now = now_ms();
        self
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IpcEnvelope {
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
    #[serde(flatten)]
    request: IpcRequest,
}

pub struct IpcRpcRequest {
    pub args: Vec<String>,
    pub request_id: Option<String>,
}

pub type IpcRpcHandler = Arc<
    dyn Fn(
            IpcRpcRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = IpcResponse> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub struct IpcAuth {
    tokens: Vec<IpcAuthToken>,
}

impl IpcAuth {
    #[must_use]
    pub fn new(tokens: Vec<IpcAuthToken>) -> Self {
        Self { tokens }
    }

    fn authorize(&self, token: Option<&str>, required: IpcScope) -> Result<(), IpcAuthError> {
        if self.tokens.is_empty() {
            return Ok(());
        }

        let token = token.ok_or(IpcAuthError::MissingToken)?;
        let record = self
            .tokens
            .iter()
            .find(|candidate| candidate.token == token)
            .ok_or(IpcAuthError::InvalidToken)?;

        if let Some(expires_at) = record.expires_at_ms {
            if now_ms() >= expires_at {
                return Err(IpcAuthError::ExpiredToken);
            }
        }

        let default_scopes = [IpcScope::All];
        let scopes = if record.scopes.is_empty() {
            &default_scopes[..]
        } else {
            record.scopes.as_slice()
        };

        if scopes.iter().any(|scope| scope.allows(required)) {
            Ok(())
        } else {
            Err(IpcAuthError::InsufficientScope { required })
        }
    }
}

#[derive(Debug)]
// NOTE: Reserved for IPC auth enforcement (bd-3p06).
#[allow(dead_code)]
enum IpcAuthError {
    MissingToken,
    InvalidToken,
    ExpiredToken,
    InsufficientScope { required: IpcScope },
}

#[allow(dead_code)] // Reserved for IPC auth enforcement (bd-3p06).
impl IpcAuthError {
    fn message(&self) -> String {
        match self {
            Self::MissingToken => "missing auth token".to_string(),
            Self::InvalidToken => "invalid auth token".to_string(),
            Self::ExpiredToken => "auth token expired".to_string(),
            Self::InsufficientScope { required } => {
                format!("insufficient scope (requires {required:?})")
            }
        }
    }
}

/// Context shared by all IPC request handlers.
///
/// This struct holds references to system components needed for handling
/// various IPC request types.
pub struct IpcHandlerContext {
    /// Event bus for publishing events
    pub event_bus: Arc<EventBus>,
    /// Pane registry for pane state queries (optional for backward compatibility)
    pub registry: Option<Arc<RwLock<PaneRegistry>>>,
    /// Optional IPC auth configuration
    pub auth: Option<IpcAuth>,
    /// Optional RPC handler (robot/MCP parity).
    pub rpc_handler: Option<IpcRpcHandler>,
    // NOTE: rate_limiter field was removed in v0.2.0 (StatusUpdate removed)
}

impl IpcHandlerContext {
    /// Create a new handler context with just an event bus (backward compatible).
    #[must_use]
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            event_bus,
            registry: None,
            auth: None,
            rpc_handler: None,
        }
    }

    /// Create a new handler context with pane registry support.
    #[must_use]
    pub fn with_registry(event_bus: Arc<EventBus>, registry: Arc<RwLock<PaneRegistry>>) -> Self {
        Self {
            event_bus,
            registry: Some(registry),
            auth: None,
            rpc_handler: None,
        }
    }

    /// Create a new handler context with optional auth configuration.
    #[must_use]
    pub fn with_auth(
        event_bus: Arc<EventBus>,
        registry: Option<Arc<RwLock<PaneRegistry>>>,
        auth: Option<IpcAuth>,
    ) -> Self {
        Self {
            event_bus,
            registry,
            auth,
            rpc_handler: None,
        }
    }

    /// Create a new handler context with optional auth and RPC handler.
    #[must_use]
    pub fn with_auth_and_rpc(
        event_bus: Arc<EventBus>,
        registry: Option<Arc<RwLock<PaneRegistry>>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
    ) -> Self {
        Self {
            event_bus,
            registry,
            auth,
            rpc_handler,
        }
    }
}

/// IPC server that runs in the watcher daemon.
#[cfg(unix)]
pub struct IpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
}

#[cfg(unix)]
impl IpcServer {
    /// Create and bind a new IPC server with default permissions (0o600).
    ///
    /// # Arguments
    /// * `socket_path` - Path to the Unix socket file
    ///
    /// # Errors
    /// Returns error if socket binding fails.
    pub async fn bind(socket_path: impl AsRef<Path>) -> std::io::Result<Self> {
        Self::bind_with_permissions(socket_path, Some(0o600)).await
    }

    /// Create and bind a new IPC server with explicit permissions.
    ///
    /// # Arguments
    /// * `socket_path` - Path to the Unix socket file
    /// * `permissions` - Optional permissions to set on the socket path
    ///
    /// # Errors
    /// Returns error if socket binding or permission setting fails.
    pub async fn bind_with_permissions(
        socket_path: impl AsRef<Path>,
        permissions: Option<u32>,
    ) -> std::io::Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();

        // Remove stale socket file if it exists
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        // Create parent directory if needed
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&socket_path)?;
        if let Some(mode) = permissions {
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&socket_path, perms)?;
        }
        tracing::info!(path = %socket_path.display(), "IPC server listening");

        Ok(Self {
            socket_path,
            listener,
        })
    }

    /// Get the socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the IPC server, forwarding events to the event bus.
    ///
    /// This spawns a task for each connection. Returns when the shutdown
    /// signal is received.
    ///
    /// # Arguments
    /// * `event_bus` - Event bus to publish received events
    /// * `shutdown_rx` - Channel to receive shutdown signal
    pub async fn run(self, event_bus: Arc<EventBus>, shutdown_rx: mpsc::Receiver<()>) {
        self.run_with_auth(event_bus, None, shutdown_rx).await;
    }

    /// Run the IPC server with full handler context (including pane registry).
    ///
    /// This version supports status update handling with pane registry access.
    ///
    /// # Arguments
    /// * `event_bus` - Event bus to publish received events
    /// * `registry` - Pane registry for status update handling
    /// * `shutdown_rx` - Channel to receive shutdown signal
    pub async fn run_with_registry(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        self.run_with_registry_and_auth(event_bus, registry, None, shutdown_rx)
            .await;
    }

    /// Run the IPC server with optional auth configuration.
    pub async fn run_with_auth(
        self,
        event_bus: Arc<EventBus>,
        auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        let ctx = Arc::new(IpcHandlerContext::with_auth(event_bus, None, auth));
        self.run_with_context(ctx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with registry and optional auth configuration.
    pub async fn run_with_registry_and_auth(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        let ctx = Arc::new(IpcHandlerContext::with_auth(
            event_bus,
            Some(registry),
            auth,
        ));
        self.run_with_context(ctx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with registry, auth, and RPC handler.
    pub async fn run_with_registry_auth_and_rpc(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        let ctx = Arc::new(IpcHandlerContext::with_auth_and_rpc(
            event_bus,
            Some(registry),
            auth,
            rpc_handler,
        ));
        self.run_with_context(ctx, &mut shutdown_rx).await;
    }

    /// Internal run method with context.
    async fn run_with_context(
        self,
        ctx: Arc<IpcHandlerContext>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) {
        loop {
            tokio::select! {
                result = self.listener.accept() => {
                    match result {
                        Ok((stream, _addr)) => {
                            let ctx = ctx.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_client_with_context(stream, ctx).await {
                                    tracing::warn!(error = %e, "IPC client error");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to accept IPC connection");
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    tracing::info!("IPC server shutting down");
                    break;
                }
            }
        }

        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(not(unix))]
pub struct IpcServer {
    socket_path: PathBuf,
}

#[cfg(not(unix))]
impl IpcServer {
    /// Create and bind a new IPC server.
    ///
    /// # Errors
    /// Returns error on non-unix platforms (IPC sockets are unix-only).
    pub async fn bind(socket_path: impl AsRef<Path>) -> std::io::Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!(
                "IPC sockets are only supported on unix platforms (socket: {})",
                socket_path.display()
            ),
        ))
    }

    /// Get the socket path.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Run the IPC server (no-op on non-unix platforms).
    pub async fn run(self, _event_bus: Arc<EventBus>, mut shutdown_rx: mpsc::Receiver<()>) {
        tracing::warn!("IPC server not supported on this platform");
        let _ = shutdown_rx.recv().await;
    }

    /// Run the IPC server with registry (no-op on non-unix platforms).
    pub async fn run_with_registry(
        self,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        let _ = shutdown_rx.recv().await;
    }

    /// Run the IPC server with optional auth configuration (no-op on non-unix platforms).
    pub async fn run_with_auth(
        self,
        _event_bus: Arc<EventBus>,
        _auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        let _ = shutdown_rx.recv().await;
    }

    /// Run the IPC server with registry and auth (no-op on non-unix platforms).
    pub async fn run_with_registry_and_auth(
        self,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        _auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        let _ = shutdown_rx.recv().await;
    }

    /// Run the IPC server with registry, auth, and RPC handler (no-op on non-unix platforms).
    pub async fn run_with_registry_auth_and_rpc(
        self,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        _auth: Option<IpcAuth>,
        _rpc_handler: Option<IpcRpcHandler>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        let _ = shutdown_rx.recv().await;
    }
}

/// Handle a single client connection with full context.
#[cfg(unix)]
async fn handle_client_with_context(
    stream: UnixStream,
    ctx: Arc<IpcHandlerContext>,
) -> std::io::Result<()> {
    let start = Instant::now();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Read one request per connection (simple request-response)
    let bytes_read = reader.read_line(&mut line).await?;
    if bytes_read == 0 {
        return Ok(()); // Client disconnected
    }

    // Check message size
    if line.len() > MAX_MESSAGE_SIZE {
        let response = IpcResponse::error("message too large");
        let response_json = serde_json::to_string(&response).unwrap_or_default();
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        return Ok(());
    }

    // Parse and handle request
    let response = match serde_json::from_str::<IpcEnvelope>(&line) {
        Ok(envelope) => {
            if let Some(auth) = ctx.auth.as_ref() {
                if let Err(err) =
                    auth.authorize(envelope.token.as_deref(), envelope.request.required_scope())
                {
                    IpcResponse::error(err.message())
                } else {
                    handle_request_with_context(envelope, &ctx).await
                }
            } else {
                handle_request_with_context(envelope, &ctx).await
            }
        }
        Err(e) => IpcResponse::error(format!("invalid request: {e}")),
    };

    let response = response.with_timing(start);

    // Send response
    let response_json = serde_json::to_string(&response).unwrap_or_default();
    writer.write_all(response_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    Ok(())
}

/// Handle a parsed IPC request with full context.
async fn handle_request_with_context(
    envelope: IpcEnvelope,
    ctx: &IpcHandlerContext,
) -> IpcResponse {
    match envelope.request {
        IpcRequest::UserVar {
            pane_id,
            name,
            value,
        } => {
            // Decode and validate the user-var payload
            match UserVarPayload::decode(&value, true) {
                Ok(payload) => {
                    // Publish event to the bus
                    let event = Event::UserVarReceived {
                        pane_id,
                        name,
                        payload,
                    };
                    let subscribers = ctx.event_bus.publish(event);
                    tracing::debug!(pane_id, subscribers, "Published user-var event");
                    IpcResponse::ok()
                }
                Err(e) => IpcResponse::error(e.to_string()),
            }
        }
        // NOTE: IpcRequest::StatusUpdate was removed in v0.2.0 (Lua performance optimization)
        IpcRequest::Ping => {
            let uptime_ms = u64::try_from(ctx.event_bus.uptime().as_millis()).unwrap_or(u64::MAX);
            IpcResponse::ok_with_data(serde_json::json!({
                "pong": true,
                "uptime_ms": uptime_ms,
            }))
        }
        IpcRequest::Status => {
            let stats = ctx.event_bus.stats();
            let total_queued = stats.delta_queued + stats.detection_queued + stats.signal_queued;
            let total_subscribers =
                stats.delta_subscribers + stats.detection_subscribers + stats.signal_subscribers;
            let uptime_ms = u64::try_from(ctx.event_bus.uptime().as_millis()).unwrap_or(u64::MAX);
            let mut payload = serde_json::json!({
                "uptime_ms": uptime_ms,
                "events_queued": total_queued,
                "subscriber_count": total_subscribers,
            });
            let health = HealthSnapshot::get_global()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .unwrap_or(serde_json::Value::Null);
            payload["health"] = health;
            IpcResponse::ok_with_data(payload)
        }
        IpcRequest::PaneState { pane_id } => handle_pane_state(pane_id, ctx).await,
        IpcRequest::SetPanePriority {
            pane_id,
            priority,
            ttl_ms,
        } => handle_set_pane_priority(pane_id, priority, ttl_ms, ctx).await,
        IpcRequest::ClearPanePriority { pane_id } => handle_clear_pane_priority(pane_id, ctx).await,
        IpcRequest::Rpc { args } => {
            let Some(handler) = ctx.rpc_handler.as_ref() else {
                return IpcResponse::error("rpc handler not configured");
            };
            handler(IpcRpcRequest {
                args,
                request_id: envelope.request_id,
            })
            .await
        }
    }
}

async fn handle_pane_state(pane_id: u64, ctx: &IpcHandlerContext) -> IpcResponse {
    let Some(ref registry_lock) = ctx.registry else {
        return IpcResponse::ok_with_data(serde_json::json!({
            "pane_id": pane_id,
            "known": false,
            "reason": "no_registry",
        }));
    };

    let (entry, cursor) = {
        let registry = registry_lock.read().await;
        let Some(entry) = registry.get_entry(pane_id) else {
            return IpcResponse::ok_with_data(serde_json::json!({
                "pane_id": pane_id,
                "known": false,
                "reason": "unknown_pane",
            }));
        };
        (entry.clone(), registry.get_cursor(pane_id).cloned())
    };

    // Note: "alt_screen" and "last_status_at" are deprecated fields (always false/null since v0.2.0).
    // Use "cursor_alt_screen" for authoritative alt-screen state from escape sequence detection.
    IpcResponse::ok_with_data(serde_json::json!({
        "pane_id": pane_id,
        "known": true,
        "observed": entry.should_observe(),
        "alt_screen": entry.is_alt_screen,  // DEPRECATED: always false, use cursor_alt_screen
        "last_status_at": entry.last_status_at,  // DEPRECATED: always null
        "in_gap": cursor.as_ref().map(|c| c.in_gap),
        "cursor_alt_screen": cursor.as_ref().map(|c| c.in_alt_screen),  // Authoritative alt-screen state
    }))
}

async fn handle_set_pane_priority(
    pane_id: u64,
    priority: u32,
    ttl_ms: Option<u64>,
    ctx: &IpcHandlerContext,
) -> IpcResponse {
    let Some(ref registry_lock) = ctx.registry else {
        return IpcResponse::error_with_code(
            "ipc.no_registry",
            "pane registry not available",
            Some("Start the watcher with `wa watch` in this workspace.".to_string()),
        );
    };

    let installed = {
        let mut registry = registry_lock.write().await;
        match registry.set_priority_override(pane_id, priority, ttl_ms) {
            Ok(ov) => ov,
            Err(e) => {
                return IpcResponse::error_with_code(
                    "ipc.pane_not_found",
                    format!("pane {pane_id} not found: {e}"),
                    Some(
                        "Use `wa robot state` or `wezterm cli list` to find valid pane IDs."
                            .to_string(),
                    ),
                );
            }
        }
    };

    IpcResponse::ok_with_data(serde_json::json!({
        "pane_id": pane_id,
        "priority": installed.priority,
        "set_at": installed.set_at,
        "expires_at": installed.expires_at,
        "ttl_ms": ttl_ms,
    }))
}

async fn handle_clear_pane_priority(pane_id: u64, ctx: &IpcHandlerContext) -> IpcResponse {
    let Some(ref registry_lock) = ctx.registry else {
        return IpcResponse::error_with_code(
            "ipc.no_registry",
            "pane registry not available",
            Some("Start the watcher with `wa watch` in this workspace.".to_string()),
        );
    };

    {
        let mut registry = registry_lock.write().await;
        if let Err(e) = registry.clear_priority_override(pane_id) {
            return IpcResponse::error_with_code(
                "ipc.pane_not_found",
                format!("pane {pane_id} not found: {e}"),
                Some(
                    "Use `wa robot state` or `wezterm cli list` to find valid pane IDs."
                        .to_string(),
                ),
            );
        }
    }

    IpcResponse::ok_with_data(serde_json::json!({
        "pane_id": pane_id,
        "cleared": true,
    }))
}

// NOTE: handle_status_update function was removed in v0.2.0 (Lua performance optimization)
// Alt-screen detection is now handled via escape sequence parsing (see screen_state.rs).

/// IPC client for sending requests to the watcher daemon.
pub struct IpcClient {
    socket_path: PathBuf,
    auth_token: Option<String>,
}

impl IpcClient {
    /// Create a new IPC client.
    #[must_use]
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            auth_token: std::env::var("WA_IPC_TOKEN").ok(),
        }
    }

    /// Create a new IPC client with an explicit auth token.
    #[must_use]
    pub fn with_token(socket_path: impl AsRef<Path>, token: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            auth_token: Some(token.into()),
        }
    }

    /// Update the auth token (use `None` to clear).
    pub fn set_token(&mut self, token: Option<String>) {
        self.auth_token = token;
    }

    /// Check if the watcher socket exists.
    #[must_use]
    pub fn socket_exists(&self) -> bool {
        self.socket_path.exists()
    }
}

#[cfg(unix)]
impl IpcClient {
    /// Send a user-var event to the watcher daemon.
    ///
    /// # Arguments
    /// * `pane_id` - Pane that emitted the user-var
    /// * `name` - Variable name (e.g., "WA_EVENT")
    /// * `value` - Raw value (typically base64-encoded JSON)
    ///
    /// # Errors
    /// Returns error if connection or send fails.
    pub async fn send_user_var(
        &self,
        pane_id: u64,
        name: String,
        value: String,
    ) -> Result<IpcResponse, UserVarError> {
        let request = IpcRequest::UserVar {
            pane_id,
            name,
            value,
        };
        self.send_request(request).await
    }

    /// Ping the watcher daemon.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn ping(&self) -> Result<IpcResponse, UserVarError> {
        self.send_request(IpcRequest::Ping).await
    }

    /// Get watcher status.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn status(&self) -> Result<IpcResponse, UserVarError> {
        self.send_request(IpcRequest::Status).await
    }

    /// Request pane state from watcher registry.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn pane_state(&self, pane_id: u64) -> Result<IpcResponse, UserVarError> {
        self.send_request(IpcRequest::PaneState { pane_id }).await
    }

    /// Set a runtime pane capture priority override.
    pub async fn set_pane_priority(
        &self,
        pane_id: u64,
        priority: u32,
        ttl_ms: Option<u64>,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request(IpcRequest::SetPanePriority {
            pane_id,
            priority,
            ttl_ms,
        })
        .await
    }

    /// Clear any runtime pane capture priority override.
    pub async fn clear_pane_priority(&self, pane_id: u64) -> Result<IpcResponse, UserVarError> {
        self.send_request(IpcRequest::ClearPanePriority { pane_id })
            .await
    }

    /// Call a robot RPC command over IPC.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn call_rpc(
        &self,
        args: Vec<String>,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_id(IpcRequest::Rpc { args }, request_id)
            .await
    }

    // NOTE: send_status_update method was removed in v0.2.0 (Lua performance optimization)

    /// Send a request and receive a response.
    async fn send_request(&self, request: IpcRequest) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_id(request, None).await
    }

    async fn send_request_with_id(
        &self,
        request: IpcRequest,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        // Check if socket exists
        if !self.socket_path.exists() {
            return Err(UserVarError::WatcherNotRunning {
                socket_path: self.socket_path.display().to_string(),
            });
        }

        // Connect to socket
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            UserVarError::IpcSendFailed {
                message: format!("failed to connect: {e}"),
            }
        })?;

        let (reader, mut writer) = stream.into_split();

        // Send request
        let envelope = IpcEnvelope {
            token: self.auth_token.clone(),
            request_id,
            request,
        };
        let request_json =
            serde_json::to_string(&envelope).map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to serialize request: {e}"),
            })?;

        writer
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to send: {e}"),
            })?;
        writer
            .write_all(b"\n")
            .await
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to send newline: {e}"),
            })?;
        writer
            .flush()
            .await
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to flush: {e}"),
            })?;

        // Read response
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to read response: {e}"),
            })?;

        // Parse response
        let response: IpcResponse =
            serde_json::from_str(&line).map_err(|e| UserVarError::IpcSendFailed {
                message: format!("invalid response: {e}"),
            })?;

        Ok(response)
    }
}

#[cfg(not(unix))]
impl IpcClient {
    /// IPC is unix-only; return a clear error on other platforms.
    fn unsupported() -> UserVarError {
        UserVarError::IpcSendFailed {
            message: "IPC sockets are only supported on unix platforms".to_string(),
        }
    }

    pub async fn send_user_var(
        &self,
        _pane_id: u64,
        _name: String,
        _value: String,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn ping(&self) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn status(&self) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn pane_state(&self, _pane_id: u64) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn call_rpc(
        &self,
        _args: Vec<String>,
        _request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::items_after_statements, clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    #[test]
    fn ipc_response_ok_serializes() {
        let response = IpcResponse::ok();
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(!json.contains("error"));
        assert!(json.contains("\"elapsed_ms\""));
        assert!(json.contains("\"version\""));
        assert!(json.contains("\"now\""));
    }

    #[test]
    fn ipc_response_error_serializes() {
        let response = IpcResponse::error("test error");
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("test error"));
    }

    #[test]
    fn ipc_response_error_with_code_serializes() {
        let response = IpcResponse::error_with_code(
            "ipc.test_error",
            "test error",
            Some("try again".to_string()),
        );
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("\"error_code\":\"ipc.test_error\""));
        assert!(json.contains("\"hint\":\"try again\""));
    }

    #[test]
    fn ipc_request_user_var_serializes() {
        let request = IpcRequest::UserVar {
            pane_id: 42,
            name: "WA_EVENT".to_string(),
            value: "eyJraW5kIjoidGVzdCJ9".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"user_var\""));
        assert!(json.contains("\"pane_id\":42"));
    }

    #[test]
    fn ipc_request_ping_serializes() {
        let request = IpcRequest::Ping;
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"ping\""));
    }

    #[test]
    fn ipc_request_pane_state_serializes() {
        let request = IpcRequest::PaneState { pane_id: 42 };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"pane_state\""));
        assert!(json.contains("\"pane_id\":42"));
    }

    #[test]
    fn ipc_request_rpc_serializes() {
        let request = IpcRequest::Rpc {
            args: vec!["state".to_string()],
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"rpc\""));
        assert!(json.contains("\"state\""));
    }

    #[test]
    fn ipc_client_detects_missing_socket() {
        let client = IpcClient::new("/nonexistent/path/ipc.sock");
        assert!(!client.socket_exists());
    }

    fn build_auth(token: &str, scopes: Vec<IpcScope>, expires_at_ms: Option<u64>) -> IpcAuth {
        IpcAuth::new(vec![IpcAuthToken {
            token: token.to_string(),
            scopes,
            expires_at_ms,
        }])
    }

    async fn start_auth_server(
        socket_path: &Path,
        auth: IpcAuth,
    ) -> (mpsc::Sender<()>, tokio::task::JoinHandle<()>) {
        let server = IpcServer::bind(socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let handle = tokio::spawn(async move {
            server
                .run_with_auth(event_bus, Some(auth), shutdown_rx)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        (shutdown_tx, handle)
    }

    #[tokio::test]
    async fn ipc_auth_rejects_missing_token() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let auth = build_auth("secret", vec![IpcScope::Read], None);
        let (shutdown_tx, server_handle) = start_auth_server(&socket_path, auth).await;

        let mut client = IpcClient::new(&socket_path);
        client.set_token(None);
        let response = client.ping().await.unwrap();
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap_or_default()
                .contains("missing auth token")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_auth_rejects_invalid_token() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let auth = build_auth("secret", vec![IpcScope::Read], None);
        let (shutdown_tx, server_handle) = start_auth_server(&socket_path, auth).await;

        let client = IpcClient::with_token(&socket_path, "bad-token");
        let response = client.ping().await.unwrap();
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap_or_default()
                .contains("invalid auth token")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_auth_rejects_expired_token() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let expired_at = now_ms().saturating_sub(1);
        let auth = build_auth("secret", vec![IpcScope::Read], Some(expired_at));
        let (shutdown_tx, server_handle) = start_auth_server(&socket_path, auth).await;

        let client = IpcClient::with_token(&socket_path, "secret");
        let response = client.ping().await.unwrap();
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap_or_default()
                .contains("auth token expired")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_auth_enforces_scopes() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let auth = build_auth("reader", vec![IpcScope::Read], None);
        let (shutdown_tx, server_handle) = start_auth_server(&socket_path, auth).await;

        let client = IpcClient::with_token(&socket_path, "reader");
        let response = client
            .send_user_var(
                1,
                "WA_EVENT".to_string(),
                "eyJraW5kIjoidGVzdCJ9".to_string(),
            )
            .await
            .unwrap();
        assert!(!response.ok);
        assert!(
            response
                .error
                .unwrap_or_default()
                .contains("insufficient scope")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        // Start server
        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Create client and send ping
        let client = IpcClient::new(&socket_path);
        let response = client.ping().await.unwrap();
        assert!(response.ok);
        assert!(response.data.is_some());

        // Send user-var event
        let response = client
            .send_user_var(
                1,
                "WA_EVENT".to_string(),
                "eyJraW5kIjoidGVzdCJ9".to_string(), // {"kind":"test"}
            )
            .await
            .unwrap();
        assert!(response.ok);

        // Shutdown
        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_server_removes_socket_on_shutdown() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_handle = tokio::spawn(async move {
            server.run(event_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(socket_path.exists());

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
        assert!(!socket_path.exists());
    }

    fn make_pane_info(pane_id: u64) -> crate::wezterm::PaneInfo {
        crate::wezterm::PaneInfo {
            pane_id,
            tab_id: 1,
            window_id: 1,
            domain_id: None,
            domain_name: Some("local".to_string()),
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn ipc_pane_state_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));

        {
            let mut registry = registry.write().await;
            registry.discovery_tick(vec![make_pane_info(7)]);
            if let Some(entry) = registry.get_entry_mut(7) {
                // Note: These fields are deprecated and manually set here only for testing
                // field serialization. In production, is_alt_screen is always false and
                // last_status_at is always None since Lua status updates were removed in v0.2.0.
                entry.is_alt_screen = true;
                entry.last_status_at = Some(123);
            }
            if let Some(cursor) = registry.get_cursor_mut(7) {
                cursor.in_gap = true;
                cursor.in_alt_screen = true;
            }
        }

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let server_handle = tokio::spawn(async move {
            server
                .run_with_registry(event_bus, registry, shutdown_rx)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let client = IpcClient::new(&socket_path);
        let response = client.pane_state(7).await.unwrap();
        assert!(response.ok);
        let data = response.data.unwrap();
        assert_eq!(
            data.get("pane_id").and_then(serde_json::Value::as_u64),
            Some(7)
        );
        assert_eq!(
            data.get("known").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            data.get("observed").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            data.get("alt_screen").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            data.get("cursor_alt_screen")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            data.get("in_gap").and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert!(data.get("last_status_at").is_some());

        let response = client.pane_state(999).await.unwrap();
        assert!(response.ok);
        let data = response.data.unwrap();
        assert_eq!(
            data.get("known").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            data.get("reason").and_then(|v| v.as_str()),
            Some("unknown_pane")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    // ========================================================================
    // User-var lane IPC integration tests (wa-4vx.4.10)
    // ========================================================================

    #[tokio::test]
    async fn user_var_event_reaches_event_bus() {
        use base64::Engine;

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        // Start server
        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        // Subscribe to signal events BEFORE starting server
        let mut subscriber = event_bus.subscribe_signals();

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send a user-var event
        let client = IpcClient::new(&socket_path);
        let json = r#"{"type":"command_start","cmd":"ls"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);

        let response = client
            .send_user_var(42, "WA_EVENT".to_string(), encoded)
            .await
            .unwrap();
        assert!(response.ok);

        // Verify event reached the bus
        let event = subscriber.try_recv();
        assert!(event.is_some());
        let event = event.unwrap().unwrap();

        if let Event::UserVarReceived {
            pane_id,
            name,
            payload,
        } = event
        {
            assert_eq!(pane_id, 42);
            assert_eq!(name, "WA_EVENT");
            assert_eq!(payload.event_type, Some("command_start".to_string()));
        } else {
            panic!("Expected UserVarReceived event, got {:?}", event);
        }

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_status_returns_event_bus_stats() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        let client = IpcClient::new(&socket_path);
        let response = client.status().await.unwrap();

        assert!(response.ok);
        assert!(response.data.is_some());
        let data = response.data.unwrap();
        assert!(data.get("uptime_ms").is_some());
        assert!(data.get("events_queued").is_some());
        assert!(data.get("subscriber_count").is_some());

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_client_error_on_missing_socket() {
        let client = IpcClient::new("/nonexistent/path/ipc.sock");
        let result = client.ping().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));
    }

    #[tokio::test]
    async fn ipc_handles_invalid_json_request() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send invalid JSON directly via raw socket
        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(b"not valid json\n").await.unwrap();
        stream.flush().await.unwrap();

        // Read response
        let (reader, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(!response.ok);
        assert!(response.error.is_some());
        assert!(response.error.unwrap().contains("invalid request"));

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_rejects_oversized_messages() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Create an oversized message (> MAX_MESSAGE_SIZE)
        let oversized_value = "x".repeat(MAX_MESSAGE_SIZE + 1000);
        let request = IpcRequest::UserVar {
            pane_id: 1,
            name: "TEST".to_string(),
            value: oversized_value,
        };
        let request_json = serde_json::to_string(&request).unwrap();

        // Send directly
        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(request_json.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();

        let (reader, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(!response.ok);
        assert!(response.error.is_some());
        assert!(response.error.unwrap().contains("too large"));

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn multiple_clients_can_connect_concurrently() {
        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Spawn multiple concurrent clients
        let socket_path_clone = socket_path.clone();
        let handles: Vec<_> = (0..5)
            .map(|i| {
                let path = socket_path_clone.clone();
                tokio::spawn(async move {
                    let client = IpcClient::new(&path);
                    let response = client.ping().await.unwrap();
                    assert!(response.ok, "Client {} failed", i);
                })
            })
            .collect();

        for handle in handles {
            handle.await.unwrap();
        }

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }
}
