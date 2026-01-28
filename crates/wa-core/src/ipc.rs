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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{RwLock, mpsc};

use crate::events::{Event, EventBus, UserVarError, UserVarPayload};
use crate::ingest::PaneRegistry;

/// Default IPC socket filename relative to workspace .wa directory.
pub const IPC_SOCKET_NAME: &str = "ipc.sock";

/// Maximum message size in bytes (128KB).
pub const MAX_MESSAGE_SIZE: usize = 131_072;

/// Maximum title length in status updates (1KB).
pub const MAX_TITLE_LENGTH: usize = 1024;

/// Current schema version for status updates.
pub const STATUS_UPDATE_SCHEMA_VERSION: u32 = 0;

/// Minimum interval between status updates for rate limiting (milliseconds).
pub const STATUS_UPDATE_MIN_INTERVAL_MS: u64 = 50;

// =============================================================================
// Status Update Schema (v0)
// =============================================================================

/// Cursor position (row and column)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorPosition {
    /// Row (0-indexed)
    pub row: u32,
    /// Column (0-indexed)
    pub col: u32,
}

/// Pane dimensions (rows and columns)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneDimensions {
    /// Number of rows
    pub rows: u32,
    /// Number of columns
    pub cols: u32,
}

/// Status update payload for pane state changes.
///
/// This is a minimal, versioned payload sent from WezTerm Lua hooks to update
/// pane state without requiring full text capture. Useful for tracking cursor
/// position, dimensions, and alt-screen state in real-time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusUpdate {
    /// Pane ID (required)
    pub pane_id: u64,

    /// Domain name (optional, e.g., "local", "SSH:hostname")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,

    /// Pane title (optional, bounded to MAX_TITLE_LENGTH)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Cursor position (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<CursorPosition>,

    /// Pane dimensions (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<PaneDimensions>,

    /// Whether pane is in alternate screen buffer (e.g., vim, less)
    #[serde(default)]
    pub is_alt_screen: bool,

    /// Whether pane is the active/focused pane (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,

    /// Timestamp in epoch milliseconds (required)
    pub ts: i64,

    /// Schema version for forward compatibility (required)
    pub schema_version: u32,
}

impl StatusUpdate {
    /// Validate the status update payload.
    ///
    /// Returns an error message if validation fails, None if valid.
    #[must_use]
    pub fn validate(&self) -> Option<String> {
        // Check schema version
        if self.schema_version > STATUS_UPDATE_SCHEMA_VERSION {
            return Some(format!(
                "unsupported schema_version {}, max supported is {}",
                self.schema_version, STATUS_UPDATE_SCHEMA_VERSION
            ));
        }

        // Check title length
        if let Some(ref title) = self.title {
            if title.len() > MAX_TITLE_LENGTH {
                return Some(format!(
                    "title exceeds max length ({} > {})",
                    title.len(),
                    MAX_TITLE_LENGTH
                ));
            }
        }

        // Validate timestamp is reasonable (not too far in future)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| i64::try_from(d.as_millis()).ok())
            .unwrap_or(0);

        // Allow up to 1 minute in the future (clock skew tolerance)
        if self.ts > now_ms + 60_000 {
            return Some("timestamp is too far in the future".to_string());
        }

        None
    }

    /// Check if this update contains a material state change compared to previous state.
    ///
    /// Material changes are:
    /// - Alt-screen toggle
    /// - Title change
    /// - Significant cursor movement (more than 1 row/col)
    #[must_use]
    pub fn is_material_change(&self, prev: Option<&Self>) -> bool {
        let Some(prev) = prev else {
            return true; // First update is always material
        };

        // Alt-screen toggle is always material
        if self.is_alt_screen != prev.is_alt_screen {
            return true;
        }

        // Title change is material
        if self.title != prev.title {
            return true;
        }

        // Dimensions change is material
        if self.dimensions != prev.dimensions {
            return true;
        }

        // Active state change is material
        if self.is_active != prev.is_active {
            return true;
        }

        false
    }
}

/// Rate limiter for status updates per pane.
///
/// Coalesces rapid updates to prevent event bus spam while ensuring
/// material state changes are not dropped.
pub struct StatusUpdateRateLimiter {
    /// Last update time per pane (epoch instant)
    last_update: HashMap<u64, Instant>,
    /// Last status update per pane (for deduplication)
    last_status: HashMap<u64, StatusUpdate>,
    /// Minimum interval between updates
    min_interval: std::time::Duration,
}

impl Default for StatusUpdateRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl StatusUpdateRateLimiter {
    /// Create a new rate limiter with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_update: HashMap::new(),
            last_status: HashMap::new(),
            min_interval: std::time::Duration::from_millis(STATUS_UPDATE_MIN_INTERVAL_MS),
        }
    }

    /// Check if an update should be processed or coalesced.
    ///
    /// Returns `Some(previous)` if update should proceed (with the previous
    /// status for comparison), or `None` if it should be dropped due to rate limiting.
    pub fn should_process(&mut self, update: &StatusUpdate) -> Option<Option<StatusUpdate>> {
        let pane_id = update.pane_id;
        let now = Instant::now();

        // Check rate limit
        if let Some(last) = self.last_update.get(&pane_id) {
            if now.duration_since(*last) < self.min_interval {
                // Within rate limit - only process if material change
                let prev = self.last_status.get(&pane_id);
                if !update.is_material_change(prev) {
                    return None; // Drop - not material and within rate limit
                }
            }
        }

        // Update tracking state
        let prev = self.last_status.get(&pane_id).cloned();
        self.last_update.insert(pane_id, now);
        self.last_status.insert(pane_id, update.clone());

        Some(prev)
    }

    /// Clear state for a pane (e.g., when pane closes).
    pub fn clear_pane(&mut self, pane_id: u64) {
        self.last_update.remove(&pane_id);
        self.last_status.remove(&pane_id);
    }
}

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
    /// Status update from WezTerm Lua hook
    StatusUpdate(StatusUpdate),
    /// Ping to check if watcher is alive
    Ping,
    /// Request current watcher status
    Status,
    /// Request pane state from watcher registry
    PaneState {
        /// Pane ID to inspect
        pane_id: u64,
    },
}

/// Response message from server to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    /// Whether the request succeeded
    pub ok: bool,
    /// Error message if failed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Additional data (for status requests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl IpcResponse {
    /// Create a success response.
    #[must_use]
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            data: None,
        }
    }

    /// Create a success response with data.
    #[must_use]
    pub fn ok_with_data(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(data),
        }
    }

    /// Create an error response.
    #[must_use]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            data: None,
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
    /// Pane registry for status updates (optional for backward compatibility)
    pub registry: Option<Arc<RwLock<PaneRegistry>>>,
    /// Rate limiter for status updates
    pub rate_limiter: Arc<std::sync::Mutex<StatusUpdateRateLimiter>>,
}

impl IpcHandlerContext {
    /// Create a new handler context with just an event bus (backward compatible).
    #[must_use]
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            event_bus,
            registry: None,
            rate_limiter: Arc::new(std::sync::Mutex::new(StatusUpdateRateLimiter::new())),
        }
    }

    /// Create a new handler context with pane registry support.
    #[must_use]
    pub fn with_registry(event_bus: Arc<EventBus>, registry: Arc<RwLock<PaneRegistry>>) -> Self {
        Self {
            event_bus,
            registry: Some(registry),
            rate_limiter: Arc::new(std::sync::Mutex::new(StatusUpdateRateLimiter::new())),
        }
    }
}

/// IPC server that runs in the watcher daemon.
pub struct IpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl IpcServer {
    /// Create and bind a new IPC server.
    ///
    /// # Arguments
    /// * `socket_path` - Path to the Unix socket file
    ///
    /// # Errors
    /// Returns error if socket binding fails.
    pub async fn bind(socket_path: impl AsRef<Path>) -> std::io::Result<Self> {
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
    pub async fn run(self, event_bus: Arc<EventBus>, mut shutdown_rx: mpsc::Receiver<()>) {
        let ctx = Arc::new(IpcHandlerContext::new(event_bus));
        self.run_with_context(ctx, &mut shutdown_rx).await;
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
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        let ctx = Arc::new(IpcHandlerContext::with_registry(event_bus, registry));
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

/// Handle a single client connection with full context.
async fn handle_client_with_context(
    stream: UnixStream,
    ctx: Arc<IpcHandlerContext>,
) -> std::io::Result<()> {
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
    let response = match serde_json::from_str::<IpcRequest>(&line) {
        Ok(request) => handle_request_with_context(request, &ctx).await,
        Err(e) => IpcResponse::error(format!("invalid request: {e}")),
    };

    // Send response
    let response_json = serde_json::to_string(&response).unwrap_or_default();
    writer.write_all(response_json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    Ok(())
}

/// Handle a parsed IPC request with full context.
async fn handle_request_with_context(request: IpcRequest, ctx: &IpcHandlerContext) -> IpcResponse {
    match request {
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
        IpcRequest::StatusUpdate(update) => handle_status_update(update, ctx).await,
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
            IpcResponse::ok_with_data(serde_json::json!({
                "uptime_ms": uptime_ms,
                "events_queued": total_queued,
                "subscriber_count": total_subscribers,
            }))
        }
        IpcRequest::PaneState { pane_id } => handle_pane_state(pane_id, ctx).await,
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

    IpcResponse::ok_with_data(serde_json::json!({
        "pane_id": pane_id,
        "known": true,
        "observed": entry.should_observe(),
        "alt_screen": entry.is_alt_screen,
        "last_status_at": entry.last_status_at,
        "in_gap": cursor.as_ref().map(|c| c.in_gap),
        "cursor_alt_screen": cursor.as_ref().map(|c| c.in_alt_screen),
    }))
}

/// Handle a status update request.
///
/// This function:
/// 1. Validates the payload
/// 2. Checks rate limits / coalesces updates
/// 3. Checks if the pane is observed (ignores updates for filtered panes)
/// 4. Updates pane registry state
/// 5. Emits an event if state changed materially
async fn handle_status_update(update: StatusUpdate, ctx: &IpcHandlerContext) -> IpcResponse {
    let pane_id = update.pane_id;

    // Step 1: Validate payload
    if let Some(err) = update.validate() {
        tracing::debug!(pane_id, error = %err, "Status update validation failed");
        return IpcResponse::error(err);
    }

    // Step 2: Check rate limit / coalesce
    let prev_update = {
        let Some(prev) = (match ctx.rate_limiter.lock() {
            Ok(mut guard) => guard.should_process(&update),
            Err(e) => {
                tracing::error!(error = %e, "Rate limiter lock poisoned");
                return IpcResponse::error("internal error: rate limiter unavailable");
            }
        }) else {
            // Dropped due to rate limiting (not material change)
            tracing::trace!(pane_id, "Status update coalesced (rate limited)");
            return IpcResponse::ok_with_data(serde_json::json!({
                "processed": false,
                "reason": "coalesced"
            }));
        };
        prev
    };

    // Step 3: Check if pane is observed
    let Some(ref registry_lock) = ctx.registry else {
        // No registry - can still emit event but can't update state
        let event = Event::StatusUpdateReceived {
            pane_id,
            alt_screen_changed: false,
            is_alt_screen: update.is_alt_screen,
            title_changed: false,
            new_title: update.title,
        };
        let _ = ctx.event_bus.publish(event);
        return IpcResponse::ok_with_data(serde_json::json!({
            "processed": true,
            "state_updated": false,
            "reason": "no registry"
        }));
    };

    // Read registry to check if pane is observed
    let registry = registry_lock.read().await;
    let entry = registry.get_entry(pane_id);

    let Some(entry) = entry else {
        // Pane not known - log warning and skip
        tracing::warn!(pane_id, "Status update for unknown pane, ignoring");
        return IpcResponse::ok_with_data(serde_json::json!({
            "processed": false,
            "reason": "unknown_pane"
        }));
    };

    if !entry.should_observe() {
        // Pane is ignored - drop update without processing
        tracing::trace!(pane_id, "Status update for ignored pane, dropping");
        return IpcResponse::ok_with_data(serde_json::json!({
            "processed": false,
            "reason": "pane_ignored"
        }));
    }

    // Drop read lock before acquiring write lock
    drop(registry);

    // Step 4: Determine what changed (from rate limiter's previous state)
    let alt_screen_changed = prev_update
        .as_ref()
        .is_none_or(|p| p.is_alt_screen != update.is_alt_screen);
    let title_changed = prev_update.as_ref().is_none_or(|p| p.title != update.title);

    // Step 5: Update pane registry state
    let dimensions = update.dimensions.map(|d| (d.cols, d.rows));
    let cursor = update.cursor.map(|c| (c.col, c.row));

    {
        let mut registry = registry_lock.write().await;
        let _state_updated = registry.update_from_status(
            pane_id,
            update.title.clone(),
            dimensions,
            cursor,
            update.is_alt_screen,
            update.ts,
        );
        // Note: _state_updated tells us if state was actually changed in registry.
        // We already track alt_screen_changed/title_changed from rate limiter state.
    }

    // Step 6: Emit event if material change
    if alt_screen_changed || title_changed {
        let event = Event::StatusUpdateReceived {
            pane_id,
            alt_screen_changed,
            is_alt_screen: update.is_alt_screen,
            title_changed,
            new_title: update.title.clone(),
        };
        let subscribers = ctx.event_bus.publish(event);
        tracing::debug!(
            pane_id,
            alt_screen_changed,
            title_changed,
            subscribers,
            "Status update event emitted"
        );
    }

    IpcResponse::ok_with_data(serde_json::json!({
        "processed": true,
        "alt_screen_changed": alt_screen_changed,
        "title_changed": title_changed
    }))
}

/// IPC client for sending requests to the watcher daemon.
pub struct IpcClient {
    socket_path: PathBuf,
}

impl IpcClient {
    /// Create a new IPC client.
    #[must_use]
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
        }
    }

    /// Check if the watcher socket exists.
    #[must_use]
    pub fn socket_exists(&self) -> bool {
        self.socket_path.exists()
    }

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

    /// Send a request and receive a response.
    async fn send_request(&self, request: IpcRequest) -> Result<IpcResponse, UserVarError> {
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
        let request_json =
            serde_json::to_string(&request).map_err(|e| UserVarError::IpcSendFailed {
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

#[cfg(test)]
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
    }

    #[test]
    fn ipc_response_error_serializes() {
        let response = IpcResponse::error("test error");
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"ok\":false"));
        assert!(json.contains("test error"));
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
    fn ipc_client_detects_missing_socket() {
        let client = IpcClient::new("/nonexistent/path/ipc.sock");
        assert!(!client.socket_exists());
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

    // ========================================================================
    // StatusUpdate schema tests (wa-4vx.2.7.1)
    // ========================================================================

    fn make_status_update(pane_id: u64) -> StatusUpdate {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        StatusUpdate {
            pane_id,
            domain: Some("local".to_string()),
            title: Some("test pane".to_string()),
            cursor: Some(CursorPosition { row: 0, col: 0 }),
            dimensions: Some(PaneDimensions { rows: 24, cols: 80 }),
            is_alt_screen: false,
            is_active: Some(true),
            ts: now_ms,
            schema_version: STATUS_UPDATE_SCHEMA_VERSION,
        }
    }

    #[test]
    fn status_update_serializes_correctly() {
        let update = make_status_update(42);
        let json = serde_json::to_string(&update).unwrap();
        assert!(json.contains("\"pane_id\":42"));
        assert!(json.contains("\"schema_version\":0"));
        assert!(json.contains("\"is_alt_screen\":false"));
    }

    #[test]
    fn status_update_deserializes_correctly() {
        let json = r#"{
            "pane_id": 42,
            "domain": "local",
            "title": "test",
            "cursor": {"row": 10, "col": 5},
            "dimensions": {"rows": 24, "cols": 80},
            "is_alt_screen": true,
            "is_active": true,
            "ts": 1700000000000,
            "schema_version": 0
        }"#;

        let update: StatusUpdate = serde_json::from_str(json).unwrap();
        assert_eq!(update.pane_id, 42);
        assert_eq!(update.domain.as_deref(), Some("local"));
        assert!(update.is_alt_screen);
        assert_eq!(update.cursor.unwrap().row, 10);
        assert_eq!(update.cursor.unwrap().col, 5);
    }

    #[test]
    fn status_update_request_serializes_with_tag() {
        let update = make_status_update(1);
        let request = IpcRequest::StatusUpdate(update);
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"type\":\"status_update\""));
        assert!(json.contains("\"pane_id\":1"));
    }

    #[test]
    fn status_update_validation_accepts_valid_payload() {
        let update = make_status_update(1);
        assert!(update.validate().is_none());
    }

    #[test]
    fn status_update_validation_rejects_future_schema_version() {
        let mut update = make_status_update(1);
        update.schema_version = STATUS_UPDATE_SCHEMA_VERSION + 1;
        let err = update.validate();
        assert!(err.is_some());
        assert!(err.unwrap().contains("unsupported schema_version"));
    }

    #[test]
    fn status_update_validation_rejects_oversized_title() {
        let mut update = make_status_update(1);
        update.title = Some("x".repeat(MAX_TITLE_LENGTH + 1));
        let err = update.validate();
        assert!(err.is_some());
        assert!(err.unwrap().contains("title exceeds max length"));
    }

    #[test]
    fn status_update_validation_rejects_future_timestamp() {
        let mut update = make_status_update(1);
        // Set timestamp 2 minutes in the future (exceeds 1 minute tolerance)
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        update.ts = now_ms + 120_000;
        let err = update.validate();
        assert!(err.is_some());
        assert!(err.unwrap().contains("timestamp is too far in the future"));
    }

    #[test]
    fn status_update_validation_accepts_max_title_length() {
        let mut update = make_status_update(1);
        update.title = Some("x".repeat(MAX_TITLE_LENGTH));
        assert!(update.validate().is_none());
    }

    // ========================================================================
    // Material change detection tests
    // ========================================================================

    #[test]
    fn material_change_first_update_is_always_material() {
        let update = make_status_update(1);
        assert!(update.is_material_change(None));
    }

    #[test]
    fn material_change_alt_screen_toggle_is_material() {
        let update1 = make_status_update(1);
        let mut update2 = make_status_update(1);
        update2.is_alt_screen = true;

        assert!(update2.is_material_change(Some(&update1)));
    }

    #[test]
    fn material_change_title_change_is_material() {
        let update1 = make_status_update(1);
        let mut update2 = make_status_update(1);
        update2.title = Some("new title".to_string());

        assert!(update2.is_material_change(Some(&update1)));
    }

    #[test]
    fn material_change_dimensions_change_is_material() {
        let update1 = make_status_update(1);
        let mut update2 = make_status_update(1);
        update2.dimensions = Some(PaneDimensions {
            rows: 48,
            cols: 120,
        });

        assert!(update2.is_material_change(Some(&update1)));
    }

    #[test]
    fn material_change_active_state_change_is_material() {
        let update1 = make_status_update(1);
        let mut update2 = make_status_update(1);
        update2.is_active = Some(false);

        assert!(update2.is_material_change(Some(&update1)));
    }

    #[test]
    fn material_change_cursor_only_is_not_material() {
        let update1 = make_status_update(1);
        let mut update2 = make_status_update(1);
        // Only change cursor position
        update2.cursor = Some(CursorPosition { row: 100, col: 50 });

        // Cursor-only changes are NOT material (to avoid spam)
        assert!(!update2.is_material_change(Some(&update1)));
    }

    #[test]
    fn material_change_identical_is_not_material() {
        let update1 = make_status_update(1);
        let update2 = make_status_update(1);

        assert!(!update2.is_material_change(Some(&update1)));
    }

    // ========================================================================
    // Rate limiter tests
    // ========================================================================

    #[test]
    fn rate_limiter_allows_first_update() {
        let mut limiter = StatusUpdateRateLimiter::new();
        let update = make_status_update(1);

        let result = limiter.should_process(&update);
        assert!(result.is_some());
        // First update has no previous
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn rate_limiter_allows_material_change_within_interval() {
        let mut limiter = StatusUpdateRateLimiter::new();
        let update1 = make_status_update(1);
        limiter.should_process(&update1);

        // Immediate follow-up with material change (alt-screen toggle)
        let mut update2 = make_status_update(1);
        update2.is_alt_screen = true;

        let result = limiter.should_process(&update2);
        assert!(result.is_some()); // Should be allowed despite rate limit
    }

    #[test]
    fn rate_limiter_drops_non_material_within_interval() {
        let mut limiter = StatusUpdateRateLimiter::new();
        let update1 = make_status_update(1);
        limiter.should_process(&update1);

        // Immediate follow-up with no material change (only cursor)
        let mut update2 = make_status_update(1);
        update2.cursor = Some(CursorPosition { row: 5, col: 10 });

        let result = limiter.should_process(&update2);
        assert!(result.is_none()); // Should be dropped
    }

    #[test]
    fn rate_limiter_allows_after_interval_expires() {
        let mut limiter = StatusUpdateRateLimiter::new();
        // Use a very short interval for testing
        limiter.min_interval = std::time::Duration::from_millis(1);

        let update1 = make_status_update(1);
        limiter.should_process(&update1);

        // Wait for interval to expire
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Non-material change after interval should be allowed
        let mut update2 = make_status_update(1);
        update2.cursor = Some(CursorPosition { row: 5, col: 10 });

        let result = limiter.should_process(&update2);
        assert!(result.is_some());
    }

    #[test]
    fn rate_limiter_tracks_multiple_panes_independently() {
        let mut limiter = StatusUpdateRateLimiter::new();

        let update_pane1 = make_status_update(1);
        let update_pane2 = make_status_update(2);

        // First update for pane 1
        let result1 = limiter.should_process(&update_pane1);
        assert!(result1.is_some());
        assert!(result1.unwrap().is_none()); // No previous

        // First update for pane 2 (should also be allowed, different pane)
        let result2 = limiter.should_process(&update_pane2);
        assert!(result2.is_some());
        assert!(result2.unwrap().is_none()); // No previous
    }

    #[test]
    fn rate_limiter_clear_pane_removes_tracking() {
        let mut limiter = StatusUpdateRateLimiter::new();

        let update = make_status_update(1);
        limiter.should_process(&update);

        // Clear pane tracking
        limiter.clear_pane(1);

        // Next update for same pane should be treated as first
        let result = limiter.should_process(&update);
        assert!(result.is_some());
        assert!(result.unwrap().is_none()); // No previous after clear
    }

    #[test]
    fn rate_limiter_returns_previous_status() {
        let mut limiter = StatusUpdateRateLimiter::new();
        limiter.min_interval = std::time::Duration::from_millis(1);

        let mut update1 = make_status_update(1);
        update1.title = Some("first".to_string());
        limiter.should_process(&update1);

        // Wait for interval
        std::thread::sleep(std::time::Duration::from_millis(5));

        let mut update2 = make_status_update(1);
        update2.title = Some("second".to_string());
        let result = limiter.should_process(&update2);

        assert!(result.is_some());
        let prev = result.unwrap();
        assert!(prev.is_some());
        assert_eq!(prev.unwrap().title.as_deref(), Some("first"));
    }

    // ========================================================================
    // IPC integration tests for StatusUpdate
    // ========================================================================

    #[tokio::test]
    async fn status_update_via_ipc_validates_payload() {
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

        // Send invalid status update (future schema version)
        let mut update = make_status_update(1);
        update.schema_version = 999;

        let request = IpcRequest::StatusUpdate(update);
        let request_json = serde_json::to_string(&request).unwrap();

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
        assert!(
            response
                .error
                .unwrap()
                .contains("unsupported schema_version")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn status_update_via_ipc_emits_event() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        // Subscribe to signal events
        let mut subscriber = event_bus.subscribe_signals();

        let server_bus = event_bus.clone();
        let server_handle = tokio::spawn(async move {
            server.run(server_bus, shutdown_rx).await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send valid status update
        let update = make_status_update(42);
        let request = IpcRequest::StatusUpdate(update);
        let request_json = serde_json::to_string(&request).unwrap();

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(request_json.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();

        let (reader, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok);

        // Verify event was emitted
        let event = subscriber.try_recv();
        assert!(event.is_some());
        let event = event.unwrap().unwrap();

        match event {
            Event::StatusUpdateReceived { pane_id, .. } => {
                assert_eq!(pane_id, 42);
            }
            _ => panic!("Expected StatusUpdateReceived event, got {:?}", event),
        }

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    // ========================================================================
    // Pane registry integration tests (wa-4vx.2.7.1)
    // ========================================================================

    #[tokio::test]
    async fn status_update_updates_pane_registry() {
        use crate::ingest::{ObservationDecision, PaneEntry, PaneFingerprint, PaneRegistry};
        use crate::wezterm::PaneInfo;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        // Create a pane registry with a test pane
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));
        {
            let mut reg = registry.write().await;
            let pane_info = PaneInfo {
                pane_id: 42,
                tab_id: 1,
                window_id: 1,
                title: Some("original title".to_string()),
                domain_id: None,
                domain_name: Some("local".to_string()),
                workspace: None,
                size: None,
                rows: Some(24),
                cols: Some(80),
                cwd: None,
                tty_name: None,
                cursor_x: None,
                cursor_y: None,
                cursor_visibility: None,
                left_col: None,
                top_row: None,
                is_active: true,
                is_zoomed: false,
                extra: std::collections::HashMap::new(),
            };
            let fingerprint = PaneFingerprint::without_content(&pane_info);
            let entry = PaneEntry::new(pane_info, fingerprint, ObservationDecision::Observed);
            reg.update(vec![entry.info]);
        }

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_registry = registry.clone();
        let server_handle = tokio::spawn(async move {
            server
                .run_with_registry(server_bus, server_registry, shutdown_rx)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send status update with new title and alt-screen state
        let mut update = make_status_update(42);
        update.title = Some("new title".to_string());
        update.is_alt_screen = true;
        update.dimensions = Some(PaneDimensions {
            rows: 50,
            cols: 120,
        });

        let request = IpcRequest::StatusUpdate(update);
        let request_json = serde_json::to_string(&request).unwrap();

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(request_json.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();

        let (reader, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok);

        // Small delay to ensure registry update completes
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Verify registry was updated
        let reg = registry.read().await;
        let entry = reg.get_entry(42).expect("Pane should exist");
        let title = entry.info.title.clone();
        let is_alt_screen = entry.is_alt_screen;
        let rows = entry.info.rows;
        let cols = entry.info.cols;
        drop(reg);
        assert_eq!(title.as_deref(), Some("new title"));
        assert!(is_alt_screen);
        assert_eq!(rows, Some(50));
        assert_eq!(cols, Some(120));

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn status_update_ignores_unknown_panes() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        // Empty registry - pane 99 doesn't exist
        let registry = Arc::new(RwLock::new(PaneRegistry::new()));

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_registry = registry.clone();
        let server_handle = tokio::spawn(async move {
            server
                .run_with_registry(server_bus, server_registry, shutdown_rx)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send status update for unknown pane
        let update = make_status_update(99);
        let request = IpcRequest::StatusUpdate(update);
        let request_json = serde_json::to_string(&request).unwrap();

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(request_json.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();

        let (reader, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok);

        // Response should indicate pane was unknown
        let data = response.data.unwrap();
        assert_eq!(
            data.get("processed").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            data.get("reason").and_then(|v| v.as_str()),
            Some("unknown_pane")
        );

        // Registry should still be empty
        assert!(registry.read().await.is_empty());

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn status_update_ignores_ignored_panes() {
        use crate::config::{PaneFilterConfig, PaneFilterRule};
        use crate::ingest::PaneRegistry;
        use crate::wezterm::PaneInfo;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let temp_dir = TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test.sock");

        // Create registry with a filter that ignores pane by title
        let filter = PaneFilterConfig {
            include: vec![],
            exclude: vec![PaneFilterRule {
                id: "ignore_test".to_string(),
                domain: None,
                title: Some("ignored pane".to_string()),
                cwd: None,
            }],
        };

        let registry = Arc::new(RwLock::new(PaneRegistry::with_filter(filter)));
        {
            let mut reg = registry.write().await;
            let pane_info = PaneInfo {
                pane_id: 42,
                tab_id: 1,
                window_id: 1,
                title: Some("ignored pane".to_string()),
                domain_id: None,
                domain_name: Some("local".to_string()),
                workspace: None,
                size: None,
                rows: Some(24),
                cols: Some(80),
                cwd: None,
                tty_name: None,
                cursor_x: None,
                cursor_y: None,
                cursor_visibility: None,
                left_col: None,
                top_row: None,
                is_active: true,
                is_zoomed: false,
                extra: std::collections::HashMap::new(),
            };
            // update() will apply the filter and mark it as ignored
            reg.update(vec![pane_info]);
        }

        let server = IpcServer::bind(&socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let server_bus = event_bus.clone();
        let server_registry = registry.clone();
        let server_handle = tokio::spawn(async move {
            server
                .run_with_registry(server_bus, server_registry, shutdown_rx)
                .await;
        });

        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Send status update for the ignored pane
        let mut update = make_status_update(42);
        update.title = Some("should not update".to_string());

        let request = IpcRequest::StatusUpdate(update);
        let request_json = serde_json::to_string(&request).unwrap();

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(request_json.as_bytes()).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();

        let (reader, _) = stream.into_split();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();

        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(response.ok);

        // Response should indicate pane was ignored
        let data = response.data.unwrap();
        assert_eq!(
            data.get("processed").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert_eq!(
            data.get("reason").and_then(|v| v.as_str()),
            Some("pane_ignored")
        );

        let _ = shutdown_tx.send(()).await;
        let _ = server_handle.await;
    }
}
