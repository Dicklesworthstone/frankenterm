//! IPC module for watcher daemon communication.
//!
//! Provides Unix domain socket communication between CLI commands and the
//! watcher daemon. Used primarily for delivering user-var events from
//! shell hooks to the running watcher.
//!
//! # Protocol
//!
//! The protocol uses JSON lines (newline-delimited JSON):
//! - Client sends: `{"type":"user_var","pane_id":1,"name":"FT_EVENT","value":"base64..."}\n`
//! - Server responds: `{"ok":true}\n` or `{"ok":false,"error":"..."}\n`

use crate::runtime_compat::RwLock;
use crate::runtime_compat::mpsc;
#[cfg(unix)]
use crate::runtime_compat::unix::{self as compat_unix, AsyncWriteExt, UnixListener, UnixStream};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{IpcAuthToken, IpcScope};
use crate::crash::HealthSnapshot;
use crate::events::{Event, EventBus, UserVarError, UserVarPayload};
use crate::ingest::PaneRegistry;

/// Default IPC socket filename relative to workspace .ft directory.
pub const IPC_SOCKET_NAME: &str = "ipc.sock";

/// Maximum message size in bytes (128KB).
pub const MAX_MESSAGE_SIZE: usize = 131_072;
#[cfg(unix)]
const IPC_ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);
#[cfg(unix)]
const IPC_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(1);

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
        /// Variable name (e.g., "FT_EVENT")
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
    /// ft version
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

        let listener = compat_unix::bind(&socket_path).await?;
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
        let mut connection_tasks = crate::runtime_compat::task::JoinSet::new();

        loop {
            if shutdown_signal_pending(shutdown_rx).await {
                tracing::info!("IPC server shutting down");
                break;
            }

            match crate::runtime_compat::timeout(IPC_ACCEPT_POLL_INTERVAL, self.listener.accept())
                .await
            {
                Ok(Ok((stream, _addr))) => {
                    let ctx = ctx.clone();
                    connection_tasks.spawn(async move {
                        if let Err(e) = handle_client_with_context(stream, ctx).await {
                            tracing::warn!(error = %e, "IPC client error");
                        }
                    });
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "Failed to accept IPC connection");
                }
                Err(_elapsed) => {}
            }

            while let Some(join_result) = connection_tasks.try_join_next() {
                if let Err(join_err) = join_result {
                    tracing::debug!(error = %join_err, "IPC client task failed");
                }
            }
        }

        while let Some(join_result) = connection_tasks.join_next().await {
            if let Err(join_err) = join_result {
                tracing::debug!(error = %join_err, "IPC client task failed during shutdown");
            }
        }

        // Clean up socket file
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
async fn shutdown_signal_pending(shutdown_rx: &mut mpsc::Receiver<()>) -> bool {
    match crate::runtime_compat::timeout(
        IPC_SHUTDOWN_POLL_INTERVAL,
        crate::runtime_compat::mpsc_recv_option(shutdown_rx),
    )
    .await
    {
        Ok(Some(()) | None) => true,
        Err(_elapsed) => false,
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

    async fn recv_shutdown(shutdown_rx: &mut mpsc::Receiver<()>) {
        let _ = crate::runtime_compat::mpsc_recv_option(shutdown_rx).await;
    }

    /// Run the IPC server (no-op on non-unix platforms).
    pub async fn run(self, _event_bus: Arc<EventBus>, mut shutdown_rx: mpsc::Receiver<()>) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown(&mut shutdown_rx).await;
    }

    /// Run the IPC server with registry (no-op on non-unix platforms).
    pub async fn run_with_registry(
        self,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown(&mut shutdown_rx).await;
    }

    /// Run the IPC server with optional auth configuration (no-op on non-unix platforms).
    pub async fn run_with_auth(
        self,
        _event_bus: Arc<EventBus>,
        _auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown(&mut shutdown_rx).await;
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
        Self::recv_shutdown(&mut shutdown_rx).await;
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
        Self::recv_shutdown(&mut shutdown_rx).await;
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

    // Read one request per connection (simple request-response)
    let mut lines = compat_unix::lines(compat_unix::buffered(reader));
    let Some(line) = compat_unix::next_line(&mut lines).await? else {
        return Ok(()); // Client disconnected
    };

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
            let mut health = HealthSnapshot::get_global()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .unwrap_or(serde_json::Value::Null);
            if let Some(runtime_lock_memory) =
                crate::runtime::RuntimeLockMemoryTelemetrySnapshot::get_global()
                    .and_then(|snapshot| serde_json::to_value(snapshot).ok())
            {
                if let Some(health_obj) = health.as_object_mut() {
                    health_obj.insert("runtime_lock_memory".to_string(), runtime_lock_memory);
                } else {
                    payload["runtime_lock_memory"] = runtime_lock_memory;
                }
            }
            payload["health"] = health;
            if let Some(snapshot) =
                crate::resize_scheduler::ResizeSchedulerDebugSnapshot::get_global()
            {
                let stalled = snapshot.stalled_transactions(now_ms(), 2_000);
                payload["resize_control_plane"] =
                    serde_json::to_value(&snapshot).unwrap_or(serde_json::Value::Null);
                payload["resize_control_plane_stalled"] =
                    serde_json::to_value(stalled).unwrap_or(serde_json::Value::Null);
            } else {
                payload["resize_control_plane"] = serde_json::Value::Null;
                payload["resize_control_plane_stalled"] = serde_json::Value::Null;
            }
            if let Some(watchdog) = crate::runtime::evaluate_resize_watchdog(now_ms()) {
                payload["resize_control_plane_watchdog"] =
                    serde_json::to_value(&watchdog).unwrap_or(serde_json::Value::Null);
                let ladder = crate::runtime::derive_resize_degradation_ladder(&watchdog);
                payload["resize_degradation_ladder"] =
                    serde_json::to_value(ladder).unwrap_or(serde_json::Value::Null);
            } else {
                payload["resize_control_plane_watchdog"] = serde_json::Value::Null;
                payload["resize_degradation_ladder"] = serde_json::Value::Null;
            }
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
            Some("Start the watcher with `ft watch` in this workspace.".to_string()),
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
                        "Use `ft robot state` or `wezterm cli list` to find valid pane IDs."
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
            Some("Start the watcher with `ft watch` in this workspace.".to_string()),
        );
    };

    {
        let mut registry = registry_lock.write().await;
        if let Err(e) = registry.clear_priority_override(pane_id) {
            return IpcResponse::error_with_code(
                "ipc.pane_not_found",
                format!("pane {pane_id} not found: {e}"),
                Some(
                    "Use `ft robot state` or `wezterm cli list` to find valid pane IDs."
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
            auth_token: std::env::var("FT_IPC_TOKEN").ok(),
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
    /// * `name` - Variable name (e.g., "FT_EVENT")
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
        let stream = compat_unix::connect(&self.socket_path).await.map_err(|e| {
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
        let mut lines = compat_unix::lines(compat_unix::buffered(reader));
        let line = compat_unix::next_line(&mut lines)
            .await
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to read response: {e}"),
            })?
            .ok_or_else(|| UserVarError::IpcSendFailed {
                message: "failed to read response: server closed connection".to_string(),
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
    use crate::runtime_compat::{CompatRuntime, RuntimeBuilder, RwLock};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

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
            name: "FT_EVENT".to_string(),
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
    ) -> (
        mpsc::Sender<()>,
        crate::runtime_compat::task::JoinHandle<()>,
    ) {
        let server = IpcServer::bind(socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let handle = crate::runtime_compat::task::spawn(async move {
            server
                .run_with_auth(event_bus, Some(auth), shutdown_rx)
                .await;
        });

        crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;
        (shutdown_tx, handle)
    }

    async fn send_shutdown(shutdown_tx: &mpsc::Sender<()>) {
        let _ = crate::runtime_compat::mpsc_send(shutdown_tx, ()).await;
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for ipc tests");
        runtime.block_on(future);
    }

    #[test]
    fn ipc_auth_rejects_missing_token() {
        run_async_test(async {
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

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_auth_rejects_invalid_token() {
        run_async_test(async {
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

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_auth_rejects_expired_token() {
        run_async_test(async {
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

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_auth_enforces_scopes() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let auth = build_auth("reader", vec![IpcScope::Read], None);
            let (shutdown_tx, server_handle) = start_auth_server(&socket_path, auth).await;

            let client = IpcClient::with_token(&socket_path, "reader");
            let response = client
                .send_user_var(
                    1,
                    "FT_EVENT".to_string(),
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

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_roundtrip() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            // Start server
            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            // Give server time to start
            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

            // Create client and send ping
            let client = IpcClient::new(&socket_path);
            let response = client.ping().await.unwrap();
            assert!(response.ok);
            assert!(response.data.is_some());

            // Send user-var event
            let response = client
                .send_user_var(
                    1,
                    "FT_EVENT".to_string(),
                    "eyJraW5kIjoidGVzdCJ9".to_string(), // {"kind":"test"}
                )
                .await
                .unwrap();
            assert!(response.ok);

            // Shutdown
            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_server_removes_socket_on_shutdown() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(event_bus, shutdown_rx).await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;
            assert!(socket_path.exists());

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
            assert!(!socket_path.exists());
        });
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

    #[test]
    fn ipc_pane_state_roundtrip() {
        run_async_test(async {
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
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server
                    .run_with_registry(event_bus, registry, shutdown_rx)
                    .await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

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

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    // ========================================================================
    // User-var lane IPC integration tests (wa-4vx.4.10)
    // ========================================================================

    #[test]
    fn user_var_event_reaches_event_bus() {
        run_async_test(async {
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
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

            // Send a user-var event
            let client = IpcClient::new(&socket_path);
            let json = r#"{"type":"command_start","cmd":"ls"}"#;
            let encoded = base64::engine::general_purpose::STANDARD.encode(json);

            let response = client
                .send_user_var(42, "FT_EVENT".to_string(), encoded)
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
                assert_eq!(name, "FT_EVENT");
                assert_eq!(payload.event_type, Some("command_start".to_string()));
            } else {
                panic!("Expected UserVarReceived event, got {:?}", event);
            }

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_status_returns_event_bus_stats() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");
            crate::runtime::RuntimeLockMemoryTelemetrySnapshot::update_global(
                crate::runtime::RuntimeLockMemoryTelemetrySnapshot {
                    timestamp_ms: 123,
                    avg_storage_lock_wait_ms: 1.5,
                    p50_storage_lock_wait_ms: 1.0,
                    p95_storage_lock_wait_ms: 2.8,
                    max_storage_lock_wait_ms: 3.0,
                    storage_lock_contention_events: 2,
                    avg_storage_lock_hold_ms: 2.5,
                    p50_storage_lock_hold_ms: 2.0,
                    p95_storage_lock_hold_ms: 3.9,
                    max_storage_lock_hold_ms: 4.0,
                    cursor_snapshot_bytes_last: 2048,
                    p50_cursor_snapshot_bytes: 3000,
                    p95_cursor_snapshot_bytes: 3900,
                    cursor_snapshot_bytes_max: 4096,
                    avg_cursor_snapshot_bytes: 3072.0,
                },
            );

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

            let client = IpcClient::new(&socket_path);
            let response = client.status().await.unwrap();

            assert!(response.ok);
            assert!(response.data.is_some());
            let data = response.data.unwrap();
            assert!(data.get("uptime_ms").is_some());
            assert!(data.get("events_queued").is_some());
            assert!(data.get("subscriber_count").is_some());
            let runtime_lock_memory = data
                .get("health")
                .and_then(serde_json::Value::as_object)
                .and_then(|health| health.get("runtime_lock_memory"))
                .or_else(|| data.get("runtime_lock_memory"))
                .and_then(serde_json::Value::as_object)
                .expect("runtime_lock_memory should be present in status payload");
            assert_eq!(
                runtime_lock_memory.get("max_storage_lock_wait_ms"),
                Some(&serde_json::json!(3.0))
            );
            assert_eq!(
                runtime_lock_memory.get("p95_cursor_snapshot_bytes"),
                Some(&serde_json::json!(3900))
            );

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_client_error_on_missing_socket() {
        run_async_test(async {
            let client = IpcClient::new("/nonexistent/path/ipc.sock");
            let result = client.ping().await;

            assert!(result.is_err());
            let err = result.unwrap_err();
            assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));
        });
    }

    #[test]
    fn ipc_handles_invalid_json_request() {
        run_async_test(async {
            use crate::runtime_compat::unix::{self as compat_unix, AsyncWriteExt};

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

            // Send invalid JSON directly via raw socket
            let mut stream = compat_unix::connect(&socket_path).await.unwrap();
            stream.write_all(b"not valid json\n").await.unwrap();
            stream.flush().await.unwrap();

            // Read response
            let (reader, _) = stream.into_split();
            let mut lines = compat_unix::lines(compat_unix::buffered(reader));
            let line = compat_unix::next_line(&mut lines)
                .await
                .unwrap()
                .expect("expected response line");

            let response: IpcResponse = serde_json::from_str(&line).unwrap();
            assert!(!response.ok);
            assert!(response.error.is_some());
            assert!(response.error.unwrap().contains("invalid request"));

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_rejects_oversized_messages() {
        run_async_test(async {
            use crate::runtime_compat::unix::{self as compat_unix, AsyncWriteExt};

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

            // Create an oversized message (> MAX_MESSAGE_SIZE)
            let oversized_value = "x".repeat(MAX_MESSAGE_SIZE + 1000);
            let request = IpcRequest::UserVar {
                pane_id: 1,
                name: "TEST".to_string(),
                value: oversized_value,
            };
            let request_json = serde_json::to_string(&request).unwrap();

            // Send directly
            let mut stream = compat_unix::connect(&socket_path).await.unwrap();
            stream.write_all(request_json.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
            stream.flush().await.unwrap();

            let (reader, _) = stream.into_split();
            let mut lines = compat_unix::lines(compat_unix::buffered(reader));
            let line = compat_unix::next_line(&mut lines)
                .await
                .unwrap()
                .expect("expected response line");

            let response: IpcResponse = serde_json::from_str(&line).unwrap();
            assert!(!response.ok);
            assert!(response.error.is_some());
            assert!(response.error.unwrap().contains("too large"));

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn multiple_clients_can_connect_concurrently() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_compat::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_compat::sleep(std::time::Duration::from_millis(10)).await;

            // Spawn multiple concurrent clients
            let socket_path_clone = socket_path.clone();
            let handles: Vec<_> = (0..5)
                .map(|i| {
                    let path = socket_path_clone.clone();
                    crate::runtime_compat::task::spawn(async move {
                        let client = IpcClient::new(&path);
                        let response = client.ping().await.unwrap();
                        assert!(response.ok, "Client {} failed", i);
                    })
                })
                .collect();

            for handle in handles {
                handle.await.unwrap();
            }

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    // ========================================================================
    // Pure-function unit tests (no async server needed)
    // ========================================================================

    #[test]
    fn rpc_required_scope_send_is_write() {
        let args = vec!["send".to_string(), "1".to_string(), "ls".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_approve_is_write() {
        let args = vec!["approve".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_state_is_read() {
        let args = vec!["state".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn rpc_required_scope_empty_args_is_write() {
        let args: Vec<String> = vec![];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_workflow_run_is_write() {
        let args = vec!["workflow".to_string(), "run".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_workflow_abort_is_write() {
        let args = vec!["workflow".to_string(), "abort".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_workflow_status_is_read() {
        let args = vec!["workflow".to_string(), "status".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn rpc_required_scope_workflow_no_subcommand_is_read() {
        let args = vec!["workflow".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn rpc_required_scope_accounts_refresh_is_write() {
        let args = vec!["accounts".to_string(), "refresh".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_accounts_list_is_read() {
        let args = vec!["accounts".to_string(), "list".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn rpc_required_scope_reservations_reserve_is_write() {
        let args = vec!["reservations".to_string(), "reserve".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_reservations_release_is_write() {
        let args = vec!["reservations".to_string(), "release".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_reservations_list_is_read() {
        let args = vec!["reservations".to_string(), "list".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn rpc_required_scope_unknown_command_is_read() {
        let args = vec!["events".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn ipc_request_required_scope_user_var_is_write() {
        let req = IpcRequest::UserVar {
            pane_id: 1,
            name: "FT_EVENT".to_string(),
            value: "val".to_string(),
        };
        assert_eq!(req.required_scope(), IpcScope::Write);
    }

    #[test]
    fn ipc_request_required_scope_ping_is_read() {
        assert_eq!(IpcRequest::Ping.required_scope(), IpcScope::Read);
    }

    #[test]
    fn ipc_request_required_scope_status_is_read() {
        assert_eq!(IpcRequest::Status.required_scope(), IpcScope::Read);
    }

    #[test]
    fn ipc_request_required_scope_pane_state_is_read() {
        let req = IpcRequest::PaneState { pane_id: 5 };
        assert_eq!(req.required_scope(), IpcScope::Read);
    }

    #[test]
    fn ipc_request_required_scope_set_pane_priority_is_write() {
        let req = IpcRequest::SetPanePriority {
            pane_id: 1,
            priority: 10,
            ttl_ms: Some(5000),
        };
        assert_eq!(req.required_scope(), IpcScope::Write);
    }

    #[test]
    fn ipc_request_required_scope_clear_pane_priority_is_write() {
        let req = IpcRequest::ClearPanePriority { pane_id: 1 };
        assert_eq!(req.required_scope(), IpcScope::Write);
    }

    #[test]
    fn ipc_request_required_scope_rpc_delegates() {
        let req = IpcRequest::Rpc {
            args: vec!["state".to_string()],
        };
        assert_eq!(req.required_scope(), IpcScope::Read);

        let req = IpcRequest::Rpc {
            args: vec!["send".to_string(), "1".to_string()],
        };
        assert_eq!(req.required_scope(), IpcScope::Write);
    }

    #[test]
    fn ipc_auth_allows_when_no_tokens() {
        let auth = IpcAuth::new(vec![]);
        assert!(auth.authorize(None, IpcScope::Write).is_ok());
        assert!(auth.authorize(Some("any"), IpcScope::Read).is_ok());
    }

    #[test]
    fn ipc_auth_rejects_missing_token_when_tokens_exist() {
        let auth = build_auth("secret", vec![IpcScope::All], None);
        let err = auth.authorize(None, IpcScope::Read);
        assert!(err.is_err());
    }

    #[test]
    fn ipc_auth_rejects_wrong_token() {
        let auth = build_auth("secret", vec![IpcScope::All], None);
        let err = auth.authorize(Some("wrong"), IpcScope::Read);
        assert!(err.is_err());
    }

    #[test]
    fn ipc_auth_rejects_expired_token_sync() {
        let auth = build_auth("secret", vec![IpcScope::All], Some(0));
        let err = auth.authorize(Some("secret"), IpcScope::Read);
        assert!(err.is_err());
    }

    #[test]
    fn ipc_auth_accepts_valid_token_with_matching_scope() {
        let auth = build_auth("secret", vec![IpcScope::Read], None);
        assert!(auth.authorize(Some("secret"), IpcScope::Read).is_ok());
    }

    #[test]
    fn ipc_auth_rejects_insufficient_scope() {
        let auth = build_auth("secret", vec![IpcScope::Read], None);
        let err = auth.authorize(Some("secret"), IpcScope::Write);
        assert!(err.is_err());
    }

    #[test]
    fn ipc_auth_write_scope_allows_read() {
        let auth = build_auth("secret", vec![IpcScope::Write], None);
        assert!(auth.authorize(Some("secret"), IpcScope::Read).is_ok());
    }

    #[test]
    fn ipc_auth_all_scope_allows_everything() {
        let auth = build_auth("secret", vec![IpcScope::All], None);
        assert!(auth.authorize(Some("secret"), IpcScope::Read).is_ok());
        assert!(auth.authorize(Some("secret"), IpcScope::Write).is_ok());
    }

    #[test]
    fn ipc_auth_default_scope_is_all_when_empty() {
        let auth = IpcAuth::new(vec![IpcAuthToken {
            token: "secret".to_string(),
            scopes: vec![],
            expires_at_ms: None,
        }]);
        assert!(auth.authorize(Some("secret"), IpcScope::Write).is_ok());
        assert!(auth.authorize(Some("secret"), IpcScope::Read).is_ok());
    }

    #[test]
    fn ipc_auth_error_messages() {
        assert!(
            IpcAuthError::MissingToken
                .message()
                .contains("missing auth token")
        );
        assert!(
            IpcAuthError::InvalidToken
                .message()
                .contains("invalid auth token")
        );
        assert!(
            IpcAuthError::ExpiredToken
                .message()
                .contains("auth token expired")
        );
        assert!(
            IpcAuthError::InsufficientScope {
                required: IpcScope::Write
            }
            .message()
            .contains("insufficient scope")
        );
    }

    #[test]
    fn ipc_envelope_serde_roundtrip() {
        let envelope = IpcEnvelope {
            token: Some("my-token".to_string()),
            request_id: Some("req-123".to_string()),
            request: IpcRequest::Ping,
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: IpcEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.token.as_deref(), Some("my-token"));
        assert_eq!(parsed.request_id.as_deref(), Some("req-123"));
        assert!(matches!(parsed.request, IpcRequest::Ping));
    }

    #[test]
    fn ipc_envelope_without_token_or_request_id() {
        let json = r#"{"type":"ping"}"#;
        let envelope: IpcEnvelope = serde_json::from_str(json).unwrap();
        assert!(envelope.token.is_none());
        assert!(envelope.request_id.is_none());
        assert!(matches!(envelope.request, IpcRequest::Ping));
    }

    #[test]
    fn ipc_envelope_with_user_var() {
        let json =
            r#"{"token":"t","type":"user_var","pane_id":42,"name":"FT_EVENT","value":"abc"}"#;
        let envelope: IpcEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.token.as_deref(), Some("t"));
        if let IpcRequest::UserVar {
            pane_id,
            name,
            value,
        } = envelope.request
        {
            assert_eq!(pane_id, 42);
            assert_eq!(name, "FT_EVENT");
            assert_eq!(value, "abc");
        } else {
            panic!("Expected UserVar request");
        }
    }

    #[test]
    fn ipc_response_ok_with_data_includes_data() {
        let data = serde_json::json!({"key": "value"});
        let response = IpcResponse::ok_with_data(data.clone());
        assert!(response.ok);
        assert_eq!(response.data, Some(data));
        assert!(response.error.is_none());
    }

    #[test]
    fn ipc_response_with_timing_sets_elapsed_ms() {
        let start = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let response = IpcResponse::ok().with_timing(start);
        assert!(response.elapsed_ms >= 4);
        assert!(response.now > 0);
    }

    #[test]
    fn ipc_response_error_with_code_has_all_fields() {
        let r = IpcResponse::error_with_code("ipc.test", "msg", Some("hint".to_string()));
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("msg"));
        assert_eq!(r.error_code.as_deref(), Some("ipc.test"));
        assert_eq!(r.hint.as_deref(), Some("hint"));
    }

    #[test]
    fn ipc_response_serde_roundtrip_ok() {
        let response = IpcResponse::ok();
        let json = serde_json::to_string(&response).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.ok);
        assert!(parsed.error.is_none());
        assert!(parsed.error_code.is_none());
        assert!(parsed.hint.is_none());
        assert!(parsed.data.is_none());
    }

    #[test]
    fn ipc_response_serde_roundtrip_error() {
        let response = IpcResponse::error_with_code(
            "ipc.auth_failed",
            "unauthorized",
            Some("check token".to_string()),
        );
        let json = serde_json::to_string(&response).unwrap();
        let parsed: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error.as_deref(), Some("unauthorized"));
        assert_eq!(parsed.error_code.as_deref(), Some("ipc.auth_failed"));
        assert_eq!(parsed.hint.as_deref(), Some("check token"));
    }

    #[test]
    fn ipc_handler_context_new_has_defaults() {
        let event_bus = Arc::new(EventBus::new(10));
        let ctx = IpcHandlerContext::new(event_bus);
        assert!(ctx.registry.is_none());
        assert!(ctx.auth.is_none());
        assert!(ctx.rpc_handler.is_none());
    }

    #[test]
    fn ipc_handler_context_with_registry_sets_registry() {
        let event_bus = Arc::new(EventBus::new(10));
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        let ctx = IpcHandlerContext::with_registry(event_bus, registry);
        assert!(ctx.registry.is_some());
        assert!(ctx.auth.is_none());
    }

    #[test]
    fn ipc_handler_context_with_auth_sets_auth() {
        let event_bus = Arc::new(EventBus::new(10));
        let auth = IpcAuth::new(vec![]);
        let ctx = IpcHandlerContext::with_auth(event_bus, None, Some(auth));
        assert!(ctx.auth.is_some());
        assert!(ctx.registry.is_none());
    }

    #[test]
    fn ipc_handler_context_with_auth_and_rpc() {
        let event_bus = Arc::new(EventBus::new(10));
        let handler: IpcRpcHandler = Arc::new(|_req| Box::pin(async { IpcResponse::ok() }));
        let ctx = IpcHandlerContext::with_auth_and_rpc(event_bus, None, None, Some(handler));
        assert!(ctx.rpc_handler.is_some());
    }

    #[test]
    fn ipc_request_set_pane_priority_serializes() {
        let request = IpcRequest::SetPanePriority {
            pane_id: 3,
            priority: 10,
            ttl_ms: Some(5000),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"set_pane_priority\""));
        assert!(json.contains("\"pane_id\":3"));
        assert!(json.contains("\"priority\":10"));
        assert!(json.contains("\"ttl_ms\":5000"));
    }

    #[test]
    fn ipc_request_clear_pane_priority_serializes() {
        let request = IpcRequest::ClearPanePriority { pane_id: 7 };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"clear_pane_priority\""));
        assert!(json.contains("\"pane_id\":7"));
    }

    #[test]
    fn ipc_request_status_serializes() {
        let request = IpcRequest::Status;
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"status\""));
    }

    #[test]
    fn ipc_request_all_variants_deserialize() {
        let cases = vec![
            (r#"{"type":"ping"}"#, "Ping"),
            (r#"{"type":"status"}"#, "Status"),
            (
                r#"{"type":"user_var","pane_id":1,"name":"X","value":"Y"}"#,
                "UserVar",
            ),
            (r#"{"type":"pane_state","pane_id":1}"#, "PaneState"),
            (
                r#"{"type":"set_pane_priority","pane_id":1,"priority":5}"#,
                "SetPanePriority",
            ),
            (
                r#"{"type":"clear_pane_priority","pane_id":1}"#,
                "ClearPanePriority",
            ),
            (r#"{"type":"rpc","args":["state"]}"#, "Rpc"),
        ];
        for (json_str, expected) in cases {
            let parsed: IpcRequest = serde_json::from_str(json_str).unwrap();
            let debug = format!("{parsed:?}");
            assert!(debug.contains(expected), "Expected {expected} in {debug}");
        }
    }

    #[test]
    fn ipc_client_with_token_stores_token() {
        let client = IpcClient::with_token("/tmp/test.sock", "my-token");
        assert_eq!(client.auth_token.as_deref(), Some("my-token"));
    }

    #[test]
    fn ipc_client_set_token_updates() {
        let mut client = IpcClient::new("/tmp/test.sock");
        assert!(
            client.auth_token.is_none() || client.auth_token.is_some() // May have FT_IPC_TOKEN env var
        );
        client.set_token(Some("new-token".to_string()));
        assert_eq!(client.auth_token.as_deref(), Some("new-token"));
        client.set_token(None);
        assert!(client.auth_token.is_none());
    }

    #[test]
    fn now_ms_returns_reasonable_value() {
        let ms = now_ms();
        // Should be after 2020-01-01 and before 2100-01-01
        assert!(ms > 1_577_836_800_000);
        assert!(ms < 4_102_444_800_000);
    }

    #[test]
    fn elapsed_ms_returns_nonzero_after_sleep() {
        let start = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let ms = elapsed_ms(start);
        assert!(ms >= 4);
    }

    #[test]
    fn max_message_size_is_128kb() {
        assert_eq!(MAX_MESSAGE_SIZE, 131_072);
    }

    #[test]
    fn ipc_socket_name_constant() {
        assert_eq!(IPC_SOCKET_NAME, "ipc.sock");
    }

    #[test]
    fn ipc_response_skip_serializing_none_fields() {
        let response = IpcResponse::ok();
        let json = serde_json::to_string(&response).unwrap();
        // None fields should be absent, not "null"
        assert!(!json.contains("\"error\""));
        assert!(!json.contains("\"error_code\""));
        assert!(!json.contains("\"hint\""));
        assert!(!json.contains("\"data\""));
    }

    #[test]
    fn ipc_auth_multiple_tokens_finds_correct_one() {
        let auth = IpcAuth::new(vec![
            IpcAuthToken {
                token: "reader".to_string(),
                scopes: vec![IpcScope::Read],
                expires_at_ms: None,
            },
            IpcAuthToken {
                token: "writer".to_string(),
                scopes: vec![IpcScope::Write],
                expires_at_ms: None,
            },
        ]);
        assert!(auth.authorize(Some("reader"), IpcScope::Read).is_ok());
        assert!(auth.authorize(Some("reader"), IpcScope::Write).is_err());
        assert!(auth.authorize(Some("writer"), IpcScope::Write).is_ok());
        assert!(auth.authorize(Some("writer"), IpcScope::Read).is_ok());
    }
}
