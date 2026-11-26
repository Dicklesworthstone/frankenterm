//! IPC module for watcher daemon communication.
//!
//! Provides local socket communication between CLI commands and the watcher
//! daemon. Unix uses the existing Unix-domain-socket path; Windows uses the
//! in-tree `frankenterm-uds` Windows transport.
//!
//! # Protocol
//!
//! The protocol uses JSON lines (newline-delimited JSON):
//! - Client sends: `{"type":"user_var","pane_id":1,"name":"FT_EVENT","value":"base64..."}\n`
//! - Server responds: `{"ok":true}\n` or `{"ok":false,"error":"..."}\n`

use crate::runtime_async::mpsc;
use crate::runtime_async::task::JoinSetSettlement;
use crate::runtime_async::{LockAcquireError, RwLock};
use frankenterm_alloc::{allocator_backend, jemalloc_enabled, read_allocator_stats};
use serde::{Deserialize, Serialize};
#[cfg(any(unix, windows))]
use socket_transport::{AsyncReadExt, AsyncWriteExt, UnixListener, UnixStream};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{IpcAuthToken, IpcScope, SearchConfig};
use crate::crash::HealthSnapshot;
use crate::events::{Event, EventBus, UserVarError, UserVarPayload};
use crate::ingest::PaneRegistry;

/// Default IPC socket filename relative to workspace .ft directory.
pub const IPC_SOCKET_NAME: &str = "ipc.sock";

/// Maximum message size in bytes (128KB).
/// Overridable via `[tuning.ipc] max_message_size` in ft.toml.
pub const MAX_MESSAGE_SIZE: usize = crate::tuning_config::IpcTuning::DEFAULT_MAX_MESSAGE_SIZE;
/// Storage-authoritative, epoch-aware NDJSON event subscription protocol.
pub const IPC_EVENT_STREAM_VERSION: u32 = 2;
/// One-response RPC execution budget, measured from authenticated admission.
pub const IPC_RPC_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Pause after a failed `accept()` so a persistent error (e.g. `EMFILE`)
/// cannot spin the accept loop. Matches the pacing the loop had when it
/// polled the shutdown channel for 1 ms before every accept.
#[cfg(any(unix, windows))]
const IPC_ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(1);
#[cfg(any(unix, windows))]
const IPC_CONNECTION_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
static IPC_INITIAL_REQUEST_TIMEOUT_COUNT: AtomicU64 = AtomicU64::new(0);
static IPC_WRITE_TIMEOUT_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpcConnectionTaskDrainOutcome {
    Settled,
    TimedOut {
        active_tasks: usize,
        unacknowledged_tasks: usize,
    },
    Incomplete {
        active_tasks: usize,
        unacknowledged_tasks: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpcConnectionAdmission {
    Admit,
    RejectAtCapacity,
}

fn classify_ipc_connection_admission(
    active_connections: usize,
    max_concurrent_connections: usize,
) -> IpcConnectionAdmission {
    if active_connections < max_concurrent_connections {
        IpcConnectionAdmission::Admit
    } else {
        IpcConnectionAdmission::RejectAtCapacity
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IpcLineReadDeadlineOutcome {
    Completed,
    TimedOut,
}

fn classify_ipc_line_read_deadline(timed_out: bool) -> IpcLineReadDeadlineOutcome {
    if timed_out {
        IpcLineReadDeadlineOutcome::TimedOut
    } else {
        IpcLineReadDeadlineOutcome::Completed
    }
}

fn remaining_ipc_io_budget(limit: Duration, elapsed: Duration) -> Option<Duration> {
    limit
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
}

/// The error for an `IpcClient` request whose `cx` ended one of its waits.
/// A deadline in the caller's budget keeps the label an expired I/O timeout
/// gets; any other termination is reported as a cancellation during `stage`.
#[cfg(any(unix, windows))]
fn ipc_client_wait_ended(
    cx: &crate::cx::Cx,
    stage: &str,
    detail: &dyn std::fmt::Display,
) -> UserVarError {
    use crate::outcome::CancelKind;
    let deadline = matches!(
        cx.root_cancel_cause().map(|reason| reason.kind),
        Some(CancelKind::Deadline | CancelKind::Timeout)
    );
    UserVarError::IpcSendFailed {
        message: if deadline {
            "IPC I/O deadline exceeded".to_string()
        } else {
            format!("cancelled during {stage}: {detail}")
        },
    }
}

fn record_ipc_initial_request_timeout() {
    let timeout_count = IPC_INITIAL_REQUEST_TIMEOUT_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if timeout_count.is_power_of_two() {
        tracing::warn!(
            event = "ipc_initial_request_line_timeout",
            error_class = "ipc_initial_request_deadline_exceeded",
            timeout_count,
            "IPC connection closed before its initial request line completed"
        );
    }
}

fn record_ipc_write_timeout() {
    let timeout_count = IPC_WRITE_TIMEOUT_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    if timeout_count.is_power_of_two() {
        tracing::warn!(
            event = "ipc_write_timeout",
            error_class = "ipc_write_deadline_exceeded",
            timeout_count,
            "IPC peer write failed to settle before the finite deadline"
        );
    }
}

fn classify_ipc_connection_task_drain(
    timed_out: bool,
    settlement: JoinSetSettlement,
) -> IpcConnectionTaskDrainOutcome {
    match settlement {
        JoinSetSettlement::Settled => IpcConnectionTaskDrainOutcome::Settled,
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } if timed_out => IpcConnectionTaskDrainOutcome::TimedOut {
            active_tasks,
            unacknowledged_tasks,
        },
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } => IpcConnectionTaskDrainOutcome::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        },
    }
}

#[cfg(any(unix, windows))]
mod socket_transport {
    #[cfg(unix)]
    pub use crate::runtime_async::unix::{
        AsyncReadExt, AsyncWrite, AsyncWriteExt, UnixListener, UnixStream, bind, buffered, connect,
        lines_with_max_length, next_line_with_cx,
    };

    #[cfg(windows)]
    mod windows {
        use std::io;
        use std::path::Path;
        use std::time::Duration;

        pub use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
        pub use frankenterm_uds::UnixStream;

        pub struct UnixListener {
            inner: frankenterm_uds::UnixListener,
        }

        pub type LineReader<T> = asupersync::io::Lines<BufReader<T>>;

        pub async fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            bind_with_cx(&cx, path).await
        }

        pub async fn bind_with_cx<P: AsRef<Path>>(
            cx: &crate::cx::Cx,
            path: P,
        ) -> io::Result<UnixListener> {
            cx.checkpoint().map_err(|err| {
                io::Error::new(io::ErrorKind::Interrupted, format!("bind cancelled: {err}"))
            })?;
            let inner = frankenterm_uds::UnixListener::bind(path)?;
            inner.set_nonblocking(true)?;
            Ok(UnixListener { inner })
        }

        pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            connect_with_cx(&cx, path).await
        }

        pub async fn connect_with_cx<P: AsRef<Path>>(
            cx: &crate::cx::Cx,
            path: P,
        ) -> io::Result<UnixStream> {
            cx.checkpoint().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("connect cancelled: {err}"),
                )
            })?;
            let stream = UnixStream::connect(path)?;
            stream.set_nonblocking(true)?;
            Ok(stream)
        }

        impl UnixListener {
            pub async fn accept(&self) -> io::Result<(UnixStream, ())> {
                let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                self.accept_with_cx(&cx).await
            }

            pub async fn accept_with_cx(&self, cx: &crate::cx::Cx) -> io::Result<(UnixStream, ())> {
                loop {
                    cx.checkpoint().map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::Interrupted,
                            format!("accept cancelled: {err}"),
                        )
                    })?;
                    match self.inner.accept() {
                        Ok((stream, _addr)) => {
                            stream.set_nonblocking(true)?;
                            return Ok((stream, ()));
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            crate::runtime_async::sleep_with_cx(cx, Duration::from_millis(1))
                                .await
                                .map_err(|err| {
                                    io::Error::new(
                                        io::ErrorKind::Interrupted,
                                        format!("accept wait cancelled: {err}"),
                                    )
                                })?;
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        }

        #[must_use]
        pub fn buffered<T: AsyncRead>(stream: T) -> BufReader<T> {
            BufReader::new(stream)
        }

        /// Line reader with an explicit maximum line length; mirrors
        /// `runtime_async::unix::lines_with_max_length` (ft-kccj8: the
        /// default 64 KiB `Lines::new` cap is below the IPC message
        /// limit, so every IPC read site passes its budget explicitly).
        #[must_use]
        pub fn lines_with_max_length<T>(reader: BufReader<T>, max_length: usize) -> LineReader<T>
        where
            T: AsyncRead + Unpin,
        {
            asupersync::io::Lines::new_with_max_length(reader, max_length)
        }

        pub async fn next_line_with_cx<T>(
            cx: &crate::cx::Cx,
            lines: &mut LineReader<T>,
        ) -> io::Result<Option<String>>
        where
            T: AsyncRead + Unpin,
        {
            use asupersync::stream::StreamExt;

            // As runtime_async::unix::next_line_with_cx: the read itself
            // ends on cancellation through Next::with_cx.
            let cancelled = |err: &dyn std::fmt::Display| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("next_line cancelled: {err}"),
                )
            };
            cx.checkpoint().map_err(|err| cancelled(&err))?;
            match lines.next().with_cx(cx).await {
                Ok(Some(Ok(line))) => Ok(Some(line)),
                Ok(Some(Err(err))) => Err(err),
                Ok(None) => Ok(None),
                Err(err) => Err(cancelled(&err)),
            }
        }
    }

    #[cfg(windows)]
    pub use windows::*;
}

/// Resolved IPC runtime limits derived from tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpcRuntimeLimits {
    pub max_message_size: usize,
    pub max_concurrent_connections: usize,
    pub initial_request_timeout_ms: u64,
    pub io_timeout_ms: u64,
}

impl IpcRuntimeLimits {
    fn initial_request_timeout(self) -> Duration {
        Duration::from_millis(self.initial_request_timeout_ms)
    }

    fn io_timeout(self) -> Duration {
        Duration::from_millis(self.io_timeout_ms)
    }

    fn resource_bounds_are_valid(self) -> bool {
        (crate::tuning_config::IpcTuning::MIN_MAX_MESSAGE_SIZE
            ..=crate::tuning_config::IpcTuning::MAX_MAX_MESSAGE_SIZE)
            .contains(&self.max_message_size)
            && (crate::tuning_config::IpcTuning::MIN_MAX_CONCURRENT_CONNECTIONS
                ..=crate::tuning_config::IpcTuning::MAX_MAX_CONCURRENT_CONNECTIONS)
                .contains(&self.max_concurrent_connections)
            && (crate::tuning_config::IpcTuning::MIN_INITIAL_REQUEST_TIMEOUT_MS
                ..=crate::tuning_config::IpcTuning::MAX_INITIAL_REQUEST_TIMEOUT_MS)
                .contains(&self.initial_request_timeout_ms)
            && (crate::tuning_config::IpcTuning::MIN_IO_TIMEOUT_MS
                ..=crate::tuning_config::IpcTuning::MAX_IO_TIMEOUT_MS)
                .contains(&self.io_timeout_ms)
            && crate::tuning_config::IpcTuning {
                max_message_size: self.max_message_size,
                max_concurrent_connections: self.max_concurrent_connections,
                initial_request_timeout_ms: self.initial_request_timeout_ms,
                io_timeout_ms: self.io_timeout_ms,
            }
            .aggregate_request_buffer_bytes()
            .is_some_and(|bytes| {
                bytes <= crate::tuning_config::IpcTuning::MAX_AGGREGATE_REQUEST_BUFFER_BYTES
            })
    }

    fn message_read_limit_for(max_message_size: usize) -> u64 {
        u64::try_from(Self::line_cap_for(max_message_size)).unwrap_or(u64::MAX)
    }

    /// Maximum bytes buffered for one request line: `2·max + 2`.
    ///
    /// ft-kccj8: the read bound must comfortably EXCEED the message
    /// limit so a moderately oversized line (up to 2×) is consumed in
    /// full — including its trailing newline — before the typed
    /// "message too large" reply goes out. If the server closes with
    /// unread bytes still queued, AF_UNIX delivers ECONNRESET to the
    /// peer and destroys the reply the client was about to read. Lines
    /// beyond 2× are cut off at the bound (memory stays bounded) and
    /// the reply becomes best-effort.
    fn line_cap_for(max_message_size: usize) -> usize {
        max_message_size.saturating_mul(2).saturating_add(2)
    }
}

/// Resolve IPC runtime limits from tuning, falling back to compile-time defaults.
#[must_use]
pub fn resolve_limits(tuning: Option<&crate::tuning_config::IpcTuning>) -> IpcRuntimeLimits {
    match tuning {
        Some(tuning) => IpcRuntimeLimits {
            max_message_size: tuning.max_message_size,
            max_concurrent_connections: tuning.max_concurrent_connections,
            initial_request_timeout_ms: tuning.initial_request_timeout_ms,
            io_timeout_ms: tuning.io_timeout_ms,
        },
        None => IpcRuntimeLimits {
            max_message_size: MAX_MESSAGE_SIZE,
            max_concurrent_connections:
                crate::tuning_config::IpcTuning::DEFAULT_MAX_CONCURRENT_CONNECTIONS,
            initial_request_timeout_ms:
                crate::tuning_config::IpcTuning::DEFAULT_INITIAL_REQUEST_TIMEOUT_MS,
            io_timeout_ms: crate::tuning_config::IpcTuning::DEFAULT_IO_TIMEOUT_MS,
        },
    }
}

#[cfg(any(unix, windows))]
struct BoundedIpcJsonBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

#[cfg(any(unix, windows))]
impl BoundedIpcJsonBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(4096)),
            limit,
        }
    }
}

#[cfg(any(unix, windows))]
impl std::io::Write for BoundedIpcJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if bytes.len() > remaining {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "IPC JSON exceeded the configured message size",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(any(unix, windows))]
fn serialize_ipc_json_bounded<T>(value: &T, max_message_size: usize) -> std::io::Result<Vec<u8>>
where
    T: Serialize + ?Sized,
{
    let mut output = BoundedIpcJsonBuffer::new(max_message_size);
    serde_json::to_writer(&mut output, value).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC JSON serialization failed within the configured message size",
        )
    })?;
    Ok(output.bytes)
}

#[cfg(any(unix, windows))]
fn serialize_ipc_response_bounded(response: IpcResponse, limit: usize) -> std::io::Result<Vec<u8>> {
    if let Ok(bytes) = serialize_ipc_json_bounded(&response, limit) {
        return Ok(bytes);
    }
    // The handler already recorded the actual process/effect disposition.
    // Losing a large response must never erase that receipt or suggest that
    // the command did not run. If even the receipt cannot fit, fail the write.
    let mut fallback = IpcResponse::error_with_code(
        "ipc.response_size_exceeded",
        "RPC response exceeded the configured wire limit",
        Some(
            "The command may have executed; inspect the retained RPC receipt before retrying."
                .to_string(),
        ),
    );
    fallback.rpc = response.rpc;
    if let Some(receipt) = &fallback.rpc {
        tracing::warn!(event = "ipc_rpc_response_encoding_failed", correlation_hash = %receipt.correlation_hash,
            effects = ?receipt.effects, settlement = ?receipt.settlement, "RPC response delivery exceeded the wire limit");
    }
    serialize_ipc_json_bounded(&fallback, limit)
}

#[cfg(any(unix, windows))]
async fn write_ipc_line_with_deadline<W>(
    cx: &crate::cx::Cx,
    writer: &mut W,
    io_timeout: Duration,
    max_message_size: usize,
    line: &[u8],
) -> std::io::Result<()>
where
    W: socket_transport::AsyncWrite + Unpin,
{
    if line.len() > max_message_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC outbound line exceeded the configured message size",
        ));
    }
    match crate::runtime_async::timeout_with_cx(cx, io_timeout, async {
        writer.write_all(line).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    })
    .await
    {
        Ok(result) => result,
        Err(_elapsed) => {
            record_ipc_write_timeout();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "IPC write deadline exceeded",
            ))
        }
    }
}

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

/// `now_ms()` as `i64` for NDJSON timestamp fields (matches the
/// `detected_at` / heartbeat `now` shape the DB-cursor path emits). Consumed
/// unconditionally by the search-status payload and (on unix) by the
/// `SubscribeEvents` streaming path (ft-7h5da.4.1).
fn now_ms_i64() -> i64 {
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

fn allocator_status_payload() -> serde_json::Value {
    let mut payload = serde_json::json!({
        "backend": allocator_backend().as_str(),
        "jemalloc_enabled": jemalloc_enabled(),
        "stats": serde_json::Value::Null,
    });

    match read_allocator_stats() {
        Ok(stats) => {
            payload["stats"] = serde_json::json!({
                "allocated": stats.allocated,
                "active": stats.active,
                "resident": stats.resident,
                "mapped": stats.mapped,
                "retained": stats.retained,
            });
        }
        Err(err) => {
            payload["stats_error"] = serde_json::Value::String(err.to_string());
        }
    }

    payload
}

#[derive(Debug, PartialEq, Eq)]
enum MpscRecvState<T> {
    Value(T),
    Disconnected,
    Cancelled,
}

#[cfg(not(any(unix, windows)))]
async fn mpsc_recv_state<T>(rx: &mut mpsc::Receiver<T>) -> MpscRecvState<T> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    mpsc_recv_state_with_cx(rx, &cx).await
}

async fn mpsc_recv_state_with_cx<T>(
    rx: &mut mpsc::Receiver<T>,
    cx: &crate::cx::Cx,
) -> MpscRecvState<T> {
    match rx.recv(cx).await {
        Ok(value) => MpscRecvState::Value(value),
        Err(mpsc::RecvError::Disconnected) => MpscRecvState::Disconnected,
        // `recv()` should not surface `Empty`, but treat any non-message
        // transient state as "not a shutdown signal" rather than conflating it
        // with channel closure.
        Err(mpsc::RecvError::Cancelled | mpsc::RecvError::Empty) => MpscRecvState::Cancelled,
    }
}

#[cfg(test)]
async fn mpsc_send_value<T>(tx: &mpsc::Sender<T>, value: T) -> Result<(), mpsc::SendError<T>> {
    let cx = crate::cx::for_testing();
    match tx.reserve(&cx).await {
        Ok(permit) => {
            permit.send(value);
            Ok(())
        }
        Err(mpsc::SendError::Disconnected(())) => Err(mpsc::SendError::Disconnected(value)),
        Err(mpsc::SendError::Cancelled(())) => Err(mpsc::SendError::Cancelled(value)),
        Err(mpsc::SendError::Full(())) => Err(mpsc::SendError::Full(value)),
    }
}

fn resolve_search_index_dir(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(stripped) = trimmed.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(trimmed)
}

fn effective_search_indexing_config(search_config: &SearchConfig) -> crate::search::IndexingConfig {
    let mut effective = crate::search::IndexingConfig::default();
    let indexing = &search_config.indexing;

    if !indexing.index_dir.trim().is_empty() {
        effective.index_dir = resolve_search_index_dir(&indexing.index_dir);
    }
    effective.max_index_size_bytes = indexing.max_index_mb.saturating_mul(1024 * 1024);
    effective.ttl_days = indexing.ttl_days;
    effective.flush_interval_secs = indexing.flush_interval_secs;
    effective.flush_docs_threshold = indexing.flush_docs_threshold;
    effective.max_docs_per_second = indexing.max_docs_per_second;
    effective
}

fn search_embedder_tiers(search_config: &SearchConfig) -> Vec<String> {
    crate::search::advertised_embedder_tiers(search_config.enabled, search_config.reranker_enabled)
}

fn build_search_status_payload(
    search_config: Option<&SearchConfig>,
) -> (serde_json::Value, serde_json::Value) {
    let Some(search_config) = search_config else {
        return (
            serde_json::Value::Null,
            serde_json::json!({
                "freshness_secs": serde_json::Value::Null,
                "indexing_error_count": 0,
                "last_error": serde_json::Value::Null,
                "background_job_status": "paused",
            }),
        );
    };

    let background_job_status = if search_config.daemon.enabled {
        "idle"
    } else {
        "paused"
    };

    let index_config = effective_search_indexing_config(search_config);
    match crate::search::SearchIndex::open(index_config) {
        Ok(index) => {
            let stats = index.stats(now_ms_i64());
            let last_update_ts = stats.newest_captured_at_ms.or(stats.last_flush_at_ms);
            let freshness_secs = stats.freshness_age_ms.map(|ms| ms.max(0) / 1000);
            let search_index = serde_json::json!({
                "index_size_bytes": stats.total_bytes,
                "document_count": stats.doc_count,
                "last_update_ts": last_update_ts,
                "embedder_tiers_available": search_embedder_tiers(search_config),
                "segment_count": stats.segment_count,
            });
            let search_health = serde_json::json!({
                "freshness_secs": freshness_secs,
                "indexing_error_count": 0,
                "last_error": serde_json::Value::Null,
                "background_job_status": background_job_status,
            });
            (search_index, search_health)
        }
        Err(err) => {
            let search_health = serde_json::json!({
                "freshness_secs": serde_json::Value::Null,
                "indexing_error_count": 1,
                "last_error": err.to_string(),
                "background_job_status": background_job_status,
            });
            (serde_json::Value::Null, search_health)
        }
    }
}

#[cfg(unix)]
fn remove_stale_socket_path(socket_path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => std::fs::remove_file(socket_path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to remove non-socket IPC bind path: {}",
                socket_path.display()
            ),
        )),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(windows)]
fn remove_stale_socket_path(socket_path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("IPC socket path already exists: {}", socket_path.display()),
    ))
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
    /// Ask the owning watcher instance to perform one cooperative control
    /// action. The expected instance identity is checked by the watcher before
    /// it acknowledges or enqueues the action; it is never interpreted as a
    /// numeric process identifier.
    WatcherControl {
        /// Strict typed control payload.
        control: WatcherControlRequest,
    },
    /// RPC request forwarded to robot handlers.
    Rpc {
        /// Robot command arguments (e.g., ["state"] or ["send", "1", "ls"]).
        args: Vec<String>,
    },
    /// Subscribe to a live NDJSON stream of watch events over IPC. Unlike every
    /// other request, this does NOT return a single `IpcResponse` — the server
    /// keeps the connection open and drains selected stored rows in ascending
    /// ID order as redacted NDJSON. EventBus notifications only wake that drain.
    ///
    /// Persisted detection records carry `cursor = <storage event id>` and
    /// `cursor_epoch` (the history identity used by DB fallback), so a follower can
    /// dedupe across transports and resume on the DB path if the watcher dies
    /// mid-stream. Non-persisted live signal records carry `cursor = null`;
    /// they are live notifications, not DB-resumable history. Idle heartbeats
    /// keep the stream live and let the server notice a closed consumer. This
    /// is the low-latency transport for `ft robot watch-events --follow`; the
    /// CLI falls back to storage-cursor tailing when the watcher is down.
    SubscribeEvents {
        /// Required wire version; unsupported versions fail closed.
        version: u32,
        /// Resume after this durable ID in its storage epoch; None starts at the tail.
        cursor: Option<crate::event_stream::DurableStreamCursor>,
        /// Restrict to a single pane (server-side filter).
        pane: Option<u64>,
        /// Severity filter (case-insensitive exact, e.g. "warning").
        severity: Option<String>,
        /// Rule-id `*`-glob filter (e.g. "codex.*").
        rule_id: Option<String>,
        /// Idle heartbeat interval in ms (0 disables heartbeats).
        heartbeat_interval_ms: u64,
    },
}

impl IpcRequest {
    #[must_use]
    fn required_scope(&self) -> IpcScope {
        match self {
            Self::UserVar { .. } => IpcScope::Write,
            Self::Ping | Self::Status | Self::PaneState { .. } => IpcScope::Read,
            Self::SubscribeEvents { .. } => IpcScope::Read,
            Self::SetPanePriority { .. }
            | Self::ClearPanePriority { .. }
            | Self::WatcherControl { .. } => IpcScope::Write,
            Self::Rpc { args } => rpc_required_scope(args),
        }
    }
}

/// Cooperative watcher action carried over the local IPC transport.
///
/// The socket's filesystem policy and `IpcScope::Write` gate always apply;
/// token authentication applies when the endpoint is configured with tokens.
/// The watcher handler independently binds the request to one exact lock
/// acquisition through [`WatcherControlRequest::expected_instance_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherControlAction {
    /// Begin the watcher's ordinary receipt-producing graceful shutdown.
    Stop,
    /// Reload the hot-reloadable configuration in the owning watcher.
    Reload,
}

/// Strict watcher-control request.
///
/// `expected_instance_id` is the collision-resistant identity from the exact
/// holder-locked [`crate::lock::LockMetadata`] record admitted by the caller.
/// The watcher compares it with its own acquired guard before accepting the
/// action. A PID is intentionally absent: a numeric PID can be recycled after
/// any userspace preflight check and therefore cannot authorize a signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatcherControlRequest {
    /// Requested cooperative action.
    pub action: WatcherControlAction,
    /// Exact per-acquisition watcher instance identity expected by the caller.
    pub expected_instance_id: String,
}

// This classifier runs before the CLI parser and before the RPC handler is
// invoked. Read authority is an explicit allowlist: a command family may also
// contain mutations, and global flags can precede its subcommand.
fn rpc_required_scope(args: &[String]) -> IpcScope {
    let Some(cmd) = args.first().map(|value| value.trim().to_ascii_lowercase()) else {
        return IpcScope::Write;
    };
    let subcmd = args.get(1).map(|value| value.trim().to_ascii_lowercase());

    match cmd.as_str() {
        "help" | "quickstart" | "quick-start" | "state" | "status" | "panes" | "get-text"
        | "gettext" | "read" | "tail" | "wait-for" | "waitfor" | "watch" | "search" | "find"
        | "grep" | "search-explain" | "searchexplain" | "why" | "health" => IpcScope::Read,
        "events" | "event-log" | "eventlog" => {
            if rpc_events_query_is_read_only(&args[1..]) {
                IpcScope::Read
            } else {
                IpcScope::Write
            }
        }
        "agents" => match subcmd.as_deref() {
            // Detection refresh only replaces the child's in-memory cache.
            // Configure writes project/global integration files, including
            // through this otherwise read-oriented command family.
            Some("list" | "running" | "detect") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        "cass" => match subcmd.as_deref() {
            Some("search" | "view" | "status") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        "rules" => match subcmd.as_deref() {
            Some("list" | "test" | "show" | "lint") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        "workflow" => match subcmd.as_deref() {
            Some("status" | "list") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        "accounts" | "reservations" => match subcmd.as_deref() {
            Some("list") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        "mission" => match subcmd.as_deref() {
            Some("state" | "decisions") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        "tx" => match subcmd.as_deref() {
            Some("show" | "plan") => IpcScope::Read,
            _ => IpcScope::Write,
        },
        _ => IpcScope::Write,
    }
}

/// Recognize only the finite event-query shape. The CLI remains responsible
/// for value validation. Any positional token or unknown option requires
/// write authority rather than hiding a mutation behind an intervening flag.
fn rpc_events_query_is_read_only(args: &[String]) -> bool {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--verbose"
            || arg
                .strip_prefix('-')
                .is_some_and(|flags| !flags.is_empty() && flags.bytes().all(|flag| flag == b'v'))
        {
            continue;
        }
        let (option, inline_value) = arg
            .split_once('=')
            .map_or((arg.as_str(), None), |(option, value)| {
                (option, Some(value))
            });
        match option {
            "--unhandled" | "--unhandled-only" | "--start-at-tail" | "--would-handle"
            | "--dry-run"
                if inline_value.is_none() => {}
            "--limit" | "--pane" | "--rule-id" | "--event-type" | "--triage-state" | "--label"
            | "--since" | "--cursor" | "--cursor-epoch" | "--cursor-scope" | "--replay-limit" => {
                // Missing values are not a read-only command shape. Do not
                // reinterpret a consumed value as a subcommand: filters may
                // legitimately equal "annotate", "triage", or "label".
                if inline_value.is_none() && args.next().is_none() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
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
    /// Authoritative subprocess/audit observations for RPC requests only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc: Option<IpcRpcReceipt>,
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
            rpc: None,
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
            rpc: None,
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
            rpc: None,
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
            rpc: None,
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

/// Parsed RPC request forwarded to a registered handler.
pub struct IpcRpcRequest {
    pub args: Vec<String>,
    pub request_id: Option<String>,
    /// The authentic admitted connection context, including its capabilities.
    pub cx: crate::cx::Cx,
    /// Absolute monotonic deadline. Queuing never renews this budget.
    pub deadline: Instant,
    /// Request-local cancellation, additive to Cx cancellation.
    pub cancellation: crate::runtime_async::process::CommandCancellation,
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcRpcAdmission {
    NotStarted,
    Started,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcRpcSettlement {
    NotStarted,
    Exited,
    Terminated,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcRpcEffects {
    NotAdmitted,
    /// The child returned a complete response. This is not rollback proof or
    /// independent verification of an external service's state.
    ResponseReported,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IpcRpcAuditDelivery {
    Confirmed,
    Unconfirmed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcRpcReceipt {
    pub correlation_hash: String,
    pub child_identity_hash: Option<String>,
    pub admission: IpcRpcAdmission,
    pub settlement: IpcRpcSettlement,
    pub effects: IpcRpcEffects,
    pub audit_delivery: IpcRpcAuditDelivery,
    pub stdout_bytes: Option<usize>,
    pub stderr_bytes: Option<usize>,
}

pub type IpcRpcHandler = Arc<
    dyn Fn(
            IpcRpcRequest,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = IpcResponse> + Send>>
        + Send
        + Sync,
>;

/// Parsed watcher-control invocation forwarded to the watcher-owned handler.
pub struct IpcWatcherControlInvocation {
    /// Strict action and expected instance identity from the wire request.
    pub control: WatcherControlRequest,
    /// Optional caller correlation identity from the IPC envelope.
    pub request_id: Option<String>,
}

/// Watcher-owned cooperative control handler.
///
/// Keeping this distinct from [`IpcRpcHandler`] prevents a generic robot argv
/// vector from impersonating a control request inside the server. Transport
/// authentication and `IpcScope::Write` authorization occur before this
/// callback, while the callback remains responsible for exact watcher-instance
/// matching and bounded queue admission. Socket-policy authorization and any
/// configured token authentication occur before this callback.
pub type IpcWatcherControlHandler = Arc<
    dyn Fn(
            IpcWatcherControlInvocation,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = IpcResponse> + Send>>
        + Send
        + Sync,
>;

/// Constant-time byte-wise equality over two strings.
///
/// Short-circuiting `==` on `String`/`&str` leaks byte-prefix equality via
/// timing, which over a Unix socket is enough for a co-tenant process to
/// recover an IPC auth token character by character. This helper always
/// scans the longer of the two inputs and ORs every byte difference into a
/// single accumulator, so the elapsed time is independent of where (or
/// whether) a mismatch occurs. Mirrors `distributed::constant_time_eq`.
fn ipc_constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let max_len = a.len().max(b.len());
    let mut diff = a.len() ^ b.len();
    for idx in 0..max_len {
        let left = a.get(idx).copied().unwrap_or(0);
        let right = b.get(idx).copied().unwrap_or(0);
        diff |= usize::from(left ^ right);
    }
    diff == 0
}

/// Token-based authorization gate for IPC socket connections.
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

        let presented = token.ok_or(IpcAuthError::MissingToken)?; // ubs:ignore - request-supplied IPC credential, not a hardcoded secret.
        // [sec] Constant-time scan so a remote attacker cannot recover the
        // stored credential byte by byte via timing. A short-circuiting lookup
        // previously short-circuited on the first byte-mismatch in the first
        // candidate AND stopped iterating on the first candidate match — both
        // leaks are closed by always iterating every candidate and always
        // comparing every byte. The early-expiry/scope check is deferred until
        // after the match so the presence-of-match decision itself is not
        // leaked by subsequent branches.
        let mut matched: Option<&IpcAuthToken> = None;
        for candidate in &self.tokens {
            let configured = candidate.token.as_str(); // ubs:ignore - configured IPC credential field, not a hardcoded secret.
            if ipc_constant_time_eq(configured, presented) {
                // Don't break — keep scanning the rest of the vector so the
                // authorize() runtime is independent of which candidate (if
                // any) matched.
                if matched.is_none() {
                    matched = Some(candidate);
                }
            }
        }
        let record = matched.ok_or(IpcAuthError::InvalidToken)?;

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

// ── Fuzz seams ───────────────────────────────────────────────────────
//
// [sec] Narrow, `#[doc(hidden)]` public wrappers expose the IPC auth
// pipeline for fuzz harnesses in `fuzz/fuzz_targets/`. They are NOT
// part of the public API — `#[doc(hidden)]` keeps them out of rustdoc,
// and they return `&'static str` status tokens rather than exposing
// `IpcAuthError` internals so the fuzz harness can't accidentally
// become a public dependency on the private error enum.

/// [fuzz seam] Constant-time equality over raw strings.
///
/// Exposed so `fuzz/fuzz_targets/ipc_auth_envelope.rs` can exercise
/// `ipc_constant_time_eq` directly — malformed UTF-8 is already
/// impossible at this boundary (both args are `&str`), but the fuzz
/// harness explores every length, byte, and mismatch position in the
/// operand space to pin the no-panic invariant and the length-difference
/// short-circuit that would otherwise leak timing.
#[doc(hidden)]
#[must_use]
pub fn __fuzz_constant_time_eq(a: &str, b: &str) -> bool {
    ipc_constant_time_eq(a, b)
}

/// [fuzz seam] End-to-end parse + authorize pipeline.
///
/// Mirrors the wire-format path inside `handle_client_with_context_with_cx`:
/// take a line of bytes, parse as `IpcEnvelope`, run `IpcAuth::authorize`
/// with the envelope's token and the request's required scope, and
/// return a short status token. Returns one of:
///
/// - `"not_utf8"`   — bytes aren't valid UTF-8.
/// - `"parse_err"`  — `serde_json` rejected the envelope (malformed JSON,
///   missing required fields, unknown tag, etc.).
/// - `"allowed"`    — auth succeeded.
/// - `"missing"`    — envelope omitted `token` but auth has tokens configured.
/// - `"invalid"`    — token present but no candidate matched.
/// - `"expired"`    — token matched but `expires_at_ms` is in the past.
/// - `"scope"`      — token matched but scopes don't allow the request.
///
/// The fuzz harness never asserts on a particular return value — it
/// asserts on the absence of panics and that the same bytes + same auth
/// always produce the same status (determinism).
#[doc(hidden)]
#[must_use]
pub fn __fuzz_parse_envelope_and_authorize(line: &[u8], auth: &IpcAuth) -> &'static str {
    let s = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return "not_utf8",
    };
    let envelope: IpcEnvelope = match serde_json::from_str(s) {
        Ok(e) => e,
        Err(_) => return "parse_err",
    };
    let presented = envelope.token.as_deref(); // ubs:ignore - validates request credential, not a literal secret.
    match auth.authorize(presented, envelope.request.required_scope()) {
        Ok(()) => "allowed",
        Err(IpcAuthError::MissingToken) => "missing",
        Err(IpcAuthError::InvalidToken) => "invalid",
        Err(IpcAuthError::ExpiredToken) => "expired",
        Err(IpcAuthError::InsufficientScope { .. }) => "scope",
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
            Self::MissingToken => "missing auth token".to_string(), // ubs:ignore - static auth error text, not a credential.
            Self::InvalidToken => "invalid auth token".to_string(), // ubs:ignore - static auth error text, not a credential.
            Self::ExpiredToken => "auth token expired".to_string(), // ubs:ignore - static auth error text, not a credential.
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
    /// Durable event source for ordered, resumable subscriptions.
    pub event_storage: Option<Arc<crate::storage::StorageHandle>>,
    /// Pane registry for pane state queries (optional for backward compatibility)
    pub registry: Option<Arc<RwLock<PaneRegistry>>>,
    /// Optional IPC auth configuration
    pub auth: Option<IpcAuth>,
    /// Optional RPC handler (robot/MCP parity).
    pub rpc_handler: Option<IpcRpcHandler>,
    /// Optional watcher-owned cooperative stop/reload handler.
    pub watcher_control_handler: Option<IpcWatcherControlHandler>,
    /// Optional search configuration for IPC status enrichment.
    pub search_config: Option<SearchConfig>,
    /// Optional mux handle for live prompt evidence in `pane_state`.
    pub semantic_source: Option<crate::wezterm::WeztermHandle>,
    connection_shutdown: crate::runtime_async::ShutdownSignal,
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
            watcher_control_handler: None,
            search_config: None,
            semantic_source: None,
            event_storage: None,
            connection_shutdown: crate::runtime_async::ShutdownSignal::new(),
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
            watcher_control_handler: None,
            search_config: None,
            semantic_source: None,
            event_storage: None,
            connection_shutdown: crate::runtime_async::ShutdownSignal::new(),
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
            watcher_control_handler: None,
            search_config: None,
            semantic_source: None,
            event_storage: None,
            connection_shutdown: crate::runtime_async::ShutdownSignal::new(),
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
        Self::with_auth_rpc_and_search_config(event_bus, registry, auth, rpc_handler, None)
    }

    /// Create a new handler context with optional auth, RPC handler, and search config.
    #[must_use]
    pub fn with_auth_rpc_and_search_config(
        event_bus: Arc<EventBus>,
        registry: Option<Arc<RwLock<PaneRegistry>>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        search_config: Option<SearchConfig>,
    ) -> Self {
        Self::with_auth_rpc_control_and_search_config(
            event_bus,
            registry,
            auth,
            rpc_handler,
            None,
            search_config,
        )
    }

    /// Create a handler context with optional auth, RPC, watcher control, and
    /// search configuration.
    #[must_use]
    pub fn with_auth_rpc_control_and_search_config(
        event_bus: Arc<EventBus>,
        registry: Option<Arc<RwLock<PaneRegistry>>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        watcher_control_handler: Option<IpcWatcherControlHandler>,
        search_config: Option<SearchConfig>,
    ) -> Self {
        Self {
            event_bus,
            registry,
            auth,
            rpc_handler,
            watcher_control_handler,
            search_config,
            semantic_source: None,
            event_storage: None,
            connection_shutdown: crate::runtime_async::ShutdownSignal::new(),
        }
    }

    /// Enable versioned durable event subscriptions against the watcher store.
    #[must_use]
    pub fn with_event_storage(mut self, storage: Arc<crate::storage::StorageHandle>) -> Self {
        self.event_storage = Some(storage);
        self
    }

    /// Answer `pane_state` with live prompt evidence read from this mux.
    #[must_use]
    pub fn with_semantic_source(mut self, mux: crate::wezterm::WeztermHandle) -> Self {
        self.semantic_source = Some(mux);
        self
    }
}

/// Budget for the live semantic-zone read behind one `pane_state` reply.
const PANE_STATE_SEMANTIC_BUDGET: Duration = Duration::from_millis(750);

/// Live prompt evidence for a `pane_state` reply, or `None` without a mux.
async fn pane_state_live_prompt(
    cx: &crate::cx::Cx,
    pane_id: u64,
    ctx: &IpcHandlerContext,
) -> Option<serde_json::Value> {
    let mux = ctx.semantic_source.as_ref()?;
    let evidence = crate::pane_capability_resolution::fetch_live_prompt_evidence(
        cx,
        mux,
        pane_id,
        PANE_STATE_SEMANTIC_BUDGET,
    )
    .await;
    serde_json::to_value(evidence).ok()
}

/// IPC server that runs in the watcher daemon.
#[cfg(any(unix, windows))]
pub struct IpcServer {
    socket_path: PathBuf,
    listener: UnixListener,
    limits: IpcRuntimeLimits,
}

#[cfg(any(unix, windows))]
impl IpcServer {
    /// Create and bind a new IPC server with default permissions (0o600).
    ///
    /// # Arguments
    /// * `socket_path` - Path to the Unix socket file
    ///
    /// # Errors
    /// Returns error if socket binding fails.
    pub async fn bind(socket_path: impl AsRef<Path>) -> std::io::Result<Self> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::bind_with_cx(&cx, socket_path).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`bind`].
    ///
    /// Pre-flight checkpoint gates IPC server bind before any
    /// filesystem mutation (stale socket removal, parent dir
    /// creation) or listener setup. CLI startup paths that are
    /// already cancelled can bail before touching the socket
    /// directory.
    pub async fn bind_with_cx(
        cx: &crate::cx::Cx,
        socket_path: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        Self::bind_with_permissions_with_cx(cx, socket_path, Some(0o600)).await
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::bind_with_permissions_with_cx(&cx, socket_path, permissions).await
    }

    /// Create and bind a new IPC server with explicit permissions and tuning limits.
    ///
    /// # Errors
    /// Returns error if socket binding or permission setting fails.
    pub async fn bind_with_permissions_and_limits_with_cx(
        cx: &crate::cx::Cx,
        socket_path: impl AsRef<Path>,
        permissions: Option<u32>,
        limits: IpcRuntimeLimits,
    ) -> std::io::Result<Self> {
        Self::bind_with_permissions_limits_with_cx(cx, socket_path, permissions, limits).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`bind_with_permissions`].
    ///
    /// Checkpoint seams before each filesystem/syscall boundary:
    /// (1) pre-flight, (2) before stale-socket removal, (3)
    /// before parent-dir creation, (4) before the actual bind.
    /// Responsive cancellation during socket setup — a cx-driven
    /// CLI that was cancelled mid-startup won't leave behind
    /// empty parent dirs or touch files it doesn't need to.
    pub async fn bind_with_permissions_with_cx(
        cx: &crate::cx::Cx,
        socket_path: impl AsRef<Path>,
        permissions: Option<u32>,
    ) -> std::io::Result<Self> {
        Self::bind_with_permissions_limits_with_cx(
            cx,
            socket_path,
            permissions,
            resolve_limits(None),
        )
        .await
    }

    async fn bind_with_permissions_limits_with_cx(
        cx: &crate::cx::Cx,
        socket_path: impl AsRef<Path>,
        permissions: Option<u32>,
        limits: IpcRuntimeLimits,
    ) -> std::io::Result<Self> {
        cx.checkpoint().map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("ipc bind cancelled: {err}"),
            )
        })?;

        if !limits.resource_bounds_are_valid() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid IPC connection or line-read resource limits",
            ));
        }

        let socket_path = socket_path.as_ref().to_path_buf();

        match std::fs::symlink_metadata(&socket_path) {
            Ok(_) => {
                cx.checkpoint().map_err(|err| {
                    std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        format!("ipc bind cancelled before stale-socket removal: {err}"),
                    )
                })?;
                remove_stale_socket_path(&socket_path)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        if let Some(parent) = socket_path.parent() {
            cx.checkpoint().map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    format!("ipc bind cancelled before parent-dir creation: {err}"),
                )
            })?;
            std::fs::create_dir_all(parent)?;
        }

        cx.checkpoint().map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("ipc bind cancelled before listener bind: {err}"),
            )
        })?;

        let listener = socket_transport::bind(&socket_path).await?;
        #[cfg(windows)]
        let _ = permissions;
        #[cfg(unix)]
        if let Some(mode) = permissions {
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(&socket_path, perms)?;
        }
        tracing::info!(path = %socket_path.display(), "IPC server listening (cx-first)");

        Ok(Self {
            socket_path,
            listener,
            limits,
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_cx(&cx, event_bus, shutdown_rx).await;
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
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_registry_with_cx(&cx, event_bus, registry, shutdown_rx)
            .await;
    }

    /// Run the IPC server with optional auth configuration.
    pub async fn run_with_auth(
        self,
        event_bus: Arc<EventBus>,
        auth: Option<IpcAuth>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        // Keep one request-rooted Cx for the complete server lifetime.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_auth_with_cx(&cx, event_bus, auth, shutdown_rx)
            .await;
    }

    /// Run the IPC server with registry and optional auth configuration.
    pub async fn run_with_registry_and_auth(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_registry_and_auth_with_cx(&cx, event_bus, registry, auth, shutdown_rx)
            .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_with_context`].
    ///
    /// Threads the caller's cx into the accept loop:
    /// - each iteration waits once, on the shutdown channel, the caller's
    ///   cancellation and the next client together (see
    ///   [`next_accept_loop_wake`]), so an idle server arms no short timer.
    /// - Socket-cleanup is always run (safety invariant) so a
    ///   cancelled cx cannot leak the socket file.
    async fn run_with_context_with_cx(
        self,
        cx: &crate::cx::Cx,
        ctx: Arc<IpcHandlerContext>,
        shutdown_rx: &mut mpsc::Receiver<()>,
    ) {
        let mut connection_tasks = crate::runtime_async::task::JoinSet::new();
        let mut rejected_connections_total = 0_u64;
        let limits = self.limits;

        loop {
            if cx.checkpoint().is_err() {
                tracing::info!("IPC server shutting down on cx cancellation");
                break;
            }

            // Client arrival, handler completion, shutdown and context
            // termination all wake this wait directly. An empty server and
            // healthy pending handlers need no periodic reaping timer.
            match next_accept_loop_wake(
                cx,
                shutdown_rx,
                self.listener.accept(),
                &mut connection_tasks,
            )
            .await
            {
                AcceptLoopWake::Shutdown => {
                    tracing::info!("IPC server shutting down (cx-first)");
                    break;
                }
                AcceptLoopWake::ContextFailed(failure_class) => {
                    tracing::info!(?failure_class, "IPC server context ended");
                    break;
                }
                AcceptLoopWake::ConnectionFinished(Some(Err(error))) => {
                    tracing::debug!(
                        error_class = "ipc_client_task_join_failed",
                        failure_class = ?error.kind(),
                        "IPC client task failed"
                    );
                }
                AcceptLoopWake::ConnectionFinished(_) => {}
                AcceptLoopWake::ConnectionWaitFailed(error) => {
                    tracing::warn!(
                        failure_class = ?error.kind(),
                        "IPC connection completion wait failed"
                    );
                    break;
                }
                AcceptLoopWake::Accepted(Ok((stream, _addr))) => {
                    if cx.checkpoint().is_err() {
                        // The accept can become ready in the same scheduler
                        // turn as cancellation. Do not admit a fresh client
                        // task after the server's cancellation boundary.
                        drop(stream);
                        break;
                    }

                    // Reap terminal handlers before making the admission
                    // decision. JoinSet retains completed entries until they
                    // are joined, so using its raw length first would reject
                    // healthy clients after short-lived requests complete.
                    while let Some(join_result) = connection_tasks.try_join_next() {
                        if join_result.is_err() {
                            tracing::debug!(
                                error_class = "ipc_client_task_join_failed",
                                "IPC client task failed"
                            );
                        }
                    }

                    match classify_ipc_connection_admission(
                        connection_tasks.len(),
                        limits.max_concurrent_connections,
                    ) {
                        IpcConnectionAdmission::Admit => {
                            let ctx = ctx.clone();
                            let active_before_spawn = connection_tasks.len();
                            connection_tasks.spawn_with_cx(cx, move |child_cx| async move {
                                if handle_client_with_context_with_cx(child_cx, stream, ctx, limits)
                                    .await
                                    .is_err()
                                {
                                    tracing::warn!(
                                        error_class = "ipc_client_handler_failed",
                                        "IPC client error"
                                    );
                                }
                            });
                            debug_assert_eq!(
                                connection_tasks.len(),
                                active_before_spawn.saturating_add(1)
                            );
                            debug_assert!(
                                connection_tasks.len() <= limits.max_concurrent_connections
                            );
                        }
                        IpcConnectionAdmission::RejectAtCapacity => {
                            // Do not enqueue a task or retain the accepted FD.
                            // Telemetry is deliberately fixed/content-free: an
                            // unauthenticated peer cannot inject log content.
                            // Emit at powers of two so a connection storm
                            // cannot turn this guardrail into a log-volume DoS.
                            rejected_connections_total =
                                rejected_connections_total.saturating_add(1);
                            if rejected_connections_total.is_power_of_two() {
                                tracing::warn!(
                                    event = "ipc_connection_rejected_at_capacity",
                                    error_class = "ipc_connection_capacity_exhausted",
                                    rejected_connections_total,
                                    "IPC connection rejected at capacity"
                                );
                            }
                            drop(stream);
                        }
                    }
                }
                AcceptLoopWake::Accepted(Err(e)) => {
                    tracing::error!(error_kind = ?e.kind(), "Failed to accept IPC connection");
                    let _ = crate::runtime_async::sleep_with_cx(cx, IPC_ACCEPT_ERROR_BACKOFF).await;
                }
            }

            while let Some(join_result) = connection_tasks.try_join_next() {
                if join_result.is_err() {
                    tracing::debug!(
                        error_class = "ipc_client_task_join_failed",
                        "IPC client task failed"
                    );
                }
            }
        }

        // RPC owners must observe cancellation and finish bounded process
        // settlement before their connection future can be dropped.
        ctx.connection_shutdown.request();
        let drain_cx = crate::cx::for_request();
        let cooperative_drain = crate::runtime_async::timeout_with_cx(
            &drain_cx,
            IPC_CONNECTION_TASK_DRAIN_TIMEOUT,
            async {
                loop {
                    match connection_tasks.drain_next_with_cx(&drain_cx).await {
                        Ok(Some(_)) => {}
                        Ok(None) => return Ok(()),
                        Err(error) => return Err(error),
                    }
                }
            },
        )
        .await;
        if !matches!(cooperative_drain, Ok(Ok(()))) {
            tracing::warn!(
                event = "ipc_cooperative_drain_incomplete",
                process_settlement = "indeterminate",
                orphan_risk = true,
                "IPC cooperative settlement failed; abort acknowledgement cannot prove child settlement"
            );
        }
        connection_tasks.abort_all();
        let drain_result = crate::runtime_async::timeout_with_cx(
            &drain_cx,
            IPC_CONNECTION_TASK_DRAIN_TIMEOUT,
            async {
                loop {
                    match connection_tasks.drain_next_with_cx(&drain_cx).await {
                        Ok(Some(join_result)) => {
                            if join_result.is_err() {
                                tracing::debug!(
                                    error_class = "ipc_client_task_join_failed_during_shutdown",
                                    "IPC client task failed during shutdown"
                                );
                            }
                        }
                        Ok(None) => return Ok(()),
                        Err(drain_error) => return Err(drain_error),
                    }
                }
            },
        )
        .await;
        if let Ok(Err(drain_error)) = &drain_result {
            tracing::error!(
                failure_class = ?drain_error.kind(),
                "IPC connection task drain context failed before terminal settlement"
            );
        }
        match classify_ipc_connection_task_drain(
            drain_result.is_err(),
            connection_tasks.settlement(),
        ) {
            IpcConnectionTaskDrainOutcome::Settled => {
                tracing::debug!(
                    event = "ipc_connection_tasks_settled",
                    "IPC connection task cancellation reached terminal acknowledgement"
                );
            }
            IpcConnectionTaskDrainOutcome::TimedOut {
                active_tasks,
                unacknowledged_tasks,
            } => {
                tracing::error!(
                    event = "ipc_connection_task_drain_timeout",
                    active_tasks,
                    unacknowledged_tasks,
                    remaining_tasks = connection_tasks.len(),
                    orphan_risk = true,
                    "IPC connection cancellation did not reach terminal acknowledgement before the bounded drain timeout"
                );
            }
            IpcConnectionTaskDrainOutcome::Incomplete {
                active_tasks,
                unacknowledged_tasks,
            } => {
                tracing::error!(
                    event = "ipc_connection_task_settlement_incomplete",
                    active_tasks,
                    unacknowledged_tasks,
                    remaining_tasks = connection_tasks.len(),
                    orphan_risk = true,
                    "IPC connection task drain stopped after a nonterminal observation failure"
                );
            }
        }

        #[cfg(any(unix, windows))]
        let _ = std::fs::remove_file(&self.socket_path);
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run`].
    pub async fn run_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_bus: Arc<EventBus>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        self.run_with_auth_with_cx(cx, event_bus, None, shutdown_rx)
            .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_with_registry`].
    pub async fn run_with_registry_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        self.run_with_registry_and_auth_with_cx(cx, event_bus, registry, None, shutdown_rx)
            .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_with_auth`].
    pub async fn run_with_auth_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_bus: Arc<EventBus>,
        auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        let ctx = Arc::new(IpcHandlerContext::with_auth(event_bus, None, auth));
        self.run_with_context_with_cx(cx, ctx, &mut shutdown_rx)
            .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_with_registry_and_auth`].
    pub async fn run_with_registry_and_auth_with_cx(
        self,
        cx: &crate::cx::Cx,
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
        self.run_with_context_with_cx(cx, ctx, &mut shutdown_rx)
            .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_with_registry_auth_and_rpc`].
    pub async fn run_with_registry_auth_and_rpc_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        self.run_with_registry_auth_rpc_and_search_config_with_cx(
            cx,
            event_bus,
            registry,
            auth,
            rpc_handler,
            None,
            shutdown_rx,
        )
        .await;
    }

    /// ft-xbnl0.2.3 Cx-first sibling of
    /// [`run_with_registry_auth_rpc_and_search_config`].
    // IPC bootstrap keeps the registry, auth, RPC, and search knobs explicit at the boundary.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_registry_auth_rpc_and_search_config_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        search_config: Option<SearchConfig>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        self.run_with_registry_auth_rpc_control_and_search_config_with_cx(
            cx,
            event_bus,
            registry,
            auth,
            rpc_handler,
            None,
            search_config,
            shutdown_rx,
        )
        .await;
    }

    /// Run with the complete watcher context, including cooperative control.
    // IPC bootstrap keeps every authority-bearing optional handler explicit.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_registry_auth_rpc_control_and_search_config_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        watcher_control_handler: Option<IpcWatcherControlHandler>,
        search_config: Option<SearchConfig>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        let ctx = Arc::new(IpcHandlerContext::with_auth_rpc_control_and_search_config(
            event_bus,
            Some(registry),
            auth,
            rpc_handler,
            watcher_control_handler,
            search_config,
        ));
        self.run_with_context_with_cx(cx, ctx, &mut shutdown_rx)
            .await;
    }

    /// Cx-first server loop over a caller-assembled handler context.
    pub async fn run_with_handler_context_with_cx(
        self,
        cx: &crate::cx::Cx,
        ctx: IpcHandlerContext,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        self.run_with_context_with_cx(cx, Arc::new(ctx), &mut shutdown_rx)
            .await;
    }
}

#[cfg(all(unix, test))]
async fn shutdown_signal_pending(shutdown_rx: &mut mpsc::Receiver<()>) -> bool {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    shutdown_signal_pending_with_cx(shutdown_rx, &cx).await
}

/// Test probe: is a shutdown signal pending right now?
///
/// Polls the receive exactly once. A 1 ms timeout used to bound it, but the
/// runtime checks that deadline before the first poll, so a slow scheduler
/// turned an already closed channel into "not pending" (about 1 run in 5).
#[cfg(all(unix, test))]
async fn shutdown_signal_pending_with_cx(
    shutdown_rx: &mut mpsc::Receiver<()>,
    cx: &crate::cx::Cx,
) -> bool {
    let signal = std::pin::pin!(shutdown_signal_with_cx(shutdown_rx, cx));
    matches!(futures::poll!(signal), std::task::Poll::Ready(true))
}

/// Waits for the next shutdown-channel event. `true` means shut down (a
/// message arrived or every sender is gone); `false` means the receive was
/// cancelled through `cx`, which is not a shutdown request by itself.
#[cfg(any(unix, windows))]
async fn shutdown_signal_with_cx(shutdown_rx: &mut mpsc::Receiver<()>, cx: &crate::cx::Cx) -> bool {
    match mpsc_recv_state_with_cx(shutdown_rx, cx).await {
        MpscRecvState::Value(()) | MpscRecvState::Disconnected => true,
        MpscRecvState::Cancelled => false,
    }
}

/// What ended one wait of the IPC accept loop.
#[cfg(any(unix, windows))]
#[derive(Debug)]
enum AcceptLoopWake<A> {
    /// A shutdown message arrived, or the shutdown channel closed.
    Shutdown,
    /// `cx` terminated, preserving cancellation and budget distinctions.
    ContextFailed(crate::runtime_async::ContextErrorKind),
    /// `accept()` completed.
    Accepted(A),
    /// A handler reached a terminal result and was reaped.
    ConnectionFinished(Option<Result<(), crate::runtime_async::task::JoinError>>),
    /// Handler observation failed without surrendering settlement ownership.
    ConnectionWaitFailed(crate::runtime_async::task::JoinError),
}

/// Waits for shutdown, context termination, a finished handler, or a client.
///
/// Shutdown and context termination take priority over new admission. Healthy
/// pending handlers register completion wakers; an empty set parks indefinitely.
/// Only real context deadlines and exceptional quarantined-handle settlement
/// can arm timers here. Dropping this wait retains every unfinished handler.
#[cfg(any(unix, windows))]
async fn next_accept_loop_wake<A, F>(
    cx: &crate::cx::Cx,
    shutdown_rx: &mut mpsc::Receiver<()>,
    accept: F,
    connection_tasks: &mut crate::runtime_async::task::JoinSet<()>,
) -> AcceptLoopWake<A>
where
    F: std::future::Future<Output = A>,
{
    use std::task::Poll;

    let mut shutdown = std::pin::pin!(shutdown_signal_with_cx(shutdown_rx, cx));
    let mut cancelled = crate::runtime_async::wait_for_cancellation_boxed(cx);
    let mut accept = std::pin::pin!(accept);
    let mut completed = std::pin::pin!(async {
        if connection_tasks.is_empty() {
            std::future::pending().await
        } else {
            connection_tasks.drain_next_with_cx(cx).await
        }
    });
    std::future::poll_fn(|task_cx| {
        let shutdown_state = shutdown.as_mut().poll(task_cx);
        if matches!(shutdown_state, Poll::Ready(true)) {
            return Poll::Ready(AcceptLoopWake::Shutdown);
        }
        if let Poll::Ready(result) = cancelled.as_mut().poll(task_cx) {
            let kind = match result {
                Err(error) => error.kind(),
                Ok(()) => crate::runtime_async::ContextError::internal(
                    "IPC cancellation wait ended without context termination",
                )
                .kind(),
            };
            return Poll::Ready(AcceptLoopWake::ContextFailed(kind));
        }
        if matches!(shutdown_state, Poll::Ready(false)) {
            return Poll::Ready(AcceptLoopWake::ContextFailed(
                crate::runtime_async::ContextError::internal(
                    "IPC shutdown receive ended without context termination",
                )
                .kind(),
            ));
        }
        if let Poll::Ready(result) = completed.as_mut().poll(task_cx) {
            return Poll::Ready(match result {
                Ok(result) => AcceptLoopWake::ConnectionFinished(result),
                Err(error) => AcceptLoopWake::ConnectionWaitFailed(error),
            });
        }
        accept.as_mut().poll(task_cx).map(AcceptLoopWake::Accepted)
    })
    .await
}

/// Stub IPC server for platforms without the local socket transport.
#[cfg(not(any(unix, windows)))]
pub struct IpcServer {
    socket_path: PathBuf,
}

#[cfg(not(any(unix, windows)))]
impl IpcServer {
    /// Create and bind a new IPC server.
    ///
    /// # Errors
    /// Returns error on non-unix platforms (IPC sockets are unix-only).
    pub async fn bind(socket_path: impl AsRef<Path>) -> std::io::Result<Self> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::bind_with_cx(&cx, socket_path).await
    }

    /// Cx-first unsupported-platform bind result.
    ///
    /// # Errors
    /// Returns cancellation when the caller is already cancelled, otherwise
    /// reports that local IPC sockets are unsupported on this platform.
    pub async fn bind_with_cx(
        cx: &crate::cx::Cx,
        socket_path: impl AsRef<Path>,
    ) -> std::io::Result<Self> {
        cx.checkpoint().map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("ipc bind cancelled: {err}"),
            )
        })?;
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

    async fn recv_shutdown_with_cx(cx: &crate::cx::Cx, shutdown_rx: &mut mpsc::Receiver<()>) {
        let _ = mpsc_recv_state_with_cx(shutdown_rx, cx).await;
    }

    /// Run the IPC server (no-op on non-unix platforms).
    pub async fn run(self, event_bus: Arc<EventBus>, shutdown_rx: mpsc::Receiver<()>) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_cx(&cx, event_bus, shutdown_rx).await;
    }

    /// Cx-first unsupported-platform server wait.
    pub async fn run_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with registry (no-op on non-unix platforms).
    pub async fn run_with_registry(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_registry_with_cx(&cx, event_bus, registry, shutdown_rx)
            .await;
    }

    /// Cx-first unsupported-platform server wait with a pane registry.
    pub async fn run_with_registry_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with optional auth configuration (no-op on non-unix platforms).
    pub async fn run_with_auth(
        self,
        event_bus: Arc<EventBus>,
        auth: Option<IpcAuth>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_auth_with_cx(&cx, event_bus, auth, shutdown_rx)
            .await;
    }

    /// Cx-first unsupported-platform server wait with auth configuration.
    pub async fn run_with_auth_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        _auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with registry and auth (no-op on non-unix platforms).
    pub async fn run_with_registry_and_auth(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_registry_and_auth_with_cx(&cx, event_bus, registry, auth, shutdown_rx)
            .await;
    }

    /// Cx-first unsupported-platform server wait with registry and auth.
    pub async fn run_with_registry_and_auth_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        _auth: Option<IpcAuth>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with registry, auth, and RPC handler (no-op on non-unix platforms).
    pub async fn run_with_registry_auth_and_rpc(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_registry_auth_and_rpc_with_cx(
            &cx,
            event_bus,
            registry,
            auth,
            rpc_handler,
            shutdown_rx,
        )
        .await;
    }

    /// Cx-first unsupported-platform server wait with registry, auth, and RPC.
    pub async fn run_with_registry_auth_and_rpc_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        _auth: Option<IpcAuth>,
        _rpc_handler: Option<IpcRpcHandler>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Run the IPC server with registry, auth, RPC handler, and search config (no-op on non-unix platforms).
    pub async fn run_with_registry_auth_rpc_and_search_config(
        self,
        event_bus: Arc<EventBus>,
        registry: Arc<RwLock<PaneRegistry>>,
        auth: Option<IpcAuth>,
        rpc_handler: Option<IpcRpcHandler>,
        search_config: Option<SearchConfig>,
        shutdown_rx: mpsc::Receiver<()>,
    ) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_registry_auth_rpc_and_search_config_with_cx(
            &cx,
            event_bus,
            registry,
            auth,
            rpc_handler,
            search_config,
            shutdown_rx,
        )
        .await;
    }

    /// Cx-first unsupported-platform server wait with all optional context.
    pub async fn run_with_registry_auth_rpc_and_search_config_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        _auth: Option<IpcAuth>,
        _rpc_handler: Option<IpcRpcHandler>,
        _search_config: Option<SearchConfig>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Cx-first unsupported-platform server wait with cooperative watcher
    /// control in the explicit context.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with_registry_auth_rpc_control_and_search_config_with_cx(
        self,
        cx: &crate::cx::Cx,
        _event_bus: Arc<EventBus>,
        _registry: Arc<RwLock<PaneRegistry>>,
        _auth: Option<IpcAuth>,
        _rpc_handler: Option<IpcRpcHandler>,
        _watcher_control_handler: Option<IpcWatcherControlHandler>,
        _search_config: Option<SearchConfig>,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }

    /// Cx-first unsupported-platform server wait over a handler context.
    pub async fn run_with_handler_context_with_cx(
        self,
        cx: &crate::cx::Cx,
        _ctx: IpcHandlerContext,
        mut shutdown_rx: mpsc::Receiver<()>,
    ) {
        tracing::warn!("IPC server not supported on this platform");
        Self::recv_shutdown_with_cx(cx, &mut shutdown_rx).await;
    }
}

/// Handle a single client connection with full context.
#[cfg(unix)]
/// ft-xbnl0.2.3 Cx-first sibling of [`handle_client_with_context`].
///
/// Threads the caller's cx into the request dispatcher so registry
/// lock acquires (via `handle_request_with_context_with_cx`) honor
/// cancellation from the parent run loop. The first request-line read is
/// additionally protected by a finite wall-clock deadline because the
/// underlying line stream only observes `Cx` at its pre-read checkpoint.
/// A successfully admitted event subscription remains streaming after that
/// first-line deadline, until disconnect or watcher cancellation.
#[cfg(unix)]
async fn handle_client_with_context_with_cx(
    cx: crate::cx::Cx,
    stream: UnixStream,
    ctx: Arc<IpcHandlerContext>,
    limits: IpcRuntimeLimits,
) -> std::io::Result<()> {
    let start = Instant::now();
    let max_message_size = limits.max_message_size;
    let (reader, mut writer) = stream.into_split();

    let bounded_reader = reader.take(IpcRuntimeLimits::message_read_limit_for(max_message_size));

    // ft-kccj8: the line cap must track the configured message limit.
    // The default `lines()` cap is 64 KiB — smaller than the default
    // 128 KiB message limit — so oversized (and even legal >64 KiB)
    // requests died as InvalidData before the `line.len()` check below
    // could produce the typed "message too large" reply.
    let mut lines = socket_transport::lines_with_max_length(
        socket_transport::buffered(bounded_reader),
        IpcRuntimeLimits::line_cap_for(max_message_size),
    );
    let request_line = match futures::future::select(
        Box::pin(wait_for_ipc_shutdown(&cx, &ctx)),
        Box::pin(crate::runtime_async::timeout_with_cx(
            &cx,
            limits.initial_request_timeout(),
            socket_transport::next_line_with_cx(&cx, &mut lines),
        )),
    )
    .await
    {
        futures::future::Either::Left(_) => return Ok(()),
        futures::future::Either::Right((result, _)) => result,
    };
    if classify_ipc_line_read_deadline(request_line.is_err())
        == IpcLineReadDeadlineOutcome::TimedOut
    {
        record_ipc_initial_request_timeout();
        return Ok(());
    }
    let Some(request_line) = request_line.ok() else {
        unreachable!("completed IPC request-line deadline must retain its read result");
    };
    let Some(line) = request_line? else {
        return Ok(());
    };
    if ctx.connection_shutdown.is_requested() || cx.checkpoint().is_err() {
        return Ok(());
    }

    if line.len() > max_message_size {
        let response = IpcResponse::error("message too large");
        let response_json = serialize_ipc_json_bounded(&response, limits.max_message_size)
            .unwrap_or_else(|_| br#"{"error":"message too large"}"#.to_vec());
        write_ipc_line_with_deadline(
            &cx,
            &mut writer,
            limits.io_timeout(),
            limits.max_message_size,
            response_json.as_slice(),
        )
        .await?;
        return Ok(());
    }

    let envelope = match serde_json::from_str::<IpcEnvelope>(&line) {
        Ok(envelope) => envelope,
        Err(e) => {
            let response = IpcResponse::error(format!("invalid request: {e}")).with_timing(start);
            let response_json = serialize_ipc_json_bounded(&response, limits.max_message_size)
                .unwrap_or_else(|_| br#"{"error":"response serialization failed"}"#.to_vec());
            write_ipc_line_with_deadline(
                &cx,
                &mut writer,
                limits.io_timeout(),
                limits.max_message_size,
                response_json.as_slice(),
            )
            .await?;
            return Ok(());
        }
    };

    if let Some(auth) = ctx.auth.as_ref() {
        let presented = envelope.token.as_deref(); // ubs:ignore - validates request credential, not a literal secret.
        if let Err(err) = auth.authorize(presented, envelope.request.required_scope()) {
            let response = IpcResponse::error(err.message()).with_timing(start);
            let response_json = serialize_ipc_json_bounded(&response, limits.max_message_size)
                .unwrap_or_else(|_| br#"{"error":"response serialization failed"}"#.to_vec());
            write_ipc_line_with_deadline(
                &cx,
                &mut writer,
                limits.io_timeout(),
                limits.max_message_size,
                response_json.as_slice(),
            )
            .await?;
            return Ok(());
        }
    }

    // ft-7h5da.4.1: SubscribeEvents is a streaming request. Intercept it
    // before the single-response dispatch and keep the connection open,
    // draining stored detections and forwarding best-effort lifecycle signals
    // until the consumer disconnects or the watcher cx is cancelled.
    if let IpcRequest::SubscribeEvents {
        version,
        cursor,
        pane,
        severity,
        rule_id,
        heartbeat_interval_ms,
    } = &envelope.request
    {
        if let Err(response) =
            validate_subscribe_events(*version, cursor.as_ref(), ctx.event_storage.is_some())
        {
            let encoded = serialize_ipc_response_bounded(*response, limits.max_message_size)?;
            return write_ipc_line_with_deadline(
                &cx,
                &mut writer,
                limits.io_timeout(),
                limits.max_message_size,
                &encoded,
            )
            .await;
        }
        // A subscription owns no child process. Close it promptly on server
        // shutdown even while it is waiting for an event or a blocked write.
        return match futures::future::select(
            Box::pin(wait_for_ipc_shutdown(&cx, &ctx)),
            Box::pin(stream_subscribe_events(
                &cx,
                &mut writer,
                &ctx,
                cursor.clone(),
                *pane,
                severity.clone(),
                rule_id.clone(),
                *heartbeat_interval_ms,
                limits.io_timeout(),
                limits.max_message_size,
            )),
        )
        .await
        {
            futures::future::Either::Left(_) => Ok(()),
            futures::future::Either::Right((result, _)) => result,
        };
    }

    let response = if matches!(&envelope.request, IpcRequest::Rpc { .. }) {
        dispatch_rpc_with_disconnect(&cx, envelope, &ctx, limits, async {
            // This is a one-request connection. EOF, a read failure, or an
            // extra request ends its ownership; never abandon the RPC future.
            let _ = socket_transport::next_line_with_cx(&cx, &mut lines).await;
        })
        .await
    } else {
        handle_request_with_context_with_cx(&cx, envelope, &ctx, limits).await
    };
    let response = response.with_timing(start);

    let response_json = serialize_ipc_response_bounded(response, limits.max_message_size)?;
    write_ipc_line_with_deadline(
        &cx,
        &mut writer,
        limits.io_timeout(),
        limits.max_message_size,
        response_json.as_slice(),
    )
    .await?;

    Ok(())
}

#[cfg(windows)]
async fn handle_client_with_context_with_cx(
    cx: crate::cx::Cx,
    mut stream: UnixStream,
    ctx: Arc<IpcHandlerContext>,
    limits: IpcRuntimeLimits,
) -> std::io::Result<()> {
    let start = Instant::now();
    let max_message_size = limits.max_message_size;

    let line = {
        let bounded_reader =
            (&mut stream).take(IpcRuntimeLimits::message_read_limit_for(max_message_size));
        // ft-kccj8: cap must track the configured limit (see the unix
        // handler) — the default 64 KiB lines() cap broke both legal
        // >64 KiB requests and the typed oversize rejection.
        let mut lines = socket_transport::lines_with_max_length(
            socket_transport::buffered(bounded_reader),
            IpcRuntimeLimits::line_cap_for(max_message_size),
        );
        let request_line = match futures::future::select(
            Box::pin(wait_for_ipc_shutdown(&cx, &ctx)),
            Box::pin(crate::runtime_async::timeout_with_cx(
                &cx,
                limits.initial_request_timeout(),
                socket_transport::next_line_with_cx(&cx, &mut lines),
            )),
        )
        .await
        {
            futures::future::Either::Left(_) => return Ok(()),
            futures::future::Either::Right((result, _)) => result,
        };
        if classify_ipc_line_read_deadline(request_line.is_err())
            == IpcLineReadDeadlineOutcome::TimedOut
        {
            record_ipc_initial_request_timeout();
            return Ok(());
        }
        let Some(request_line) = request_line.ok() else {
            unreachable!("completed IPC request-line deadline must retain its read result");
        };
        let Some(line) = request_line? else {
            return Ok(());
        };
        line
    };
    if ctx.connection_shutdown.is_requested() || cx.checkpoint().is_err() {
        return Ok(());
    }

    if line.len() > max_message_size {
        let response = IpcResponse::error("message too large");
        let response_json = serialize_ipc_json_bounded(&response, limits.max_message_size)
            .unwrap_or_else(|_| br#"{"error":"message too large"}"#.to_vec());
        write_ipc_line_with_deadline(
            &cx,
            &mut stream,
            limits.io_timeout(),
            limits.max_message_size,
            response_json.as_slice(),
        )
        .await?;
        return Ok(());
    }

    let envelope = match serde_json::from_str::<IpcEnvelope>(&line) {
        Ok(envelope) => envelope,
        Err(e) => {
            let response = IpcResponse::error(format!("invalid request: {e}")).with_timing(start);
            let response_json = serialize_ipc_json_bounded(&response, limits.max_message_size)
                .unwrap_or_else(|_| br#"{"error":"response serialization failed"}"#.to_vec());
            write_ipc_line_with_deadline(
                &cx,
                &mut stream,
                limits.io_timeout(),
                limits.max_message_size,
                response_json.as_slice(),
            )
            .await?;
            return Ok(());
        }
    };

    if let Some(auth) = ctx.auth.as_ref() {
        let presented = envelope.token.as_deref(); // ubs:ignore - validates request credential, not a literal secret.
        if let Err(err) = auth.authorize(presented, envelope.request.required_scope()) {
            let response = IpcResponse::error(err.message()).with_timing(start);
            let response_json = serialize_ipc_json_bounded(&response, limits.max_message_size)
                .unwrap_or_else(|_| br#"{"error":"response serialization failed"}"#.to_vec());
            write_ipc_line_with_deadline(
                &cx,
                &mut stream,
                limits.io_timeout(),
                limits.max_message_size,
                response_json.as_slice(),
            )
            .await?;
            return Ok(());
        }
    }

    // ft-7h5da.4.1: the live SubscribeEvents streaming transport is a
    // unix-only optimization (its CLI consumer, `IpcEventStream`, is
    // unix-gated). On Windows a SubscribeEvents request falls through to the
    // single-response dispatcher, which returns a typed
    // `ipc.subscribe_events.not_streamed` error rather than hanging.
    let response = if matches!(&envelope.request, IpcRequest::Rpc { .. }) {
        dispatch_rpc_with_disconnect(&cx, envelope, &ctx, limits, async {
            let mut byte = [0_u8; 1];
            let _ = stream.read(&mut byte).await;
        })
        .await
    } else {
        handle_request_with_context_with_cx(&cx, envelope, &ctx, limits).await
    };
    let response = response.with_timing(start);

    let response_json = serialize_ipc_response_bounded(response, limits.max_message_size)?;
    write_ipc_line_with_deadline(
        &cx,
        &mut stream,
        limits.io_timeout(),
        limits.max_message_size,
        response_json.as_slice(),
    )
    .await?;

    Ok(())
}

/// Idle-heartbeat NDJSON record for the streaming subscribe path. Heartbeats
/// carry no durable authority: direct clients retain the last flushed event
/// or cursor checkpoint, while the CLI follower owns its separate DB cursor.
#[cfg(unix)]
fn watch_heartbeat_record(now: i64) -> serde_json::Value {
    serde_json::json!({ "type": "heartbeat", "cursor": serde_json::Value::Null, "now": now })
}

/// Publish a committed prefix without claiming delivery before write + flush.
/// If either fails, terminal diagnostics and reconnect retain the prior cursor.
#[cfg(unix)]
async fn write_subscribe_cursor_checkpoint<W>(
    cx: &crate::cx::Cx,
    writer: &mut W,
    io_timeout: Duration,
    max_message_size: usize,
    flushed_cursor: &mut Option<crate::event_stream::DurableStreamCursor>,
    cursor: &crate::event_stream::DurableStreamCursor,
    reason: &'static str,
) -> std::io::Result<()>
where
    W: socket_transport::AsyncWrite + Unpin,
{
    let record = serde_json::json!({
        "type": "cursor_checkpoint",
        "cursor": cursor.after_id,
        "cursor_epoch": cursor.epoch,
        "reason": reason,
    });
    let encoded = serialize_ipc_json_bounded(&record, max_message_size)?;
    write_ipc_line_with_deadline(cx, writer, io_timeout, max_message_size, &encoded).await?;
    *flushed_cursor = Some(cursor.clone());
    Ok(())
}

/// ft-7h5da.4.2 explicit gap record for the streaming subscribe path. When the
/// bounded broadcast buffer drops `missed_count` live bus records (the
/// subscriber fell behind), the server emits this instead of silently skipping.
/// Persisted detection records can still be re-baselined through the DB cursor;
/// non-persisted signal records are honestly reported as missed live signals.
#[cfg(unix)]
fn watch_lag_gap_record(missed_count: u64) -> serde_json::Value {
    serde_json::json!({
        "type": "gap",
        "reason": "broadcast_lag",
        "missed_count": missed_count,
        "cursor": serde_json::Value::Null,
        "message": "live broadcast buffer overflowed; persisted detections can \
                    resync via the DB cursor, while non-persisted live signals were missed",
    })
}

#[cfg(unix)]
fn redact_subscription_text(text: &str, redactor: &crate::redactor::Redactor) -> String {
    let normalized = crate::output::normalize_terminal_text_for_redaction(text);
    redactor.redact(&normalized)
}

#[cfg(unix)]
fn redact_subscription_value(
    mut value: serde_json::Value,
    redactor: &crate::redactor::Redactor,
) -> serde_json::Value {
    let mut pending = vec![&mut value];
    while let Some(value) = pending.pop() {
        match value {
            serde_json::Value::String(text) => *text = redact_subscription_text(text, redactor),
            serde_json::Value::Array(values) => pending.extend(values.iter_mut()),
            serde_json::Value::Object(values) => pending.extend(values.values_mut()),
            _ => {}
        }
    }
    value
}

/// ft-7h5da.4.1/ft-0q7a2: build the NDJSON record for one live bus event.
/// Detection records keep the DB-cursor-compatible shape and apply the same
/// pane/severity/rule-glob filters as the DB-cursor path while redacting
/// `matched_text` + nested `extracted` secrets before emission (defense in
/// depth — the runtime publisher already redacts via `redact_detection`, but
/// other EventBus publishers may not). Pane lifecycle signals are emitted with
/// `cursor = null` because they are live-only notifications rather than
/// persisted event-history rows.
#[cfg(unix)]
fn subscribe_event_record(
    event: &Event,
    pane: Option<u64>,
    severity: Option<&str>,
    rule_glob: Option<&str>,
    redactor: &crate::redactor::Redactor,
) -> Option<serde_json::Value> {
    if let Some(want) = pane {
        if event.pane_id() != Some(want) {
            return None;
        }
    }

    match event {
        Event::PatternDetected {
            pane_id,
            detection,
            event_id,
            ..
        } => {
            let cursor = (*event_id)?;
            let severity_label = match detection.severity {
                crate::patterns::Severity::Info => "info",
                crate::patterns::Severity::Warning => "warning",
                crate::patterns::Severity::Critical => "critical",
            };
            if let Some(want) = severity {
                if !severity_label.eq_ignore_ascii_case(want) {
                    return None;
                }
            }
            if let Some(glob) = rule_glob {
                if !crate::events::rule_glob_matches(glob, &detection.rule_id) {
                    return None;
                }
            }
            Some(serde_json::json!({
                "type": "event",
                "cursor": cursor,
                "id": cursor,
                "pane_id": pane_id,
                "rule_id": detection.rule_id,
                "agent_type": detection.agent_type.to_string(),
                "event_type": detection.event_type,
                "severity": severity_label,
                "confidence": detection.confidence,
                "detected_at": now_ms_i64(),
                "matched_text": redact_subscription_text(&detection.matched_text, redactor),
                "extracted": redact_subscription_value(detection.extracted.clone(), redactor),
                "handled": false,
                "handled_status": serde_json::Value::Null,
            }))
        }
        Event::PaneDiscovered {
            pane_id,
            domain,
            title,
        } => {
            if severity.is_some() || rule_glob.is_some() {
                return None;
            }
            // ft-d3x5y: redact domain + title before emission, mirroring the
            // PatternDetected branch (redact_detection). These pane-lifecycle
            // records are live-only (cursor = null, never persisted) so this IPC
            // stream is the ONLY surface they reach — they are NOT reads of
            // already-redacted storage. A pane title routinely echoes secrets
            // (a shell putting the last command / env in the title), and the
            // same field is redacted on wa.state and the web SSE path; without
            // this the watch-events stream leaks it raw at IpcScope::Read.
            Some(serde_json::json!({
                "type": "event",
                "cursor": serde_json::Value::Null,
                "id": serde_json::Value::Null,
                "pane_id": pane_id,
                "rule_id": serde_json::Value::Null,
                "agent_type": serde_json::Value::Null,
                "event_type": "pane_discovered",
                "severity": serde_json::Value::Null,
                "confidence": serde_json::Value::Null,
                "detected_at": now_ms_i64(),
                "matched_text": serde_json::Value::Null,
                "extracted": serde_json::json!({
                    "domain": redact_subscription_text(domain, redactor),
                    "title": redact_subscription_text(title, redactor),
                }),
                "handled": false,
                "handled_status": serde_json::Value::Null,
            }))
        }
        Event::PaneDisappeared { pane_id } => {
            if severity.is_some() || rule_glob.is_some() {
                return None;
            }
            Some(serde_json::json!({
                "type": "event",
                "cursor": serde_json::Value::Null,
                "id": serde_json::Value::Null,
                "pane_id": pane_id,
                "rule_id": serde_json::Value::Null,
                "agent_type": serde_json::Value::Null,
                "event_type": "pane_disappeared",
                "severity": serde_json::Value::Null,
                "confidence": serde_json::Value::Null,
                "detected_at": now_ms_i64(),
                "matched_text": serde_json::Value::Null,
                "extracted": serde_json::json!({}),
                "handled": false,
                "handled_status": serde_json::Value::Null,
            }))
        }
        _ => None,
    }
}

/// Validate admission before taking a subscription or reading storage.
/// The refusal is boxed: an `IpcResponse` is ~280 bytes, too large to return
/// by value in every `Result` (clippy::result_large_err).
#[cfg(unix)]
fn validate_subscribe_events(
    version: u32,
    cursor: Option<&crate::event_stream::DurableStreamCursor>,
    storage_available: bool,
) -> Result<(), Box<IpcResponse>> {
    if version != IPC_EVENT_STREAM_VERSION || !storage_available {
        return Err(Box::new(IpcResponse::error_with_code(
            "ipc.subscribe_events.unavailable",
            "Storage-authoritative event subscriptions require protocol version 2 and event storage",
            None,
        )));
    }
    if cursor.is_some_and(|cursor| {
        cursor.after_id < 0
            || cursor.epoch.len() != 32
            || !cursor
                .epoch
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    }) {
        return Err(Box::new(IpcResponse::error_with_code(
            "ipc.subscribe_events.invalid_cursor",
            "Event resume cursor requires a nonnegative ID and a canonical storage epoch",
            None,
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn subscribe_stored_event_record(
    event: &crate::storage::StoredEvent,
    epoch: &str,
    pane: Option<u64>,
    severity: Option<&str>,
    rule_glob: Option<&str>,
    redactor: &crate::redactor::Redactor,
) -> Option<serde_json::Value> {
    if pane.is_some_and(|want| event.pane_id != want)
        || severity.is_some_and(|want| !event.severity.eq_ignore_ascii_case(want))
        || rule_glob.is_some_and(|glob| !crate::events::rule_glob_matches(glob, &event.rule_id))
    {
        return None;
    }
    let extracted = redact_subscription_value(
        event.extracted.clone().unwrap_or(serde_json::Value::Null),
        redactor,
    );
    Some(serde_json::json!({
        "type": "event",
        "cursor": event.id,
        "cursor_epoch": epoch,
        "id": event.id,
        "pane_id": event.pane_id,
        "rule_id": event.rule_id,
        "agent_type": event.agent_type,
        "event_type": event.event_type,
        "severity": event.severity,
        "confidence": event.confidence,
        "detected_at": event.detected_at,
        "matched_text": event.matched_text.as_deref().map(|text| redact_subscription_text(text, redactor)),
        "extracted": extracted,
        "handled": event.handled_at.is_some(),
        "handled_status": event.handled_status.as_deref().map(|text| redact_subscription_text(text, redactor)),
    }))
}

/// Stream ordered durable rows and best-effort lifecycle signals as NDJSON.
///
/// Version 2 binds resume IDs to a storage epoch. A fresh client receives the
/// captured tail checkpoint before later rows. Persisted payloads come only
/// from storage, and their cursor advances only after a successful flush or
/// deliberate filtering. A filtered page publishes its committed prefix even
/// with heartbeats disabled, so reconnect need not rescan excluded history.
/// Heartbeats and writes keep their finite deadlines.
#[cfg(unix)]
async fn stream_subscribe_events<W>(
    cx: &crate::cx::Cx,
    writer: &mut W,
    ctx: &IpcHandlerContext,
    cursor: Option<crate::event_stream::DurableStreamCursor>,
    pane: Option<u64>,
    severity: Option<String>,
    rule_glob: Option<String>,
    heartbeat_interval_ms: u64,
    io_timeout: Duration,
    max_message_size: usize,
) -> std::io::Result<()>
where
    W: socket_transport::AsyncWrite + Unpin,
{
    use crate::event_stream::{DurableFilteredEventStream, DurableStreamError, DurableStreamItem};

    let storage = ctx.event_storage.as_ref().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "event storage unavailable",
        )
    })?;
    let mut flushed_cursor = cursor.clone();
    let mut stream = DurableFilteredEventStream::new(&ctx.event_bus, Arc::clone(storage), cursor);
    let heartbeat = Duration::from_millis(heartbeat_interval_ms.max(1));
    let mut last_emit = Instant::now();

    loop {
        if cx.checkpoint().is_err() {
            return Ok(());
        }

        // A matching event publishes the filtered prefix before it. A page
        // ending in excluded rows has no such event, so checkpoint once at the
        // page boundary before waiting or querying another page. The stream
        // exposes no page checkpoint while any delivered row awaits its ack.
        if let Some(cursor) = stream.acknowledged_page_cursor()
            && flushed_cursor.as_ref() != Some(cursor)
        {
            if write_subscribe_cursor_checkpoint(
                cx,
                writer,
                io_timeout,
                max_message_size,
                &mut flushed_cursor,
                cursor,
                "examined_through_page",
            )
            .await
            .is_err()
            {
                return Ok(());
            }
            last_emit = Instant::now();
        }

        // Filtered traffic must not postpone heartbeat deadlines. At this
        // boundary every prior durable row is flushed or deliberately skipped.
        if heartbeat_interval_ms > 0 && last_emit.elapsed() >= heartbeat {
            let heartbeat_record = watch_heartbeat_record(now_ms_i64());
            let encoded = serialize_ipc_json_bounded(&heartbeat_record, max_message_size)?;
            if write_ipc_line_with_deadline(cx, writer, io_timeout, max_message_size, &encoded)
                .await
                .is_err()
            {
                return Ok(());
            }
            last_emit = Instant::now();
        }

        let idle_budget = if heartbeat_interval_ms > 0 {
            heartbeat.saturating_sub(last_emit.elapsed())
        } else {
            Duration::from_millis(250)
        };
        let mut acknowledge = None;
        let mut terminal = false;
        let record = match stream.next_item_cx(cx, idle_budget).await {
            Ok(Some(DurableStreamItem::Stored(event))) => {
                acknowledge = Some(event.id);
                let epoch = &stream.cursor().expect("stored row has a cursor").epoch;
                match subscribe_stored_event_record(
                    &event,
                    epoch,
                    pane,
                    severity.as_deref(),
                    rule_glob.as_deref(),
                    storage.redactor(),
                ) {
                    Some(record) => record,
                    None => {
                        stream
                            .acknowledge(event.id)
                            .map_err(std::io::Error::other)?;
                        continue;
                    }
                }
            }
            Ok(Some(DurableStreamItem::Live(event))) => match subscribe_event_record(
                &event,
                pane,
                severity.as_deref(),
                rule_glob.as_deref(),
                storage.redactor(),
            ) {
                Some(record) => record,
                None => continue,
            },
            Ok(Some(DurableStreamItem::Gap { missed_count })) => watch_lag_gap_record(missed_count),
            Ok(Some(DurableStreamItem::Checkpoint(cursor))) => {
                if write_subscribe_cursor_checkpoint(
                    cx,
                    writer,
                    io_timeout,
                    max_message_size,
                    &mut flushed_cursor,
                    &cursor,
                    "fresh_tail_baseline",
                )
                .await
                .is_err()
                {
                    return Ok(());
                }
                last_emit = Instant::now();
                continue;
            }
            Ok(Some(DurableStreamItem::Idle)) => {
                if heartbeat_interval_ms == 0 || last_emit.elapsed() < heartbeat {
                    continue;
                }
                watch_heartbeat_record(now_ms_i64())
            }
            Ok(None) | Err(DurableStreamError::Cancelled) => return Ok(()),
            Err(error) => {
                terminal = true;
                let code = if matches!(error, DurableStreamError::CursorExpired(_)) {
                    "ipc.event_stream.cursor_expired"
                } else {
                    "ipc.event_stream.storage_unavailable"
                };
                tracing::warn!(
                    code,
                    "durable IPC event stream stopped before cursor advancement"
                );
                serde_json::json!({
                    "type": "error",
                    "code": code,
                    "terminal": true,
                    "cursor": flushed_cursor.as_ref().map(|cursor| cursor.after_id),
                    "cursor_epoch": flushed_cursor.as_ref().map(|cursor| cursor.epoch.as_str()),
                })
            }
        };

        let buf = serialize_ipc_json_bounded(&record, max_message_size)?;
        if write_ipc_line_with_deadline(cx, writer, io_timeout, max_message_size, buf.as_slice())
            .await
            .is_err()
        {
            return Ok(());
        }
        if let Some(event_id) = acknowledge {
            stream
                .acknowledge(event_id)
                .map_err(std::io::Error::other)?;
            flushed_cursor = stream.cursor().cloned();
        }
        last_emit = Instant::now();
        if terminal {
            return Ok(());
        }
    }
}

/// Handle a parsed IPC request with full context and finite registry access.
async fn handle_request_with_context(
    cx: &crate::cx::Cx,
    envelope: IpcEnvelope,
    ctx: &IpcHandlerContext,
    limits: IpcRuntimeLimits,
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
            payload["streaming_health"] = crate::tailer::StreamingHealth::get_global()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .unwrap_or(serde_json::Value::Null);
            payload["health"] = health;
            payload["pane_output_rates"] = crate::runtime::PaneOutputRatesSnapshot::get_global()
                .and_then(|snapshot| serde_json::to_value(snapshot).ok())
                .unwrap_or(serde_json::Value::Null);
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
            let (search_index, search_health) =
                build_search_status_payload(ctx.search_config.as_ref());
            payload["search_index"] = search_index;
            payload["search_health"] = search_health;
            payload["allocator"] = allocator_status_payload();
            if let Some(registry_lock) = ctx.registry.as_ref() {
                let (active_count, panes) = {
                    if ctx.connection_shutdown.is_requested() {
                        return registry_lock_error_response(LockAcquireError::Cancelled);
                    }
                    // A blocked status snapshot retains an admitted connection.
                    // Bound that wait by the server's I/O limit and the caller's
                    // budget, and release it during cooperative server shutdown.
                    let read = crate::runtime_async::timeout_with_cx(
                        cx,
                        limits.io_timeout(),
                        registry_lock.read_with_cx(cx),
                    );
                    let result = match futures::future::select(
                        Box::pin(read),
                        Box::pin(wait_for_ipc_shutdown(cx, ctx)),
                    )
                    .await
                    {
                        futures::future::Either::Left((result, _)) => result,
                        futures::future::Either::Right(((), read)) => {
                            if ctx.connection_shutdown.is_requested() {
                                return registry_lock_error_response(LockAcquireError::Cancelled);
                            }
                            // The Cx ended the shutdown wait. Let the bounded
                            // acquire preserve its cancellation/budget class.
                            read.await
                        }
                    };
                    let registry = match result {
                        Ok(Ok(registry)) => registry,
                        Ok(Err(error)) => return registry_lock_error_response(error),
                        Err(_) => {
                            return registry_lock_error_response(
                                LockAcquireError::DeadlineExceeded,
                            );
                        }
                    };
                    let snapshot = registry.pane_arena_stats_snapshot();
                    let active_count = snapshot.len();
                    let panes: Vec<_> = snapshot
                        .into_iter()
                        .map(|snapshot| {
                            serde_json::json!({
                                "pane_id": snapshot.arena.pane_id(),
                                "arena_id": snapshot.arena.arena_id().raw(),
                                "tracked_bytes": snapshot.stats.tracked_bytes,
                                "peak_tracked_bytes": snapshot.stats.peak_tracked_bytes,
                                "updates": snapshot.stats.updates,
                            })
                        })
                        .collect();
                    (active_count, panes)
                };
                payload["pane_arenas"] = serde_json::json!({
                    "active_count": active_count,
                    "panes": panes,
                });
            } else {
                payload["pane_arenas"] = serde_json::json!({
                    "active_count": 0,
                    "panes": [],
                    "source": "unavailable",
                });
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
        IpcRequest::WatcherControl { control } => {
            let Some(handler) = ctx.watcher_control_handler.as_ref() else {
                return IpcResponse::error_with_code(
                    "ipc.watcher_control.unavailable",
                    "Watcher control is unavailable on this IPC endpoint",
                    None,
                );
            };
            handler(IpcWatcherControlInvocation {
                control,
                request_id: envelope.request_id,
            })
            .await
        }
        IpcRequest::Rpc { .. } => IpcResponse::error_with_code(
            "ipc.rpc_context_required",
            "RPC requires an authenticated connection context",
            None,
        ),
        // SubscribeEvents is a streaming request: the connection handler
        // (`handle_client_with_context_with_cx`) intercepts it before the
        // single-response dispatch and never routes it here. Reaching this
        // arm means the streaming intercept was bypassed — fail closed with
        // a typed error rather than silently dropping the subscription.
        IpcRequest::SubscribeEvents { .. } => IpcResponse::error_with_code(
            "ipc.subscribe_events.not_streamed",
            "SubscribeEvents must be handled by the streaming connection path",
            None,
        ),
    }
}

/// ft-xbnl0.2.3 Cx-first sibling of [`handle_request_with_context`].
///
/// Routes PaneState / SetPanePriority / ClearPanePriority to their
/// cx-first handler siblings so a cancelled client-handler cx
/// propagates into the registry RwLock acquires. The shared dispatcher
/// also receives the explicit cx and limits so Status cannot retain a
/// connection indefinitely while waiting for the registry.
async fn handle_request_with_context_with_cx(
    cx: &crate::cx::Cx,
    envelope: IpcEnvelope,
    ctx: &IpcHandlerContext,
    limits: IpcRuntimeLimits,
) -> IpcResponse {
    let IpcEnvelope {
        token,
        request_id,
        request,
    } = envelope;
    match request {
        IpcRequest::PaneState { pane_id } => handle_pane_state_with_cx(cx, pane_id, ctx).await,
        IpcRequest::SetPanePriority {
            pane_id,
            priority,
            ttl_ms,
        } => handle_set_pane_priority_with_cx(cx, pane_id, priority, ttl_ms, ctx).await,
        IpcRequest::ClearPanePriority { pane_id } => {
            handle_clear_pane_priority_with_cx(cx, pane_id, ctx).await
        }
        IpcRequest::Rpc { args } => {
            let Some(handler) = ctx.rpc_handler.as_ref() else {
                return IpcResponse::error("rpc handler not configured");
            };
            handler(IpcRpcRequest {
                args,
                request_id,
                cx: cx.clone(),
                deadline: Instant::now() + IPC_RPC_EXECUTION_TIMEOUT,
                cancellation: crate::runtime_async::process::CommandCancellation::new(),
                max_output_bytes: MAX_MESSAGE_SIZE.saturating_sub(1024),
            })
            .await
        }
        // Status keeps the caller's context and finite server limit when
        // delegating to the shared dispatcher.
        other => {
            let envelope = IpcEnvelope {
                token,
                request_id,
                request: other,
            };
            handle_request_with_context(cx, envelope, ctx, limits).await
        }
    }
}

/// Wake unread connections, blocked status snapshots, and admitted RPC owners
/// on the same server stop signal, without cancelling their shared caller Cx.
/// Only admitted RPCs must retain their work until process settlement.
///
/// Context cancellation and its actual budget deadline also end the wait.
/// The shared sticky signal wakes every registered connection and is visible
/// to late waiters; an idle connection needs no shutdown polling timer.
async fn wait_for_ipc_shutdown(cx: &crate::cx::Cx, ctx: &IpcHandlerContext) {
    let _ = ctx.connection_shutdown.wait(cx).await;
}

/// Retain the handler after disconnect/shutdown so its owned process reports
/// settlement. A per-request signal must not cancel the shared server Cx.
#[cfg(any(unix, windows))]
async fn dispatch_rpc_with_disconnect<F>(
    cx: &crate::cx::Cx,
    envelope: IpcEnvelope,
    ctx: &IpcHandlerContext,
    limits: IpcRuntimeLimits,
    disconnect: F,
) -> IpcResponse
where
    F: std::future::Future<Output = ()>,
{
    let IpcRequest::Rpc { args } = envelope.request else {
        return IpcResponse::error("RPC dispatcher received a non-RPC request");
    };
    let Some(handler) = ctx.rpc_handler.as_ref() else {
        return IpcResponse::error("rpc handler not configured");
    };
    let cancellation = crate::runtime_async::process::CommandCancellation::new();
    let request = IpcRpcRequest {
        args,
        request_id: envelope.request_id,
        cx: cx.clone(),
        deadline: Instant::now() + IPC_RPC_EXECUTION_TIMEOUT,
        cancellation: cancellation.clone(),
        max_output_bytes: limits.max_message_size.saturating_sub(1024),
    };
    let stop = async {
        let _ = futures::future::select(
            Box::pin(disconnect),
            Box::pin(wait_for_ipc_shutdown(cx, ctx)),
        )
        .await;
    };
    match futures::future::select(Box::pin(stop), handler(request)).await {
        futures::future::Either::Right((response, _)) => response,
        futures::future::Either::Left(((), pending)) => {
            cancellation.cancel();
            tracing::debug!(
                event = "ipc_rpc_connection_cancel_requested",
                "IPC RPC connection cancellation requested"
            );
            pending.await
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
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    let live_prompt = pane_state_live_prompt(&cx, pane_id, ctx).await;

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
        "live_prompt": live_prompt,
    }))
}

fn registry_lock_error_response(error: LockAcquireError) -> IpcResponse {
    match error {
        LockAcquireError::Cancelled => IpcResponse::error_with_code(
            "ipc.request_cancelled",
            "pane registry access was cancelled",
            None,
        ),
        LockAcquireError::DeadlineExceeded => IpcResponse::error_with_code(
            "ipc.request_deadline_exceeded",
            "pane registry access exceeded the request deadline",
            Some("Retry with a longer request deadline.".to_string()),
        ),
        LockAcquireError::PollQuotaExhausted => IpcResponse::error_with_code(
            "ipc.request_poll_quota_exhausted",
            "pane registry access exhausted the request poll quota",
            Some("Retry with a larger cooperative poll budget.".to_string()),
        ),
        LockAcquireError::CostBudgetExhausted => IpcResponse::error_with_code(
            "ipc.request_cost_budget_exhausted",
            "pane registry access exhausted the request cost budget",
            Some("Retry with a larger request cost budget.".to_string()),
        ),
        LockAcquireError::ContextFailure => IpcResponse::error_with_code(
            "ipc.request_context_failed",
            "pane registry access failed its capability context",
            None,
        ),
        LockAcquireError::TimedOut { .. } => IpcResponse::error_with_code(
            "ipc.registry_lock_timed_out",
            format!("pane registry lock timed out: {error}"),
            Some("Retry the request; the registry lock was contended.".to_string()),
        ),
        LockAcquireError::Poisoned | LockAcquireError::PolledAfterCompletion => {
            IpcResponse::error_with_code(
                "ipc.registry_lock_failed",
                format!("pane registry lock failed: {error}"),
                Some("Retry the request; restart the watcher if the failure persists.".to_string()),
            )
        }
    }
}

/// ft-xbnl0.2.3 Cx-first sibling of [`handle_pane_state`].
///
/// Threads caller cx through the registry RwLock `read_with_cx`
/// acquire so a client-disconnection-cancelled cx interrupts the
/// lock wait cleanly rather than blocking.
async fn handle_pane_state_with_cx(
    cx: &crate::cx::Cx,
    pane_id: u64,
    ctx: &IpcHandlerContext,
) -> IpcResponse {
    let Some(ref registry_lock) = ctx.registry else {
        return IpcResponse::ok_with_data(serde_json::json!({
            "pane_id": pane_id,
            "known": false,
            "reason": "no_registry",
        }));
    };

    let (entry, cursor) = {
        let registry = match registry_lock.read_with_cx(cx).await {
            Ok(registry) => registry,
            Err(error) => return registry_lock_error_response(error),
        };
        let Some(entry) = registry.get_entry(pane_id) else {
            return IpcResponse::ok_with_data(serde_json::json!({
                "pane_id": pane_id,
                "known": false,
                "reason": "unknown_pane",
            }));
        };
        (entry.clone(), registry.get_cursor(pane_id).cloned())
    };
    let live_prompt = pane_state_live_prompt(cx, pane_id, ctx).await;

    IpcResponse::ok_with_data(serde_json::json!({
        "pane_id": pane_id,
        "known": true,
        "observed": entry.should_observe(),
        "alt_screen": entry.is_alt_screen,
        "last_status_at": entry.last_status_at,
        "in_gap": cursor.as_ref().map(|c| c.in_gap),
        "cursor_alt_screen": cursor.as_ref().map(|c| c.in_alt_screen),
        "live_prompt": live_prompt,
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

/// ft-xbnl0.2.3 Cx-first sibling of [`handle_set_pane_priority`].
///
/// Threads caller cx through the registry RwLock `write_with_cx`
/// acquire.
async fn handle_set_pane_priority_with_cx(
    cx: &crate::cx::Cx,
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
        let mut registry = match registry_lock.write_with_cx(cx).await {
            Ok(registry) => registry,
            Err(error) => return registry_lock_error_response(error),
        };
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

/// ft-xbnl0.2.3 Cx-first sibling of [`handle_clear_pane_priority`].
///
/// Threads caller cx through the registry RwLock `write_with_cx`
/// acquire.
async fn handle_clear_pane_priority_with_cx(
    cx: &crate::cx::Cx,
    pane_id: u64,
    ctx: &IpcHandlerContext,
) -> IpcResponse {
    let Some(ref registry_lock) = ctx.registry else {
        return IpcResponse::error_with_code(
            "ipc.no_registry",
            "pane registry not available",
            Some("Start the watcher with `ft watch` in this workspace.".to_string()),
        );
    };

    {
        let mut registry = match registry_lock.write_with_cx(cx).await {
            Ok(registry) => registry,
            Err(error) => return registry_lock_error_response(error),
        };
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
    limits: IpcRuntimeLimits,
}

impl IpcClient {
    /// Create a new IPC client.
    #[must_use]
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            auth_token: std::env::var("FT_IPC_TOKEN").ok(),
            limits: resolve_limits(None),
        }
    }

    /// Create a new IPC client with an explicit auth token.
    #[must_use]
    pub fn with_token(socket_path: impl AsRef<Path>, token: impl Into<String>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_path_buf(),
            auth_token: Some(token.into()),
            limits: resolve_limits(None),
        }
    }

    /// Replace the client's runtime limits after validating the connection and
    /// line-read resource bounds.
    ///
    /// # Errors
    /// Returns `InvalidInput` when a connection or line-read limit falls
    /// outside the supported finite range.
    pub fn try_with_runtime_limits(mut self, limits: IpcRuntimeLimits) -> std::io::Result<Self> {
        if !limits.resource_bounds_are_valid() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid IPC connection or line-read resource limits",
            ));
        }
        self.limits = limits;
        Ok(self)
    }

    /// Update the auth token (use `None` to clear).
    pub fn set_token(&mut self, token: Option<String>) {
        let presented = token; // ubs:ignore - caller-provided IPC credential, not a hardcoded secret.
        self.auth_token = presented; // ubs:ignore - updates IPC credential storage from caller input.
    }

    /// Check if the watcher socket exists.
    #[must_use]
    pub fn socket_exists(&self) -> bool {
        #[cfg(unix)]
        {
            self.socket_path.exists()
        }
        #[cfg(windows)]
        {
            self.socket_path.parent().is_some_and(Path::exists)
        }
        #[cfg(not(any(unix, windows)))]
        {
            self.socket_path.exists()
        }
    }
}

#[cfg(any(unix, windows))]
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
        // ft-4ku44: ergonomic wrapper around `send_user_var_with_cx`. Borrow
        // the ambient request cx if one is in scope; otherwise construct a
        // fresh request-rooted cx so the inner `_with_cx` chain still gates
        // every primitive use through the RuntimeProof seal.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.send_user_var_with_cx(&cx, pane_id, name, value).await
    }

    /// Ping the watcher daemon.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn ping(&self) -> Result<IpcResponse, UserVarError> {
        // ft-4ku44: ergonomic wrapper around `ping_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.ping_with_cx(&cx).await
    }

    /// Cx-first [`Self::ping`] (ft-xbnl0.2.3). Routes the IPC
    /// ping through [`Self::send_request_with_cx`] so caller
    /// cancellation can abort the round-trip at any of the 4
    /// await points (connect, write, flush, read).
    pub async fn ping_with_cx(&self, cx: &crate::cx::Cx) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_cx(cx, IpcRequest::Ping).await
    }

    /// Cx-first [`Self::send_user_var`] (ft-xbnl0.2.3). Routes
    /// the user-var update through [`Self::send_request_with_cx`]
    /// so the IPC round-trip honors caller cancellation.
    pub async fn send_user_var_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        name: String,
        value: String,
    ) -> Result<IpcResponse, UserVarError> {
        let request = IpcRequest::UserVar {
            pane_id,
            name,
            value,
        };
        self.send_request_with_cx(cx, request).await
    }

    /// Get watcher status.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn status(&self) -> Result<IpcResponse, UserVarError> {
        // ft-4ku44: ergonomic wrapper around `status_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.status_with_cx(&cx).await
    }

    /// Cx-first [`Self::status`] (ft-xbnl0.2.3). Routes the IPC
    /// status round-trip through [`Self::send_request_with_cx`].
    pub async fn status_with_cx(&self, cx: &crate::cx::Cx) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_cx(cx, IpcRequest::Status).await
    }

    /// Request pane state from watcher registry.
    ///
    /// # Errors
    /// Returns error if connection fails.
    pub async fn pane_state(&self, pane_id: u64) -> Result<IpcResponse, UserVarError> {
        // ft-4ku44: ergonomic wrapper around `pane_state_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.pane_state_with_cx(&cx, pane_id).await
    }

    /// Cx-first [`Self::pane_state`] (ft-xbnl0.2.3).
    pub async fn pane_state_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_cx(cx, IpcRequest::PaneState { pane_id })
            .await
    }

    /// Set a runtime pane capture priority override.
    pub async fn set_pane_priority(
        &self,
        pane_id: u64,
        priority: u32,
        ttl_ms: Option<u64>,
    ) -> Result<IpcResponse, UserVarError> {
        // ft-4ku44: ergonomic wrapper around `set_pane_priority_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.set_pane_priority_with_cx(&cx, pane_id, priority, ttl_ms)
            .await
    }

    /// Cx-first [`Self::set_pane_priority`] (ft-xbnl0.2.3).
    pub async fn set_pane_priority_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        priority: u32,
        ttl_ms: Option<u64>,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_cx(
            cx,
            IpcRequest::SetPanePriority {
                pane_id,
                priority,
                ttl_ms,
            },
        )
        .await
    }

    /// Clear any runtime pane capture priority override.
    pub async fn clear_pane_priority(&self, pane_id: u64) -> Result<IpcResponse, UserVarError> {
        // ft-4ku44: ergonomic wrapper around `clear_pane_priority_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.clear_pane_priority_with_cx(&cx, pane_id).await
    }

    /// Cx-first [`Self::clear_pane_priority`] (ft-xbnl0.2.3).
    pub async fn clear_pane_priority_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_cx(cx, IpcRequest::ClearPanePriority { pane_id })
            .await
    }

    /// Request one instance-bound cooperative watcher action.
    ///
    /// The server still performs the authoritative instance comparison. This
    /// method merely preserves the strict typed payload over the bounded IPC
    /// transport; it never converts the identity into a PID or signal target.
    pub async fn watcher_control(
        &self,
        control: WatcherControlRequest,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.watcher_control_with_cx(&cx, control, request_id).await
    }

    /// Cx-first [`Self::watcher_control`].
    pub async fn watcher_control_with_cx(
        &self,
        cx: &crate::cx::Cx,
        control: WatcherControlRequest,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_id_with_cx(cx, IpcRequest::WatcherControl { control }, request_id)
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
        // ft-4ku44: ergonomic wrapper around `call_rpc_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.call_rpc_with_cx(&cx, args, request_id).await
    }

    /// Cx-first [`Self::call_rpc`] (ft-xbnl0.2.3). Uses
    /// [`Self::send_request_with_id_with_cx`] directly (not
    /// [`Self::send_request_with_cx`]) because RPC calls carry a
    /// caller-supplied `request_id` for response correlation.
    pub async fn call_rpc_with_cx(
        &self,
        cx: &crate::cx::Cx,
        args: Vec<String>,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_id_with_cx(cx, IpcRequest::Rpc { args }, request_id)
            .await
    }

    // NOTE: send_status_update method was removed in v0.2.0 (Lua performance optimization)
    // NOTE: ft-4ku44 removed the private `send_request` helper. Every
    // public IpcClient method now constructs a default request `Cx` and
    // delegates to its `_with_cx` sibling, so the only primitive-using
    // path on this client runs through `send_request_with_id_with_cx`.

    /// Cx-first send_request: delegates to [`Self::send_request_with_id_with_cx`]
    /// with `None` request_id (the chain that legacy callers used to reach
    /// via `send_request` → `send_request_with_id` before ft-4ku44).
    async fn send_request_with_cx(
        &self,
        cx: &crate::cx::Cx,
        request: IpcRequest,
    ) -> Result<IpcResponse, UserVarError> {
        self.send_request_with_id_with_cx(cx, request, None).await
    }

    /// Cx-first [`Self::send_request_with_id`] (ft-xbnl0.2.3).
    /// Threads caller `&Cx` through the 4 await points in the
    /// IPC round-trip via checkpoint seams:
    ///
    ///   1. pre-flight: pre-cancelled cx returns before
    ///      `socket_path.exists()` check (saves the syscall).
    ///   2. before `compat_unix::connect`: a cancel here avoids
    ///      creating the socket connection at all.
    ///   3. before write: a cancel here avoids sending a request
    ///      that will immediately be abandoned.
    ///   4. before read: a pre-cancelled caller skips the response wait.
    ///
    /// One absolute I/O deadline covers connect, request write/flush, and
    /// response read (and is shortened by any earlier capability deadline).
    /// Each of those waits also ends as soon as `cx` is cancelled: since
    /// asupersync 0.6 that deadline does not expire on cancellation, and the
    /// socket futures register no cancellation wake of their own, so connect
    /// and write run under [`crate::runtime_async::until_cancelled`] and the
    /// read through `next_line_with_cx`. Before that, a request cancelled
    /// while the watcher was slow to answer waited for the reply or the whole
    /// I/O timeout (30 s by default; ft-3268r).
    ///
    /// All cancellation errors wrap into `UserVarError::IpcSendFailed`
    /// with "cancelled" prefix for pattern-matching.
    async fn send_request_with_id_with_cx(
        &self,
        cx: &crate::cx::Cx,
        request: IpcRequest,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled pre-start: {err}"),
        })?;

        #[cfg(unix)]
        if !self.socket_path.exists() {
            return Err(UserVarError::WatcherNotRunning {
                socket_path: self.socket_path.display().to_string(),
            });
        }

        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled before connect: {err}"),
        })?;

        let io_started = Instant::now();
        let io_timeout = self.limits.io_timeout();
        let connect_budget =
            remaining_ipc_io_budget(io_timeout, io_started.elapsed()).ok_or_else(|| {
                UserVarError::IpcSendFailed {
                    message: "IPC I/O deadline exceeded".to_string(),
                }
            })?;
        let stream = crate::runtime_async::timeout_with_cx(
            cx,
            connect_budget,
            crate::runtime_async::until_cancelled(cx, socket_transport::connect(&self.socket_path)),
        )
        .await
        .map_err(|_elapsed| UserVarError::IpcSendFailed {
            message: "IPC I/O deadline exceeded".to_string(),
        })?
        .map_err(|err| ipc_client_wait_ended(cx, "connect", &err))?
        .map_err(|e| UserVarError::IpcSendFailed {
            message: format!("failed to connect: {e}"),
        })?;

        #[cfg(unix)]
        let (reader, mut writer) = stream.into_split();
        #[cfg(windows)]
        let mut stream = stream;

        let envelope = IpcEnvelope {
            token: self.auth_token.clone(),
            request_id,
            request,
        };
        let request_json = serialize_ipc_json_bounded(&envelope, self.limits.max_message_size)
            .map_err(|_| UserVarError::IpcSendFailed {
                message: "IPC request exceeded the configured message size".to_string(),
            })?;

        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled before write: {err}"),
        })?;

        #[cfg(unix)]
        {
            let write_budget = remaining_ipc_io_budget(io_timeout, io_started.elapsed())
                .ok_or_else(|| UserVarError::IpcSendFailed {
                    message: "IPC I/O deadline exceeded".to_string(),
                })?;
            crate::runtime_async::timeout_with_cx(
                cx,
                write_budget,
                crate::runtime_async::until_cancelled(cx, async {
                    writer.write_all(request_json.as_slice()).await?;
                    writer.write_all(b"\n").await?;
                    writer.flush().await
                }),
            )
            .await
            .map_err(|_elapsed| UserVarError::IpcSendFailed {
                message: "IPC I/O deadline exceeded".to_string(),
            })?
            .map_err(|err| ipc_client_wait_ended(cx, "write", &err))?
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to write request: {e}"),
            })?;
        }

        #[cfg(windows)]
        {
            let write_budget = remaining_ipc_io_budget(io_timeout, io_started.elapsed())
                .ok_or_else(|| UserVarError::IpcSendFailed {
                    message: "IPC I/O deadline exceeded".to_string(),
                })?;
            crate::runtime_async::timeout_with_cx(
                cx,
                write_budget,
                crate::runtime_async::until_cancelled(cx, async {
                    stream.write_all(request_json.as_slice()).await?;
                    stream.write_all(b"\n").await?;
                    stream.flush().await
                }),
            )
            .await
            .map_err(|_elapsed| UserVarError::IpcSendFailed {
                message: "IPC I/O deadline exceeded".to_string(),
            })?
            .map_err(|err| ipc_client_wait_ended(cx, "write", &err))?
            .map_err(|e| UserVarError::IpcSendFailed {
                message: format!("failed to write request: {e}"),
            })?;
        }

        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled before read: {err}"),
        })?;

        // ft-kccj8: responses share the request message-size contract;
        // the default 64 KiB lines() cap silently failed any response
        // above it with InvalidData.
        #[cfg(unix)]
        let mut lines = socket_transport::lines_with_max_length(
            socket_transport::buffered(reader),
            IpcRuntimeLimits::line_cap_for(self.limits.max_message_size),
        );
        #[cfg(windows)]
        let mut lines = socket_transport::lines_with_max_length(
            socket_transport::buffered(&mut stream),
            IpcRuntimeLimits::line_cap_for(self.limits.max_message_size),
        );
        let read_budget =
            remaining_ipc_io_budget(io_timeout, io_started.elapsed()).ok_or_else(|| {
                UserVarError::IpcSendFailed {
                    message: "IPC I/O deadline exceeded".to_string(),
                }
            })?;
        let response_line = crate::runtime_async::timeout_with_cx(
            cx,
            read_budget,
            socket_transport::next_line_with_cx(cx, &mut lines),
        )
        .await
        .map_err(|_elapsed| UserVarError::IpcSendFailed {
            message: "IPC I/O deadline exceeded".to_string(),
        })?;
        let line = response_line
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::Interrupted && cx.checkpoint().is_err() {
                    ipc_client_wait_ended(cx, "read", &e)
                } else {
                    UserVarError::IpcSendFailed {
                        message: format!("failed to read response: {e}"),
                    }
                }
            })?
            .ok_or_else(|| UserVarError::IpcSendFailed {
                message: "failed to read response: server closed connection".to_string(),
            })?;

        if line.len() > self.limits.max_message_size {
            return Err(UserVarError::IpcSendFailed {
                message: "IPC response exceeded the configured message size".to_string(),
            });
        }

        let response: IpcResponse =
            serde_json::from_str(&line).map_err(|e| UserVarError::IpcSendFailed {
                message: format!("invalid response: {e}"),
            })?;

        Ok(response)
    }

    // ft-4ku44: legacy `send_request_with_id` was removed. Every public
    // IpcClient method now delegates to a `_with_cx` sibling whose body
    // ultimately reaches `send_request_with_id_with_cx` — the canonical,
    // checkpoint-instrumented, primitive-using path.
}

/// ft-7h5da.4.1 live event stream returned by
/// [`IpcClient::subscribe_events_with_cx`]. Yields one NDJSON line per
/// detection event (plus idle heartbeats) fanned out from the watcher's
/// in-process `EventBus`. The lower-latency transport for
/// `ft robot watch-events --follow`; the CLI falls back to storage-cursor
/// tailing when the watcher is down.
#[cfg(unix)]
pub struct IpcEventStream {
    lines: crate::runtime_async::unix::LineReader<crate::runtime_async::unix::UnixStream>,
}

#[cfg(unix)]
impl IpcEventStream {
    /// Read the next NDJSON line from the live event stream. `Ok(None)` means
    /// the watcher closed the connection (it shut down or dropped the
    /// `EventBus`) — callers should fall back to DB-cursor tailing. Ends with
    /// an `Interrupted` error as soon as `cx` is cancelled, also while it is
    /// waiting for a line.
    ///
    /// # Errors
    /// Returns an I/O error if the underlying socket read fails.
    pub async fn next_line_with_cx(
        &mut self,
        cx: &crate::cx::Cx,
    ) -> std::io::Result<Option<String>> {
        socket_transport::next_line_with_cx(cx, &mut self.lines).await
    }
}

#[cfg(unix)]
impl IpcClient {
    /// ft-7h5da.4.1: open a live NDJSON event subscription over IPC. Connects
    /// to the watcher socket, sends a `SubscribeEvents` request, and returns a
    /// stream beginning with a fresh-tail cursor checkpoint, followed by
    /// ordered durable rows and best-effort lifecycle signals. This is the low-latency
    /// transport for `ft robot watch-events --follow`; when the watcher is down
    /// (socket missing / connect fails) the caller transparently falls back to
    /// storage-cursor tailing.
    ///
    /// # Errors
    /// Returns [`UserVarError::WatcherNotRunning`] when the socket is absent and
    /// [`UserVarError::IpcSendFailed`] on connect/serialize/write/cancel
    /// failures.
    pub async fn subscribe_events_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane: Option<u64>,
        severity: Option<String>,
        rule_id: Option<String>,
        heartbeat_interval_ms: u64,
    ) -> Result<IpcEventStream, UserVarError> {
        self.subscribe_events_from_cursor_with_cx(
            cx,
            None,
            pane,
            severity,
            rule_id,
            heartbeat_interval_ms,
        )
        .await
    }

    /// Resume the ordered stream after an ID bound to its storage epoch.
    ///
    /// A fresh subscription first emits a cursor_checkpoint baseline. Stored
    /// rows are emitted in ID order; bus notifications are only wake hints.
    /// Filtered page suffixes emit an `examined_through_page` checkpoint so
    /// direct clients can resume beyond deliberately excluded rows.
    /// An unavailable history terminates with an explicit cursor-expired error.
    pub async fn subscribe_events_from_cursor_with_cx(
        &self,
        cx: &crate::cx::Cx,
        cursor: Option<crate::event_stream::DurableStreamCursor>,
        pane: Option<u64>,
        severity: Option<String>,
        rule_id: Option<String>,
        heartbeat_interval_ms: u64,
    ) -> Result<IpcEventStream, UserVarError> {
        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled pre-start: {err}"),
        })?;

        if !self.socket_path.exists() {
            return Err(UserVarError::WatcherNotRunning {
                socket_path: self.socket_path.display().to_string(),
            });
        }

        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled before connect: {err}"),
        })?;

        let io_started = Instant::now();
        let io_timeout = self.limits.io_timeout();
        let connect_budget =
            remaining_ipc_io_budget(io_timeout, io_started.elapsed()).ok_or_else(|| {
                UserVarError::IpcSendFailed {
                    message: "IPC I/O deadline exceeded".to_string(),
                }
            })?;
        let mut stream = crate::runtime_async::timeout_with_cx(
            cx,
            connect_budget,
            crate::runtime_async::until_cancelled(cx, socket_transport::connect(&self.socket_path)),
        )
        .await
        .map_err(|_elapsed| UserVarError::IpcSendFailed {
            message: "IPC I/O deadline exceeded".to_string(),
        })?
        .map_err(|err| ipc_client_wait_ended(cx, "connect", &err))?
        .map_err(|e| UserVarError::IpcSendFailed {
            message: format!("failed to connect: {e}"),
        })?;

        let envelope = IpcEnvelope {
            token: self.auth_token.clone(),
            request_id: None,
            request: IpcRequest::SubscribeEvents {
                version: IPC_EVENT_STREAM_VERSION,
                cursor,
                pane,
                severity,
                rule_id,
                heartbeat_interval_ms,
            },
        };
        let request_json = serialize_ipc_json_bounded(&envelope, self.limits.max_message_size)
            .map_err(|_| UserVarError::IpcSendFailed {
                message: "IPC request exceeded the configured message size".to_string(),
            })?;

        cx.checkpoint().map_err(|err| UserVarError::IpcSendFailed {
            message: format!("cancelled before write: {err}"),
        })?;

        // Write the one-shot subscribe request, then reuse the same stream
        // for the (read-only from here) NDJSON line stream. `UnixStream`
        // implements both `AsyncRead` and `AsyncWrite`, so no split is needed.
        let write_budget =
            remaining_ipc_io_budget(io_timeout, io_started.elapsed()).ok_or_else(|| {
                UserVarError::IpcSendFailed {
                    message: "IPC I/O deadline exceeded".to_string(),
                }
            })?;
        write_ipc_line_with_deadline(
            cx,
            &mut stream,
            write_budget,
            self.limits.max_message_size,
            request_json.as_slice(),
        )
        .await
        .map_err(|error| UserVarError::IpcSendFailed {
            message: if error.kind() == std::io::ErrorKind::TimedOut {
                "IPC I/O deadline exceeded".to_string()
            } else {
                format!("failed to write request: {error}")
            },
        })?;

        // ft-kccj8: event lines share the message-size contract; see the
        // response-read cap above.
        let lines = socket_transport::lines_with_max_length(
            socket_transport::buffered(stream),
            IpcRuntimeLimits::line_cap_for(self.limits.max_message_size),
        );
        Ok(IpcEventStream { lines })
    }
}

#[cfg(not(any(unix, windows)))]
impl IpcClient {
    /// IPC is unavailable on this platform; return a clear error.
    fn unsupported() -> UserVarError {
        UserVarError::IpcSendFailed {
            message: "IPC sockets are not supported on this platform".to_string(),
        }
    }

    pub async fn send_user_var(
        &self,
        pane_id: u64,
        name: String,
        value: String,
    ) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.send_user_var_with_cx(&cx, pane_id, name, value).await
    }

    pub async fn ping(&self) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.ping_with_cx(&cx).await
    }

    pub async fn status(&self) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.status_with_cx(&cx).await
    }

    pub async fn pane_state(&self, pane_id: u64) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.pane_state_with_cx(&cx, pane_id).await
    }

    pub async fn set_pane_priority(
        &self,
        pane_id: u64,
        priority: u32,
        ttl_ms: Option<u64>,
    ) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.set_pane_priority_with_cx(&cx, pane_id, priority, ttl_ms)
            .await
    }

    pub async fn clear_pane_priority(&self, pane_id: u64) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.clear_pane_priority_with_cx(&cx, pane_id).await
    }

    pub async fn watcher_control(
        &self,
        control: WatcherControlRequest,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.watcher_control_with_cx(&cx, control, request_id).await
    }

    pub async fn call_rpc(
        &self,
        args: Vec<String>,
        request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.call_rpc_with_cx(&cx, args, request_id).await
    }

    // ft-xbnl0.2.3 Cx-first: non-unix stubs so the Cx-first
    // API surface is available on all platforms for
    // cross-platform call sites. Each always returns
    // `unsupported()` just like the legacy counterparts.

    pub async fn send_user_var_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        _pane_id: u64,
        _name: String,
        _value: String,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn ping_with_cx(&self, _cx: &crate::cx::Cx) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn status_with_cx(&self, _cx: &crate::cx::Cx) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn pane_state_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        _pane_id: u64,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn set_pane_priority_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        _pane_id: u64,
        _priority: u32,
        _ttl_ms: Option<u64>,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn clear_pane_priority_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        _pane_id: u64,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn watcher_control_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        _control: WatcherControlRequest,
        _request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }

    pub async fn call_rpc_with_cx(
        &self,
        _cx: &crate::cx::Cx,
        _args: Vec<String>,
        _request_id: Option<String>,
    ) -> Result<IpcResponse, UserVarError> {
        Err(Self::unsupported())
    }
}

#[cfg(all(test, any(unix, windows)))]
mod transport_roundtrip_tests {
    use super::*;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build runtime for IPC transport tests");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn ipc_platform_transport_ping_roundtrip() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir
                .path()
                .join(format!("platform-roundtrip-{}.sock", std::process::id()));

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(event_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(Duration::from_millis(25)).await;

            let client = IpcClient::new(&socket_path);
            let response = client.ping().await.unwrap();
            assert!(response.ok);

            mpsc_send_value(&shutdown_tx, ()).await.unwrap();
            let _ = server_handle.await;
            assert!(
                !socket_path.exists(),
                "IPC transport path should be removed after shutdown"
            );
        });
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::items_after_statements, clippy::significant_drop_tightening)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::redactor::Redactor;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder, RwLock};
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
    fn subscribe_events_required_scope_is_read() {
        // ft-7h5da.4.1: a live event subscription is a read-only surface.
        let req = IpcRequest::SubscribeEvents {
            version: IPC_EVENT_STREAM_VERSION,
            cursor: None,
            pane: Some(3),
            severity: Some("warning".into()),
            rule_id: Some("codex.*".into()),
            heartbeat_interval_ms: 5000,
        };
        assert_eq!(req.required_scope(), IpcScope::Read);
    }

    #[cfg(unix)]
    fn sample_detection(
        rule_id: &str,
        severity: crate::patterns::Severity,
        matched: &str,
    ) -> crate::patterns::Detection {
        crate::patterns::Detection {
            rule_id: rule_id.into(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "usage.reached".into(),
            severity,
            confidence: 0.9,
            extracted: serde_json::json!({"k": "v"}),
            matched_text: matched.into(),
            span: (0, matched.len()),
        }
    }

    #[cfg(unix)]
    #[test]
    fn durable_subscription_admission_rejects_unversioned_or_invalid_resume() {
        use crate::event_stream::DurableStreamCursor;

        let cursor = DurableStreamCursor {
            after_id: 0,
            epoch: "0123456789abcdef0123456789abcdef".to_string(),
        };
        assert!(validate_subscribe_events(IPC_EVENT_STREAM_VERSION, Some(&cursor), true).is_ok());
        assert!(validate_subscribe_events(IPC_EVENT_STREAM_VERSION, None, true).is_ok());
        for (version, available) in [(1, true), (IPC_EVENT_STREAM_VERSION, false)] {
            let error = validate_subscribe_events(version, Some(&cursor), available).unwrap_err();
            assert_eq!(
                error.error_code.as_deref(),
                Some("ipc.subscribe_events.unavailable")
            );
        }
        for invalid in [
            DurableStreamCursor {
                after_id: -1,
                ..cursor.clone()
            },
            DurableStreamCursor {
                epoch: "short".to_string(),
                ..cursor.clone()
            },
            DurableStreamCursor {
                epoch: cursor.epoch.to_uppercase(),
                ..cursor.clone()
            },
        ] {
            let error = validate_subscribe_events(IPC_EVENT_STREAM_VERSION, Some(&invalid), true)
                .unwrap_err();
            assert_eq!(
                error.error_code.as_deref(),
                Some("ipc.subscribe_events.invalid_cursor")
            );
        }
        assert!(
            serde_json::from_value::<DurableStreamCursor>(serde_json::json!({
                "after_id": 0,
                "epoch": cursor.epoch,
                "unknown": true,
            }))
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn stored_subscription_preserves_history_filters_and_redacts_nested_strings() {
        let secret = "ghp_0123456789012345678901234567890123456789"; // ubs:ignore - synthetic redaction fixture, not a live credential.
        let epoch = "0123456789abcdef0123456789abcdef";
        let event = crate::storage::StoredEvent {
            id: 42,
            pane_id: 7,
            rule_id: "codex.usage.reached".to_string(),
            agent_type: "codex".to_string(),
            event_type: "usage.reached".to_string(),
            severity: "warning".to_string(),
            confidence: 0.9,
            extracted: Some(serde_json::json!({
                "nested": [{"secret": secret, "count": 3}],
                "ordinary": "keep this",
            })),
            matched_text: Some(format!("stored {secret}")),
            segment_id: None,
            detected_at: 1_700_000_000_123,
            dedupe_key: None,
            handled_at: Some(1_700_000_000_456),
            handled_by_workflow_id: Some("workflow-1".to_string()),
            handled_status: Some(format!("completed {secret}")),
        };
        let record = subscribe_stored_event_record(
            &event,
            epoch,
            Some(7),
            Some("WARNING"),
            Some("codex.*"),
            &Redactor::new(),
        )
        .expect("matching stored record");
        assert_eq!(record["cursor"], 42);
        assert_eq!(record["id"], 42);
        assert_eq!(record["cursor_epoch"], epoch);
        assert_eq!(record["detected_at"], 1_700_000_000_123_i64);
        assert_eq!(record["handled"], true);
        assert_eq!(record["extracted"]["nested"][0]["count"], 3);
        assert_eq!(record["extracted"]["ordinary"], "keep this");
        assert!(!serde_json::to_string(&record).unwrap().contains(secret));
        assert!(event.matched_text.as_deref().unwrap().contains(secret));
        assert_eq!(record.as_object().unwrap().len(), 15);
        for (pane, severity, rule) in [
            (Some(8), Some("warning"), Some("codex.*")),
            (Some(7), Some("critical"), Some("codex.*")),
            (Some(7), Some("warning"), Some("claude.*")),
        ] {
            assert!(
                subscribe_stored_event_record(
                    &event,
                    epoch,
                    pane,
                    severity,
                    rule,
                    &Redactor::new()
                )
                .is_none()
            );
        }
    }

    struct CheckpointFlushWriter {
        pending: Vec<u8>,
        flushed: Vec<u8>,
        fail_flush: bool,
    }

    impl socket_transport::AsyncWrite for CheckpointFlushWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.get_mut().pending.extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let writer = self.get_mut();
            if writer.fail_flush {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "checkpoint flush failed",
                )));
            }
            writer.flushed.append(&mut writer.pending);
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn subscription_checkpoint_commits_cursor_only_after_successful_flush() {
        use crate::event_stream::DurableStreamCursor;

        run_async_test(async {
            let cx = crate::cx::Cx::current().expect("runtime context");
            let initial = DurableStreamCursor {
                after_id: 7,
                epoch: "0123456789abcdef0123456789abcdef".to_string(),
            };
            let examined = DurableStreamCursor {
                after_id: 70,
                epoch: initial.epoch.clone(),
            };
            let mut flushed_cursor = Some(initial.clone());
            let mut failed = CheckpointFlushWriter {
                pending: Vec::new(),
                flushed: Vec::new(),
                fail_flush: true,
            };
            let error = write_subscribe_cursor_checkpoint(
                &cx,
                &mut failed,
                Duration::from_secs(1),
                MAX_MESSAGE_SIZE,
                &mut flushed_cursor,
                &examined,
                "examined_through_page",
            )
            .await
            .expect_err("an accepted write is not a successful flush");
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert!(!failed.pending.is_empty(), "write completed before failure");
            assert!(failed.flushed.is_empty());
            assert_eq!(
                flushed_cursor,
                Some(initial),
                "failure must retain the terminal/reconnect cursor"
            );

            let mut successful = CheckpointFlushWriter {
                pending: Vec::new(),
                flushed: Vec::new(),
                fail_flush: false,
            };
            write_subscribe_cursor_checkpoint(
                &cx,
                &mut successful,
                Duration::from_secs(1),
                MAX_MESSAGE_SIZE,
                &mut flushed_cursor,
                &examined,
                "examined_through_page",
            )
            .await
            .expect("checkpoint flush succeeds");
            assert!(successful.pending.is_empty());
            assert_eq!(successful.flushed.last(), Some(&b'\n'));
            let record: serde_json::Value = serde_json::from_slice(&successful.flushed).unwrap();
            assert_eq!(record["type"], "cursor_checkpoint");
            assert_eq!(record["cursor"], 70);
            assert_eq!(record["reason"], "examined_through_page");
            assert_eq!(flushed_cursor, Some(examined));
        });
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_emits_cursor_and_filtered_fields() {
        let event = Event::PatternDetected {
            pane_id: 7,
            pane_uuid: None,
            detection: sample_detection(
                "codex.usage.reached",
                crate::patterns::Severity::Warning,
                "limit reached",
            ),
            event_id: Some(42),
        };
        let rec =
            subscribe_event_record(&event, None, None, None, &Redactor::new()).expect("record");
        assert_eq!(rec["type"], "event");
        assert_eq!(rec["cursor"], 42);
        assert_eq!(rec["id"], 42);
        assert_eq!(rec["pane_id"], 7);
        assert_eq!(rec["rule_id"], "codex.usage.reached");
        assert_eq!(rec["severity"], "warning");
        assert_eq!(rec["event_type"], "usage.reached");
        assert_eq!(rec["handled"], false);
        assert!(rec["handled_status"].is_null());
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_requires_event_id() {
        // No storage id => no cursor => skipped, preserving the per-record
        // resume invariant the DB-cursor follower relies on.
        let event = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection: sample_detection("codex.x", crate::patterns::Severity::Info, "hi"),
            event_id: None,
        };
        assert!(subscribe_event_record(&event, None, None, None, &Redactor::new()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_applies_pane_severity_rule_filters() {
        let event = Event::PatternDetected {
            pane_id: 3,
            pane_uuid: None,
            detection: sample_detection(
                "codex.usage.reached",
                crate::patterns::Severity::Critical,
                "x",
            ),
            event_id: Some(5),
        };
        // Pane filter.
        assert!(subscribe_event_record(&event, Some(4), None, None, &Redactor::new()).is_none());
        assert!(subscribe_event_record(&event, Some(3), None, None, &Redactor::new()).is_some());
        // Severity filter (case-insensitive exact).
        assert!(
            subscribe_event_record(&event, None, Some("warning"), None, &Redactor::new()).is_none()
        );
        assert!(
            subscribe_event_record(&event, None, Some("CRITICAL"), None, &Redactor::new())
                .is_some()
        );
        // Rule-id glob filter.
        assert!(
            subscribe_event_record(&event, None, None, Some("claude_code.*"), &Redactor::new())
                .is_none()
        );
        assert!(
            subscribe_event_record(&event, None, None, Some("codex.*"), &Redactor::new()).is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_redacts_matched_text() {
        // A GitHub-token-shaped fixture in matched_text must be redacted before
        // it ever reaches the socket (defense in depth — INV-RED-1).
        let fixture = "ghp_0123456789012345678901234567890123456789"; // ubs:ignore - synthetic redaction fixture, not a live credential.
        let event = Event::PatternDetected {
            pane_id: 1,
            pane_uuid: None,
            detection: sample_detection("codex.x", crate::patterns::Severity::Info, fixture),
            event_id: Some(9),
        };
        let rec =
            subscribe_event_record(&event, None, None, None, &Redactor::new()).expect("record");
        let matched = rec["matched_text"].as_str().unwrap_or_default();
        assert!(!matched.contains(fixture), "fixture leaked: {matched}");
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_uses_configured_redaction_after_terminal_normalization() {
        let secret = "PRIVATE_ORCHID";
        let split_secret = "PRIVATE_\u{1b}[31mORCHID\u{1b}[0m";
        let redactor = crate::config::RedactionConfig {
            patterns: vec!["PRIVATE_[A-Z]+".to_string()],
            placeholder: "[workspace-hidden]".to_string(),
            ..Default::default()
        }
        .build_redactor()
        .unwrap();
        assert_eq!(Redactor::new().redact(secret), secret);
        let mut detection = sample_detection(
            "codex.usage.reached",
            crate::patterns::Severity::Warning,
            split_secret,
        );
        detection.extracted = serde_json::json!({
            "nested": [{"text": split_secret, "count": 3}],
            "ordinary": "keep this",
        });
        let event = Event::PatternDetected {
            pane_id: 7,
            pane_uuid: None,
            detection,
            event_id: Some(42),
        };
        let record = subscribe_event_record(&event, None, None, None, &redactor).unwrap();
        assert_eq!(record["matched_text"], "[workspace-hidden]");
        assert_eq!(
            record["extracted"]["nested"][0]["text"],
            "[workspace-hidden]"
        );
        assert_eq!(record["extracted"]["nested"][0]["count"], 3);
        assert_eq!(record["extracted"]["ordinary"], "keep this");
        assert_eq!(record["rule_id"], "codex.usage.reached");
        assert_eq!(record["pane_id"], 7);
        assert_eq!(record["cursor"], 42);

        // Per-stream policy does not contaminate another workspace's output.
        let default_record =
            subscribe_event_record(&event, None, None, None, &Redactor::new()).unwrap();
        assert_eq!(default_record["matched_text"], secret);
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_emits_live_pane_discovered_signal() {
        let event = Event::PaneDiscovered {
            pane_id: 11,
            domain: "local".to_string(),
            title: "shell".to_string(),
        };
        let rec =
            subscribe_event_record(&event, None, None, None, &Redactor::new()).expect("record");
        assert_eq!(rec["type"], "event");
        assert!(rec["cursor"].is_null());
        assert!(rec["id"].is_null());
        assert_eq!(rec["pane_id"], 11);
        assert_eq!(rec["event_type"], "pane_discovered");
        assert_eq!(rec["extracted"]["domain"], "local");
        assert_eq!(rec["extracted"]["title"], "shell");
        assert_eq!(rec["handled"], false);
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_redacts_pane_discovered_title_and_domain() {
        // ft-d3x5y: PaneDiscovered is live-only (this IPC stream is its sole
        // surface), so a secret echoed into the pane title/domain must be
        // redacted here too — not just on the PatternDetected branch.
        let fixture = "ghp_0123456789012345678901234567890123456789"; // ubs:ignore - synthetic redaction fixture, not a live credential.
        let event = Event::PaneDiscovered {
            pane_id: 5,
            domain: format!("local:{fixture}"),
            title: format!("$ echo {fixture}"),
        };
        let rec =
            subscribe_event_record(&event, None, None, None, &Redactor::new()).expect("record");
        let domain = rec["extracted"]["domain"].as_str().unwrap_or_default();
        let title = rec["extracted"]["title"].as_str().unwrap_or_default();
        assert!(!domain.contains(fixture), "domain leaked secret: {domain}");
        assert!(!title.contains(fixture), "title leaked secret: {title}");

        // Non-secret titles still pass through (redaction is targeted, not
        // destructive) — keeps the live signal useful.
        let plain = subscribe_event_record(
            &Event::PaneDiscovered {
                pane_id: 5,
                domain: "local".to_string(),
                title: "shell".to_string(),
            },
            None,
            None,
            None,
            &Redactor::new(),
        )
        .expect("record");
        assert_eq!(plain["extracted"]["domain"], "local");
        assert_eq!(plain["extracted"]["title"], "shell");
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_emits_live_pane_disappeared_signal() {
        let event = Event::PaneDisappeared { pane_id: 1 };
        let rec =
            subscribe_event_record(&event, None, None, None, &Redactor::new()).expect("record");
        assert_eq!(rec["type"], "event");
        assert!(rec["cursor"].is_null());
        assert_eq!(rec["pane_id"], 1);
        assert_eq!(rec["event_type"], "pane_disappeared");
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_filters_live_signals_by_pane_only() {
        let event = Event::PaneDisappeared { pane_id: 9 };
        assert!(subscribe_event_record(&event, Some(8), None, None, &Redactor::new()).is_none());
        assert!(subscribe_event_record(&event, Some(9), None, None, &Redactor::new()).is_some());
        assert!(
            subscribe_event_record(&event, None, Some("warning"), None, &Redactor::new()).is_none()
        );
        assert!(
            subscribe_event_record(&event, None, None, Some("codex.*"), &Redactor::new()).is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn subscribe_event_record_ignores_non_watch_signal_events() {
        let event = Event::WorkflowCompleted {
            workflow_id: "w1".to_string(),
            success: true,
            reason: None,
        };
        assert!(subscribe_event_record(&event, None, None, None, &Redactor::new()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn watch_lag_gap_record_is_explicit_not_silent() {
        // ft-7h5da.4.2: broadcast lag surfaces as an explicit gap record with a
        // non-silent missed_count, never a dropped event.
        let rec = watch_lag_gap_record(7);
        assert_eq!(rec["type"], "gap");
        assert_eq!(rec["reason"], "broadcast_lag");
        assert_eq!(rec["missed_count"], 7);
        assert!(rec["cursor"].is_null());
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

    #[cfg(any(unix, windows))]
    #[test]
    fn rpc_response_wire_cap_preserves_admitted_effect_receipt() {
        let mut response =
            IpcResponse::ok_with_data(serde_json::json!({"large": "\"\\\n".repeat(8192)}));
        response.rpc = Some(IpcRpcReceipt {
            correlation_hash: "sha256:owned-test-correlation".to_string(),
            child_identity_hash: Some("sha256:owned-test-child".to_string()),
            admission: IpcRpcAdmission::Started,
            settlement: IpcRpcSettlement::Exited,
            effects: IpcRpcEffects::ResponseReported,
            audit_delivery: IpcRpcAuditDelivery::Confirmed,
            stdout_bytes: Some(8192),
            stderr_bytes: Some(0),
        });
        let bytes = serialize_ipc_response_bounded(response.clone(), 1024).unwrap();
        assert!(bytes.len() <= 1024);
        let fallback: IpcResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            fallback.error_code.as_deref(),
            Some("ipc.response_size_exceeded")
        );
        let receipt = fallback.rpc.unwrap();
        assert_eq!(receipt.admission, IpcRpcAdmission::Started);
        assert_eq!(receipt.effects, IpcRpcEffects::ResponseReported);
        assert_eq!(receipt.audit_delivery, IpcRpcAuditDelivery::Confirmed);
        assert!(
            serialize_ipc_response_bounded(response, 1).is_err(),
            "never emit a misleading fallback without the disposition"
        );
    }

    #[test]
    fn ipc_connection_task_drain_requires_zero_active_and_unacknowledged_authority() {
        assert_eq!(
            classify_ipc_connection_task_drain(false, JoinSetSettlement::Settled),
            IpcConnectionTaskDrainOutcome::Settled
        );
        assert_eq!(
            classify_ipc_connection_task_drain(
                false,
                JoinSetSettlement::Incomplete {
                    active_tasks: 0,
                    unacknowledged_tasks: 1,
                },
            ),
            IpcConnectionTaskDrainOutcome::Incomplete {
                active_tasks: 0,
                unacknowledged_tasks: 1,
            }
        );
        assert_eq!(
            classify_ipc_connection_task_drain(true, JoinSetSettlement::Settled),
            IpcConnectionTaskDrainOutcome::Settled
        );
        assert_eq!(
            classify_ipc_connection_task_drain(
                true,
                JoinSetSettlement::Incomplete {
                    active_tasks: 2,
                    unacknowledged_tasks: 1,
                },
            ),
            IpcConnectionTaskDrainOutcome::TimedOut {
                active_tasks: 2,
                unacknowledged_tasks: 1,
            }
        );
    }

    #[test]
    fn ipc_connection_admission_never_exceeds_the_configured_ceiling() {
        assert_eq!(
            classify_ipc_connection_admission(0, 1),
            IpcConnectionAdmission::Admit
        );
        assert_eq!(
            classify_ipc_connection_admission(1023, 1024),
            IpcConnectionAdmission::Admit
        );
        assert_eq!(
            classify_ipc_connection_admission(1024, 1024),
            IpcConnectionAdmission::RejectAtCapacity
        );
        assert_eq!(
            classify_ipc_connection_admission(1025, 1024),
            IpcConnectionAdmission::RejectAtCapacity
        );
        assert_eq!(
            classify_ipc_connection_admission(0, 0),
            IpcConnectionAdmission::RejectAtCapacity
        );
    }

    #[test]
    fn ipc_line_read_deadline_classifier_is_explicit() {
        assert_eq!(
            classify_ipc_line_read_deadline(false),
            IpcLineReadDeadlineOutcome::Completed
        );
        assert_eq!(
            classify_ipc_line_read_deadline(true),
            IpcLineReadDeadlineOutcome::TimedOut
        );
    }

    #[test]
    fn ipc_io_budget_is_absolute_and_fails_closed_at_the_exact_deadline() {
        let limit = Duration::from_millis(100);
        assert_eq!(remaining_ipc_io_budget(limit, Duration::ZERO), Some(limit));
        assert_eq!(
            remaining_ipc_io_budget(limit, Duration::from_millis(40)),
            Some(Duration::from_millis(60))
        );
        assert_eq!(remaining_ipc_io_budget(limit, limit), None);
        assert_eq!(
            remaining_ipc_io_budget(limit, Duration::from_millis(101)),
            None
        );
    }

    #[test]
    fn ipc_json_serialization_is_bounded_at_the_exact_byte_limit() {
        let value = serde_json::json!({"payload": "bounded"});
        let expected = serde_json::to_vec(&value).expect("fixture serializes");

        let exact = serialize_ipc_json_bounded(&value, expected.len())
            .expect("exact byte limit must be admitted");
        assert_eq!(exact, expected);

        let error = serialize_ipc_json_bounded(&value, expected.len() - 1)
            .expect_err("one byte below the encoded length must fail closed");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn ipc_runtime_limits_resolve_all_resource_defaults_and_overrides() {
        let defaults = resolve_limits(None);
        assert_eq!(
            defaults.max_concurrent_connections,
            crate::tuning_config::IpcTuning::DEFAULT_MAX_CONCURRENT_CONNECTIONS
        );
        assert_eq!(
            defaults.initial_request_timeout_ms,
            crate::tuning_config::IpcTuning::DEFAULT_INITIAL_REQUEST_TIMEOUT_MS
        );
        assert_eq!(
            defaults.io_timeout_ms,
            crate::tuning_config::IpcTuning::DEFAULT_IO_TIMEOUT_MS
        );
        assert!(defaults.resource_bounds_are_valid());

        let mut tuning = crate::tuning_config::IpcTuning::default();
        // Keep the override distinct from the default while remaining inside
        // the aggregate request-line buffering envelope. At 2,048, the two
        // delimiter bytes per connection intentionally push the 128 KiB
        // message default just over the 512 MiB process cap.
        tuning.max_concurrent_connections = 1536;
        tuning.initial_request_timeout_ms = 750;
        tuning.io_timeout_ms = 45_000;
        let resolved = resolve_limits(Some(&tuning));
        assert_eq!(resolved.max_concurrent_connections, 1536);
        assert_eq!(resolved.initial_request_timeout_ms, 750);
        assert_eq!(resolved.io_timeout_ms, 45_000);
        assert!(resolved.resource_bounds_are_valid());
    }

    #[test]
    fn ipc_runtime_limit_validation_rejects_each_out_of_range_resource_bound() {
        let defaults = resolve_limits(None);

        let mut invalid = defaults;
        invalid.max_message_size = crate::tuning_config::IpcTuning::MIN_MAX_MESSAGE_SIZE - 1;
        assert!(!invalid.resource_bounds_are_valid());
        invalid.max_message_size = crate::tuning_config::IpcTuning::MAX_MAX_MESSAGE_SIZE + 1;
        assert!(!invalid.resource_bounds_are_valid());

        invalid = defaults;
        invalid.max_concurrent_connections = 0;
        assert!(!invalid.resource_bounds_are_valid());
        invalid.max_concurrent_connections =
            crate::tuning_config::IpcTuning::MAX_MAX_CONCURRENT_CONNECTIONS + 1;
        assert!(!invalid.resource_bounds_are_valid());

        invalid = defaults;
        invalid.initial_request_timeout_ms =
            crate::tuning_config::IpcTuning::MIN_INITIAL_REQUEST_TIMEOUT_MS - 1;
        assert!(!invalid.resource_bounds_are_valid());
        invalid.initial_request_timeout_ms =
            crate::tuning_config::IpcTuning::MAX_INITIAL_REQUEST_TIMEOUT_MS + 1;
        assert!(!invalid.resource_bounds_are_valid());

        invalid = defaults;
        invalid.io_timeout_ms = crate::tuning_config::IpcTuning::MIN_IO_TIMEOUT_MS - 1;
        assert!(!invalid.resource_bounds_are_valid());
        invalid.io_timeout_ms = crate::tuning_config::IpcTuning::MAX_IO_TIMEOUT_MS + 1;
        assert!(!invalid.resource_bounds_are_valid());

        invalid = defaults;
        invalid.max_message_size = crate::tuning_config::IpcTuning::MAX_MAX_MESSAGE_SIZE;
        invalid.max_concurrent_connections =
            crate::tuning_config::IpcTuning::MAX_MAX_CONCURRENT_CONNECTIONS;
        assert!(!invalid.resource_bounds_are_valid());

        invalid.max_concurrent_connections = 3;
        assert!(invalid.resource_bounds_are_valid());
    }

    #[test]
    fn registry_lock_error_response_preserves_finite_context_and_timeout_classes() {
        let cases = [
            (LockAcquireError::Cancelled, "ipc.request_cancelled"),
            (
                LockAcquireError::DeadlineExceeded,
                "ipc.request_deadline_exceeded",
            ),
            (
                LockAcquireError::PollQuotaExhausted,
                "ipc.request_poll_quota_exhausted",
            ),
            (
                LockAcquireError::CostBudgetExhausted,
                "ipc.request_cost_budget_exhausted",
            ),
            (
                LockAcquireError::ContextFailure,
                "ipc.request_context_failed",
            ),
            (
                LockAcquireError::TimedOut { deadline_nanos: 41 },
                "ipc.registry_lock_timed_out",
            ),
            (LockAcquireError::Poisoned, "ipc.registry_lock_failed"),
            (
                LockAcquireError::PolledAfterCompletion,
                "ipc.registry_lock_failed",
            ),
        ];

        for (error, expected_code) in cases {
            let response = registry_lock_error_response(error);
            assert!(!response.ok);
            assert_eq!(response.error_code.as_deref(), Some(expected_code));
        }
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

    /// ft-xbnl0.2.3 Cx-first: remaining 5 IpcClient `_with_cx`
    /// entries (status, pane_state, set_pane_priority,
    /// clear_pane_priority, call_rpc) must match their legacy
    /// siblings on the missing-socket path. Consolidated test
    /// covers all 5 with one fixture.
    #[cfg(unix)]
    #[test]
    fn remaining_ipc_with_cx_entries_match_legacy_on_missing_socket() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let bogus = std::env::temp_dir().join(format!(
                    "ft-rusticmaple-tick78-ipc-trail-complete-{}-{}.sock",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                assert!(!bogus.exists());
                let client = IpcClient::new(&bogus);
                let cx = crate::cx::for_request();

                // status_with_cx
                let err = client
                    .status_with_cx(&cx)
                    .await
                    .expect_err("status_with_cx on missing socket");
                assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));

                // pane_state_with_cx
                let err = client
                    .pane_state_with_cx(&cx, 42)
                    .await
                    .expect_err("pane_state_with_cx on missing socket");
                assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));

                // set_pane_priority_with_cx
                let err = client
                    .set_pane_priority_with_cx(&cx, 42, 7, None)
                    .await
                    .expect_err("set_pane_priority_with_cx on missing socket");
                assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));

                // clear_pane_priority_with_cx
                let err = client
                    .clear_pane_priority_with_cx(&cx, 42)
                    .await
                    .expect_err("clear_pane_priority_with_cx on missing socket");
                assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));

                // call_rpc_with_cx (takes request_id)
                let err = client
                    .call_rpc_with_cx(&cx, vec!["test".to_string()], Some("corr-78".to_string()))
                    .await
                    .expect_err("call_rpc_with_cx on missing socket");
                assert!(matches!(err, UserVarError::WatcherNotRunning { .. }));
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    /// ft-xbnl0.2.3 Cx-first: `ping_with_cx` must match the
    /// legacy `ping` on the missing-socket path. Both variants
    /// return `WatcherNotRunning` with the same socket_path
    /// string. The Cx-first path's pre-flight checkpoint doesn't
    /// short-circuit an uncancelled cx, so the socket check
    /// still fires as the next step after the checkpoint.
    #[cfg(unix)]
    #[test]
    fn ping_with_cx_returns_watcher_not_running_on_missing_socket() {
        use crate::runtime_async::CompatRuntime;
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("build test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let bogus = std::env::temp_dir().join(format!(
                    "ft-rusticmaple-tick77-ipc-no-such-{}-{}.sock",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos())
                        .unwrap_or(0),
                ));
                assert!(!bogus.exists(), "precondition: test socket must not exist");
                let client = IpcClient::new(&bogus);
                let cx = crate::cx::for_request();

                let legacy_err = client
                    .ping()
                    .await
                    .expect_err("legacy ping must error on missing socket");
                let cx_err = client
                    .ping_with_cx(&cx)
                    .await
                    .expect_err("Cx-first ping must error on missing socket");

                let paths = match (&legacy_err, &cx_err) {
                    (
                        UserVarError::WatcherNotRunning {
                            socket_path: legacy_path,
                        },
                        UserVarError::WatcherNotRunning {
                            socket_path: cx_path,
                        },
                    ) => Some((legacy_path, cx_path)),
                    _ => None,
                };
                assert!(
                    paths.is_some(),
                    "expected WatcherNotRunning on both paths, got {legacy_err:?} and {cx_err:?}"
                );
                if let Some((legacy_path, cx_path)) = paths {
                    assert_eq!(legacy_path, cx_path);
                    assert!(cx_path.contains("tick77-ipc-no-such"));
                }
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
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
    ) -> (mpsc::Sender<()>, crate::runtime_async::task::JoinHandle<()>) {
        let server = IpcServer::bind(socket_path).await.unwrap();
        let event_bus = Arc::new(EventBus::new(100));
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let handle = crate::runtime_async::task::spawn(async move {
            server
                .run_with_auth(event_bus, Some(auth), shutdown_rx)
                .await;
        });

        crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;
        (shutdown_tx, handle)
    }

    async fn send_shutdown(shutdown_tx: &mpsc::Sender<()>) {
        let _ = mpsc_send_value(shutdown_tx, ()).await;
    }

    #[test]
    fn shutdown_signal_pending_reports_closed_channel() {
        run_async_test(async {
            let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
            drop(shutdown_tx);

            assert!(shutdown_signal_pending(&mut shutdown_rx).await);
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `run_with_cx` must exit cleanly
    /// when the shutdown channel fires. Verifies that the
    /// cx-first run path reaches completion, removes the socket
    /// file, and releases the bound path (a follow-up bind must
    /// succeed).
    #[test]
    fn ipc_server_run_with_cx_exits_cleanly_on_shutdown() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("run-cx-test.sock");
            let cx = crate::cx::for_testing();

            let server = IpcServer::bind_with_cx(&cx, &socket_path)
                .await
                .expect("bind_with_cx");

            let event_bus = Arc::new(EventBus::new(16));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let run_cx = cx.clone();
            let handle = crate::runtime_async::task::spawn(async move {
                server.run_with_cx(&run_cx, event_bus, shutdown_rx).await;
            });

            // Let the accept loop start, then shut it down.
            crate::runtime_async::sleep(std::time::Duration::from_millis(20)).await;
            send_shutdown(&shutdown_tx).await;
            let _ = handle.await;

            assert!(
                !socket_path.exists(),
                "socket file should be removed after clean shutdown"
            );
        });
    }

    #[test]
    fn ipc_server_run_with_auth_legacy_entry_exits_cleanly_on_shutdown() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("run-auth-legacy.sock");

            let server = IpcServer::bind(&socket_path).await.expect("bind");
            let event_bus = Arc::new(EventBus::new(16));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let handle = crate::runtime_async::task::spawn(async move {
                server.run_with_auth(event_bus, None, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(20)).await;
            send_shutdown(&shutdown_tx).await;
            let _ = handle.await;

            assert!(
                !socket_path.exists(),
                "legacy run_with_auth entry should remove the socket on shutdown"
            );
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `bind_with_cx` must successfully
    /// bind when given a fresh, uncancelled cx — producing a
    /// server with the same socket path as the legacy `bind`.
    #[test]
    fn ipc_server_bind_with_cx_matches_legacy() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("bind-cx-test.sock");
            let cx = crate::cx::for_testing();

            let server = IpcServer::bind_with_cx(&cx, &socket_path)
                .await
                .expect("bind_with_cx should succeed on fresh cx");

            assert_eq!(server.socket_path(), socket_path.as_path());
            assert!(socket_path.exists(), "socket file should exist after bind");
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `bind_with_cx` must return a
    /// cancellation error when given a pre-cancelled cx,
    /// without creating the socket or parent directory.
    #[test]
    fn ipc_server_bind_with_precancelled_cx_fails_before_filesystem_mutation() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("bind-cancelled.sock");
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel bind test"),
            );

            let result = IpcServer::bind_with_cx(&cx, &socket_path).await;
            let Some(err) = result.err() else {
                assert!(false, "bind_with_cx should fail on cancelled cx");
                return;
            };

            assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
            assert!(
                err.to_string().contains("cancelled"),
                "error should mention cancellation: {err}"
            );
            assert!(
                !socket_path.exists(),
                "socket must not be created when cx is pre-cancelled"
            );
        });
    }

    #[test]
    fn ipc_server_bind_refuses_to_unlink_non_socket_path() {
        run_async_test(async {
            let temp_dir = tempfile::tempdir().expect("tempdir");
            let socket_path = temp_dir.path().join("ipc.sock");
            std::fs::write(&socket_path, b"not a socket").expect("write sentinel file");
            let cx = crate::cx::for_testing();

            let result = IpcServer::bind_with_cx(&cx, &socket_path).await;
            assert!(
                result.is_err(),
                "bind must reject non-socket path instead of unlinking it"
            );
            let err = match result {
                Err(err) => err,
                Ok(_) => return,
            };

            assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
            assert!(
                err.to_string().contains("non-socket IPC bind path"),
                "error should explain refusal: {err}"
            );
            let contents = std::fs::read(&socket_path).expect("sentinel file should remain");
            assert_eq!(contents, b"not a socket");
        });
    }

    #[test]
    fn shutdown_signal_pending_with_cx_ignores_cancelled_receive() {
        run_async_test(async {
            let (_shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("test shutdown cancellation"),
            );

            assert!(!shutdown_signal_pending_with_cx(&mut shutdown_rx, &cx).await);
        });
    }

    /// A connection's wait for server shutdown ends on either signal: the
    /// server's `connection_shutdown`, or its own Cx's cancellation. Before,
    /// a cancelled Cx made each 10 ms sleep complete at once with Ok, and the
    /// wait went on, one scheduler trip per iteration, until the server
    /// cancelled `connection_shutdown`; here that leaves it pending and the
    /// 2 s bound wins.
    #[test]
    fn wait_for_ipc_shutdown_ends_on_its_cx_cancellation() {
        use futures::future::Either;
        run_async_test(async {
            let ctx = IpcHandlerContext::new(Arc::new(EventBus::new(16)));
            let cx = crate::cx::for_testing();

            let idle = futures::future::select(
                Box::pin(wait_for_ipc_shutdown(&cx, &ctx)),
                Box::pin(crate::runtime_async::sleep(Duration::from_millis(50))),
            )
            .await;
            assert!(
                matches!(idle, Either::Right(_)),
                "with neither signal the wait must stay pending"
            );

            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("test connection cancellation"),
            );
            let cancelled = futures::future::select(
                Box::pin(wait_for_ipc_shutdown(&cx, &ctx)),
                Box::pin(crate::runtime_async::sleep(Duration::from_secs(2))),
            )
            .await;
            assert!(
                matches!(cancelled, Either::Left(_)),
                "a cancelled Cx must end the wait without connection_shutdown"
            );
            assert!(!ctx.connection_shutdown.is_requested());

            let server_cx = crate::cx::for_testing();
            ctx.connection_shutdown.request();
            let shutdown = futures::future::select(
                Box::pin(wait_for_ipc_shutdown(&server_cx, &ctx)),
                Box::pin(crate::runtime_async::sleep(Duration::from_secs(2))),
            )
            .await;
            assert!(
                matches!(shutdown, Either::Left(_)),
                "connection_shutdown must end the wait"
            );
        });
    }

    /// A request whose `cx` is cancelled while it waits for the watcher's
    /// reply ends at once. Before, nothing woke that read on cancellation:
    /// it waited for the reply or the whole I/O timeout (30 s by default;
    /// ft-3268r), and here the 10 s bound would have ended it instead.
    #[test]
    fn ipc_client_request_ends_when_cancelled_while_awaiting_the_reply() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("silent.sock");
            let listener = crate::runtime_async::unix::bind(&socket_path)
                .await
                .expect("bind silent watcher");
            // Accept the request and never answer it.
            let _silent_watcher = crate::runtime_async::task::spawn(async move {
                let held = listener.accept().await;
                crate::runtime_async::sleep(Duration::from_secs(15)).await;
                drop(held);
            });

            let client = IpcClient::new(&socket_path);
            let cx = crate::cx::for_testing();
            let cancel_cx = cx.clone();
            let _canceller = crate::runtime_async::task::spawn(async move {
                crate::runtime_async::sleep(Duration::from_millis(200)).await;
                cancel_cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("caller gave up on the reply"),
                );
            });

            let started = std::time::Instant::now();
            let outcome = crate::runtime_async::timeout_with_cx(
                &crate::cx::for_request(),
                Duration::from_secs(10),
                client.ping_with_cx(&cx),
            )
            .await;
            let elapsed = started.elapsed();

            let error = outcome
                .expect("cancellation must end the request before the 10 s bound")
                .expect_err("the watcher never answers, so only cancellation can end the request");
            let UserVarError::IpcSendFailed { message } = &error else {
                panic!("expected IpcSendFailed, got {error:?}");
            };
            assert!(
                message.starts_with("cancelled during read"),
                "the cancellation must be reported as such; got: {message}"
            );
            assert!(
                elapsed < Duration::from_secs(5),
                "the request must end soon after the 200 ms cancel; took {elapsed:?}"
            );
        });
    }

    /// A deadline in the caller's budget keeps the I/O-timeout label, so a
    /// message classifier does not count it as a cancellation.
    #[test]
    fn ipc_client_wait_ended_labels_deadlines_apart_from_cancellation() {
        let cancelled = crate::cx::for_testing();
        cancelled.cancel_with(crate::outcome::CancelKind::User, Some("caller cancel"));
        let UserVarError::IpcSendFailed { message } =
            ipc_client_wait_ended(&cancelled, "read", &"cause")
        else {
            panic!("expected IpcSendFailed");
        };
        assert_eq!(message, "cancelled during read: cause");

        let expired = crate::cx::for_testing();
        expired.cancel_with(
            crate::outcome::CancelKind::Deadline,
            Some("budget deadline"),
        );
        let UserVarError::IpcSendFailed { message } =
            ipc_client_wait_ended(&expired, "connect", &"cause")
        else {
            panic!("expected IpcSendFailed");
        };
        assert_eq!(message, "IPC I/O deadline exceeded");
    }

    #[derive(Default)]
    struct AcceptWaitWakeProbe(AtomicU64);

    impl std::task::Wake for AcceptWaitWakeProbe {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn accept_wait_virtual_context(
        budget: crate::cx::Budget,
    ) -> (
        crate::cx::Cx,
        Arc<asupersync::time::VirtualClock>,
        asupersync::time::TimerDriverHandle,
    ) {
        let clock = Arc::new(asupersync::time::VirtualClock::new());
        let timer = asupersync::time::TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
        let identity = crate::cx::for_testing();
        let cx = crate::cx::Cx::new_with_drivers(
            identity.region_id(),
            identity.task_id(),
            budget,
            None,
            None,
            None,
            Some(timer.clone()),
            None,
        );
        (cx, clock, timer)
    }

    #[test]
    fn connected_shutdown_wait_is_timer_free_and_wakes_all_live_handlers() {
        use std::future::Future;
        use std::task::{Context, Waker};

        let (cx, clock, timer) = accept_wait_virtual_context(crate::cx::Budget::INFINITE);
        let ctx = Arc::new(IpcHandlerContext::new(Arc::new(EventBus::new(16))));
        let cloned_ctx = Arc::clone(&ctx);
        let first_probe = Arc::new(AcceptWaitWakeProbe::default());
        let second_probe = Arc::new(AcceptWaitWakeProbe::default());
        let abandoned_probe = Arc::new(AcceptWaitWakeProbe::default());
        let first_waker = Waker::from(Arc::clone(&first_probe));
        let second_waker = Waker::from(Arc::clone(&second_probe));
        let abandoned_waker = Waker::from(Arc::clone(&abandoned_probe));
        let mut first_task_cx = Context::from_waker(&first_waker);
        let mut second_task_cx = Context::from_waker(&second_waker);
        let mut abandoned_task_cx = Context::from_waker(&abandoned_waker);
        let mut first = Box::pin(wait_for_ipc_shutdown(&cx, &ctx));
        let mut second = Box::pin(wait_for_ipc_shutdown(&cx, &cloned_ctx));
        let mut abandoned = Box::pin(wait_for_ipc_shutdown(&cx, &ctx));

        assert!(first.as_mut().poll(&mut first_task_cx).is_pending());
        assert!(second.as_mut().poll(&mut second_task_cx).is_pending());
        assert!(abandoned.as_mut().poll(&mut abandoned_task_cx).is_pending());
        drop(abandoned);
        assert_eq!(
            timer.pending_count(),
            0,
            "connected idle handlers need no timer"
        );
        clock.advance_to(asupersync::Time::from_secs(3_600));
        assert_eq!(timer.process_timers(), 0);
        assert_eq!(first_probe.0.load(Ordering::SeqCst), 0);
        assert_eq!(second_probe.0.load(Ordering::SeqCst), 0);

        assert!(!ctx.connection_shutdown.request());
        assert!(first_probe.0.load(Ordering::SeqCst) > 0);
        assert!(second_probe.0.load(Ordering::SeqCst) > 0);
        assert_eq!(
            abandoned_probe.0.load(Ordering::SeqCst),
            0,
            "dropping a connection must remove its shutdown waiter"
        );
        assert!(first.as_mut().poll(&mut first_task_cx).is_ready());
        assert!(second.as_mut().poll(&mut second_task_cx).is_ready());
        assert!(
            !cx.is_cancel_requested(),
            "server stop must not cancel the caller Cx"
        );

        // A request before registration is sticky and repeated requests remain
        // idempotent, so newly arriving waiters also finish without a timer.
        assert!(ctx.connection_shutdown.request());
        let mut late = Box::pin(wait_for_ipc_shutdown(&cx, &ctx));
        assert!(late.as_mut().poll(&mut first_task_cx).is_ready());
        assert_eq!(timer.pending_count(), 0);
    }

    #[test]
    fn connected_shutdown_wait_observes_cancellation_before_and_after_registration() {
        use std::future::Future;
        use std::task::{Context, Waker};

        for cancelled_before_wait in [false, true] {
            let (cx, _clock, timer) = accept_wait_virtual_context(crate::cx::Budget::INFINITE);
            let ctx = IpcHandlerContext::new(Arc::new(EventBus::new(16)));
            let probe = Arc::new(AcceptWaitWakeProbe::default());
            let waker = Waker::from(Arc::clone(&probe));
            let mut task_cx = Context::from_waker(&waker);
            if cancelled_before_wait {
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("connection cancelled"),
                );
            }
            let mut wait = Box::pin(wait_for_ipc_shutdown(&cx, &ctx));
            if !cancelled_before_wait {
                assert!(wait.as_mut().poll(&mut task_cx).is_pending());
                assert_eq!(timer.pending_count(), 0);
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("connection cancelled"),
                );
                assert!(probe.0.load(Ordering::SeqCst) > 0);
            }
            assert!(wait.as_mut().poll(&mut task_cx).is_ready());
            assert!(!ctx.connection_shutdown.is_requested());
            assert_eq!(timer.pending_count(), 0);
        }
    }

    #[test]
    fn connected_shutdown_wait_arms_only_its_context_deadline() {
        let deadline = asupersync::Time::from_secs(5);
        let observed = Arc::new(std::sync::Mutex::new(None));
        let task_observed = Arc::clone(&observed);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x135C_CC01)
                .with_auto_advance()
                .max_steps(10_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(
                region,
                asupersync::Budget::new().with_deadline(deadline),
                async move {
                    let cx = crate::cx::Cx::current().expect("LabRuntime context");
                    let ctx = IpcHandlerContext::new(Arc::new(EventBus::new(16)));
                    wait_for_ipc_shutdown(&cx, &ctx).await;
                    assert!(!ctx.connection_shutdown.is_requested());
                    *task_observed.lock().expect("record deadline result") =
                        Some(cx.checkpoint().is_err());
                },
            )
            .expect("spawn connection deadline waiter");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_idle();
        assert!(observed.lock().expect("deadline result").is_none());
        assert_eq!(runtime.pending_timer_count(), 1);

        let report = runtime.run_with_auto_advance();
        assert_eq!(
            report.auto_advances, 1,
            "only the context deadline may end an idle connection wait"
        );
        assert_eq!(runtime.now(), deadline);
        assert_eq!(*observed.lock().expect("deadline result"), Some(true));
        assert_eq!(runtime.pending_timer_count(), 0);
    }

    /// #135: advancing an empty server's clock cannot generate idle wakes.
    /// Both a shutdown message and sender closure wake the registered wait.
    #[test]
    fn idle_accept_wait_arms_no_timer_and_wakes_on_shutdown() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        for close_channel in [false, true] {
            let (cx, clock, timer) = accept_wait_virtual_context(crate::cx::Budget::INFINITE);
            let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
            let mut tasks = crate::runtime_async::task::JoinSet::new();
            let probe = Arc::new(AcceptWaitWakeProbe::default());
            let waker = Waker::from(Arc::clone(&probe));
            let mut task_cx = Context::from_waker(&waker);
            let mut wait = Box::pin(next_accept_loop_wake(
                &cx,
                &mut shutdown_rx,
                std::future::pending::<u8>(),
                &mut tasks,
            ));
            assert!(wait.as_mut().poll(&mut task_cx).is_pending());
            assert_eq!(timer.pending_count(), 0, "empty IPC must arm no timer");
            clock.advance_to(asupersync::Time::from_secs(3_600));
            assert_eq!(timer.process_timers(), 0);
            assert_eq!(probe.0.load(Ordering::SeqCst), 0, "idle must not self-wake");

            if close_channel {
                drop(shutdown_tx);
            } else {
                let mut send = std::pin::pin!(shutdown_tx.send(&cx, ()));
                assert!(matches!(
                    send.as_mut().poll(&mut task_cx),
                    Poll::Ready(Ok(()))
                ));
            }
            assert!(
                probe.0.load(Ordering::SeqCst) > 0,
                "shutdown must wake the waiter"
            );
            assert!(matches!(
                wait.as_mut().poll(&mut task_cx),
                Poll::Ready(AcceptLoopWake::Shutdown)
            ));
            drop(wait);
            assert_eq!(timer.pending_count(), 0);
            assert!(tasks.is_empty());
        }
    }

    /// Shutdown and cancellation win over a simultaneously ready client.
    #[test]
    fn next_accept_loop_wake_covers_shutdown_cancellation_and_clients() {
        run_async_test(async {
            let cx = crate::cx::for_testing();
            let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
            let mut tasks = crate::runtime_async::task::JoinSet::new();

            let wake =
                next_accept_loop_wake(&cx, &mut shutdown_rx, std::future::ready(7_u8), &mut tasks)
                    .await;
            assert!(matches!(wake, AcceptLoopWake::Accepted(7)), "got {wake:?}");

            send_shutdown(&shutdown_tx).await;
            let wake =
                next_accept_loop_wake(&cx, &mut shutdown_rx, std::future::ready(7_u8), &mut tasks)
                    .await;
            assert!(matches!(wake, AcceptLoopWake::Shutdown), "got {wake:?}");

            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("accept loop cancellation"),
            );
            let wake =
                next_accept_loop_wake(&cx, &mut shutdown_rx, std::future::ready(7_u8), &mut tasks)
                    .await;
            assert!(
                matches!(
                    wake,
                    AcceptLoopWake::ContextFailed(
                        crate::runtime_async::ContextErrorKind::Cancelled
                    )
                ),
                "got {wake:?}"
            );
        });
    }

    #[test]
    fn accept_wait_reaps_handler_completion_without_new_clients() {
        run_async_test(async {
            let (cx, _clock, timer) = accept_wait_virtual_context(crate::cx::Budget::INFINITE);
            let witness_cx = crate::cx::for_testing();
            let (_shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
            let (release_tx, release_rx) = crate::runtime_async::oneshot::channel::<()>();
            let mut tasks = crate::runtime_async::task::JoinSet::new();
            tasks.spawn(async move {
                crate::runtime_async::oneshot_recv(release_rx)
                    .await
                    .expect("release handler");
            });

            // A client can win while the handler is pending without dropping
            // that handler's ownership or its later completion notification.
            let wake =
                next_accept_loop_wake(&cx, &mut shutdown_rx, std::future::ready(7_u8), &mut tasks)
                    .await;
            assert!(matches!(wake, AcceptLoopWake::Accepted(7)), "got {wake:?}");
            assert_eq!(tasks.len(), 1);

            let mut wait = Box::pin(next_accept_loop_wake(
                &cx,
                &mut shutdown_rx,
                std::future::pending::<u8>(),
                &mut tasks,
            ));
            std::future::poll_fn(|task_cx| {
                assert!(std::future::Future::poll(wait.as_mut(), task_cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert_eq!(
                timer.pending_count(),
                0,
                "pending healthy handlers need no reap timer"
            );
            release_tx.send(()).expect("finish the waiting handler");
            let wake =
                crate::runtime_async::timeout_with_cx(&witness_cx, Duration::from_secs(5), wait)
                    .await
                    .expect("handler completion must wake the accept loop");
            assert!(
                matches!(wake, AcceptLoopWake::ConnectionFinished(Some(Ok(())))),
                "got {wake:?}"
            );
            assert!(
                tasks.is_empty(),
                "completion must immediately free admission capacity"
            );
            assert_eq!(timer.pending_count(), 0);
        });
    }

    #[test]
    fn accept_wait_cancellation_preserves_failure_kind_without_a_timer() {
        use crate::outcome::CancelKind;
        use crate::runtime_async::ContextErrorKind;
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        for (cancel_kind, expected) in [
            (CancelKind::User, ContextErrorKind::Cancelled),
            (CancelKind::Deadline, ContextErrorKind::DeadlineExceeded),
            (CancelKind::PollQuota, ContextErrorKind::PollQuotaExhausted),
            (CancelKind::CostBudget, ContextErrorKind::CostQuotaExhausted),
        ] {
            let (cx, _clock, timer) = accept_wait_virtual_context(crate::cx::Budget::INFINITE);
            let (_shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
            let mut tasks = crate::runtime_async::task::JoinSet::new();
            let probe = Arc::new(AcceptWaitWakeProbe::default());
            let waker = Waker::from(Arc::clone(&probe));
            let mut task_cx = Context::from_waker(&waker);
            let mut wait = Box::pin(next_accept_loop_wake(
                &cx,
                &mut shutdown_rx,
                std::future::pending::<u8>(),
                &mut tasks,
            ));
            assert!(wait.as_mut().poll(&mut task_cx).is_pending());
            assert_eq!(timer.pending_count(), 0);
            cx.cancel_with(cancel_kind, Some("accept wait cancellation proof"));
            assert!(
                probe.0.load(Ordering::SeqCst) > 0,
                "context termination must wake the waiter"
            );
            let Poll::Ready(AcceptLoopWake::ContextFailed(actual)) =
                wait.as_mut().poll(&mut task_cx)
            else {
                panic!("context termination must end the pending accept wait");
            };
            assert_eq!(actual, expected);
            drop(wait);
            assert_eq!(timer.pending_count(), 0);
        }
    }

    #[test]
    fn accept_wait_arms_only_its_context_deadline() {
        let deadline = asupersync::Time::from_secs(5);
        let observed = Arc::new(std::sync::Mutex::new(None));
        let task_observed = Arc::clone(&observed);
        let (_shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(0x135A_CC01)
                .with_auto_advance()
                .max_steps(10_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(
                region,
                asupersync::Budget::new().with_deadline(deadline),
                async move {
                    let cx = crate::cx::Cx::current().expect("LabRuntime context");
                    let mut tasks = crate::runtime_async::task::JoinSet::new();
                    let wake = next_accept_loop_wake(
                        &cx,
                        &mut shutdown_rx,
                        std::future::pending::<u8>(),
                        &mut tasks,
                    )
                    .await;
                    let AcceptLoopWake::ContextFailed(kind) = wake else {
                        panic!("context deadline must end the accept wait, got {wake:?}");
                    };
                    *task_observed.lock().expect("record deadline result") = Some(kind);
                },
            )
            .expect("spawn deadline waiter");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.run_until_idle();
        assert!(observed.lock().expect("deadline result").is_none());
        assert_eq!(runtime.pending_timer_count(), 1);

        let report = runtime.run_with_auto_advance();
        assert_eq!(
            report.auto_advances, 1,
            "only the context deadline may end an idle wait"
        );
        assert_eq!(runtime.now(), deadline);
        assert_eq!(
            *observed.lock().expect("deadline result"),
            Some(crate::runtime_async::ContextErrorKind::DeadlineExceeded)
        );
        assert_eq!(runtime.pending_timer_count(), 0);
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for ipc tests");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        // Absorb TLS destructor panics from asupersync during runtime drop.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        // Clear handle from TLS so it doesn't panic during thread exit.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
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
    fn ipc_read_token_rejects_rpc_mutations_before_handler_dispatch() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("rpc-scope.sock");
            let auth = build_auth("reader", vec![IpcScope::Read], None);
            let (shutdown_tx, server_handle) = start_auth_server(&socket_path, auth).await;
            let client = IpcClient::with_token(&socket_path, "reader");

            for args in [
                vec!["agents", "configure", "--scope", "global"],
                vec!["agents", "configure", "--dry-run"],
                vec!["events", "-v", "annotate", "7", "--note", "note"],
                vec!["events", "--verbose", "triage", "7", "--state", "closed"],
                vec!["eventlog", "-vv", "label", "7", "--add", "reviewed"],
            ] {
                let response = client
                    .call_rpc(args.iter().map(|arg| (*arg).to_string()).collect(), None)
                    .await
                    .unwrap();
                assert!(!response.ok, "{args:?}");
                assert!(
                    response
                        .error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("insufficient scope"),
                    "RPC must be refused at authorization: {args:?} {response:?}"
                );
                assert!(response.rpc.is_none(), "no child was admitted");
            }

            // A read operation passes authorization and reaches the absent
            // handler. This distinguishes the scope gate from a server that
            // rejects all RPCs or failed to start.
            let response = client
                .call_rpc(vec!["agents".to_string(), "list".to_string()], None)
                .await
                .unwrap();
            assert_eq!(
                response.error.as_deref(),
                Some("rpc handler not configured")
            );
            assert!(client.ping().await.unwrap().ok);

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
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            // Give server time to start
            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

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

            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(event_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;
            assert!(socket_path.exists());

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
            assert!(!socket_path.exists());
        });
    }

    #[test]
    fn shutdown_does_not_hang_with_idle_client_connection() {
        run_async_test(async {
            use crate::runtime_async::unix::{self as compat_unix, AsyncWriteExt};

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(event_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

            // Hold a connection open with an incomplete request to simulate an
            // idle/stuck client handler blocked on line reads.
            let mut idle_client = compat_unix::connect(&socket_path).await.unwrap();
            idle_client.write_all(br#"{"type":"ping""#).await.unwrap();

            send_shutdown(&shutdown_tx).await;
            let shutdown_wait =
                crate::runtime_async::timeout(std::time::Duration::from_millis(500), server_handle)
                    .await;
            assert!(
                shutdown_wait.is_ok(),
                "IPC shutdown timed out with idle client connection"
            );
        });
    }

    #[test]
    fn ipc_server_closes_client_that_misses_initial_request_line_deadline() {
        run_async_test(async {
            use crate::runtime_async::unix as compat_unix;

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");
            let mut limits = resolve_limits(None);
            limits.initial_request_timeout_ms = 100;
            limits.io_timeout_ms = 1_000;
            limits.max_concurrent_connections = 4;
            let cx = crate::cx::for_testing();

            let server = IpcServer::bind_with_permissions_and_limits_with_cx(
                &cx,
                &socket_path,
                Some(0o600),
                limits,
            )
            .await
            .unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(event_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(Duration::from_millis(10)).await;
            let stream = compat_unix::connect(&socket_path).await.unwrap();
            let (reader, writer_guard) = stream.into_split();
            let mut lines = compat_unix::lines(compat_unix::buffered(reader));

            let closed = crate::runtime_async::timeout(
                Duration::from_millis(1_000),
                compat_unix::next_line(&mut lines),
            )
            .await
            .expect("initial request-line deadline did not close the connection")
            .expect("reading the deadline-closed connection failed");
            assert!(closed.is_none());

            drop(writer_guard);
            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
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
            let server_handle = crate::runtime_async::task::spawn(async move {
                server
                    .run_with_registry(event_bus, registry, shutdown_rx)
                    .await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

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
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

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

            let Event::UserVarReceived {
                pane_id,
                name,
                payload,
            } = event
            else {
                assert!(false, "Expected UserVarReceived event, got {event:?}");
                return;
            };
            assert_eq!(pane_id, 42);
            assert_eq!(name, "FT_EVENT");
            assert_eq!(payload.event_type, Some("command_start".to_string()));

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
            crate::tailer::StreamingHealth::update_global(crate::tailer::StreamingHealth {
                mode: crate::tailer::TailerMode::Streaming,
                events_processed: 42,
                dirty_ranges_total: 10,
                dirty_rows_total: 80,
                gaps_emitted: 1,
                fallback_count: 2,
                active_panes: 3,
            });

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

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
            let streaming_health = data
                .get("streaming_health")
                .and_then(serde_json::Value::as_object)
                .expect("streaming_health should be present in status payload");
            assert_eq!(
                streaming_health.get("events_processed"),
                Some(&serde_json::json!(42))
            );
            assert_eq!(
                streaming_health.get("dirty_rows_total"),
                Some(&serde_json::json!(80))
            );
            assert_eq!(data.get("search_index"), Some(&serde_json::Value::Null));
            let search_health = data
                .get("search_health")
                .and_then(serde_json::Value::as_object)
                .expect("search_health should be present in status payload");
            assert_eq!(
                search_health.get("background_job_status"),
                Some(&serde_json::json!("paused"))
            );
            assert_eq!(
                search_health.get("indexing_error_count"),
                Some(&serde_json::json!(0))
            );
            let allocator = data
                .get("allocator")
                .and_then(serde_json::Value::as_object)
                .expect("allocator should be present in status payload");
            assert!(matches!(
                allocator.get("backend").and_then(serde_json::Value::as_str),
                Some("system" | "jemalloc")
            ));
            assert!(allocator.get("stats").is_some());
            let pane_arenas = data
                .get("pane_arenas")
                .and_then(serde_json::Value::as_object)
                .expect("pane_arenas should be present in status payload");
            assert_eq!(pane_arenas.get("active_count"), Some(&serde_json::json!(0)));
            assert_eq!(
                pane_arenas.get("source"),
                Some(&serde_json::json!("unavailable"))
            );

            send_shutdown(&shutdown_tx).await;
            let _ = server_handle.await;
        });
    }

    #[test]
    fn ipc_status_with_registry_reports_pane_arenas() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let registry = Arc::new(RwLock::new(PaneRegistry::new()));
            {
                let mut registry = registry.write().await;
                registry.discovery_tick(vec![make_pane_info(7), make_pane_info(9)]);
            }
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_async::task::spawn(async move {
                server
                    .run_with_registry(server_bus, registry, shutdown_rx)
                    .await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

            let client = IpcClient::new(&socket_path);
            let response = client.status().await.unwrap();

            assert!(response.ok);
            let data = response.data.expect("status payload should be present");
            let pane_arenas = data
                .get("pane_arenas")
                .and_then(serde_json::Value::as_object)
                .expect("pane_arenas should be present");
            assert_eq!(pane_arenas.get("active_count"), Some(&serde_json::json!(2)));
            let panes = pane_arenas
                .get("panes")
                .and_then(serde_json::Value::as_array)
                .expect("pane_arenas.panes should be an array");
            assert_eq!(panes.len(), 2);
            let pane_ids: Vec<_> = panes
                .iter()
                .filter_map(|value| value.get("pane_id").and_then(serde_json::Value::as_u64))
                .collect();
            assert_eq!(pane_ids, vec![7, 9]);
            assert!(panes.iter().all(|pane| pane.get("arena_id").is_some()));
            assert!(panes.iter().all(|pane| {
                pane.get("tracked_bytes")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|value| value > 0)
            }));
            assert!(panes.iter().all(|pane| {
                let tracked = pane
                    .get("tracked_bytes")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default();
                pane.get("peak_tracked_bytes")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|peak| peak >= tracked)
            }));
            assert!(panes.iter().all(|pane| {
                pane.get("updates")
                    .and_then(serde_json::Value::as_u64)
                    .is_some_and(|value| value >= 1)
            }));

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
            use crate::runtime_async::unix::{self as compat_unix, AsyncWriteExt};

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

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
            use crate::runtime_async::unix::{self as compat_unix, AsyncWriteExt};

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");

            let server = IpcServer::bind(&socket_path).await.unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

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
    fn ipc_server_applies_configured_max_message_size() {
        run_async_test(async {
            use crate::runtime_async::unix::{self as compat_unix, AsyncWriteExt};

            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("test.sock");
            let limits = IpcRuntimeLimits {
                max_message_size: crate::tuning_config::IpcTuning::MIN_MAX_MESSAGE_SIZE,
                max_concurrent_connections: 4,
                initial_request_timeout_ms: 500,
                io_timeout_ms: 500,
            };
            let cx = crate::cx::for_testing();

            let server = IpcServer::bind_with_permissions_and_limits_with_cx(
                &cx,
                &socket_path,
                Some(0o600),
                limits,
            )
            .await
            .unwrap();
            let event_bus = Arc::new(EventBus::new(100));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

            let server_bus = event_bus.clone();
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

            let request = IpcRequest::UserVar {
                pane_id: 1,
                name: "TEST".to_string(),
                value: "x".repeat(20 * 1024),
            };
            let request_json = serde_json::to_string(&request).unwrap();
            assert!(request_json.len() > limits.max_message_size);
            assert!(request_json.len() < MAX_MESSAGE_SIZE);

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
            let server_handle = crate::runtime_async::task::spawn(async move {
                server.run(server_bus, shutdown_rx).await;
            });

            crate::runtime_async::sleep(std::time::Duration::from_millis(10)).await;

            // Spawn multiple concurrent clients
            let socket_path_clone = socket_path.clone();
            let handles: Vec<_> = (0..5)
                .map(|i| {
                    let path = socket_path_clone.clone();
                    crate::runtime_async::task::spawn(async move {
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
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
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
    fn rpc_required_scope_events_annotate_is_write() {
        let args = vec!["events".to_string(), "annotate".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
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
    fn rpc_required_scope_unknown_command_is_write() {
        let args = vec!["not-a-real-command".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Write);
    }

    #[test]
    fn rpc_required_scope_known_read_command_remains_read() {
        let args = vec!["events".to_string()];
        assert_eq!(rpc_required_scope(&args), IpcScope::Read);
    }

    #[test]
    fn rpc_scope_read_families_require_explicit_read_operations() {
        for (family, read_operations) in [
            ("agents", &["list", "running", "detect"][..]),
            ("cass", &["search", "view", "status"][..]),
            ("rules", &["list", "test", "show", "lint"][..]),
            ("workflow", &["list", "status"][..]),
            ("accounts", &["list"][..]),
            ("reservations", &["list"][..]),
            ("mission", &["state", "decisions"][..]),
            ("tx", &["plan", "show"][..]),
        ] {
            for operation in read_operations {
                let args = vec![family.to_string(), (*operation).to_string()];
                assert_eq!(rpc_required_scope(&args), IpcScope::Read, "{args:?}");
            }
            for tail in [
                vec![],
                vec!["future-mutation"],
                vec!["--verbose", "future-mutation"],
            ] {
                let args = std::iter::once(family)
                    .chain(tail)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                assert_eq!(rpc_required_scope(&args), IpcScope::Write, "{args:?}");
            }
        }
    }

    #[test]
    fn rpc_scope_event_queries_consume_only_known_read_options() {
        for family in ["events", "event-log", "eventlog"] {
            for tail in [
                vec![],
                vec!["--verbose", "-vv", "--unhandled", "--would-handle"],
                vec!["--limit", "10", "--pane=7", "--since", "-1"],
                vec![
                    "--label",
                    "annotate",
                    "--rule-id=triage",
                    "--event-type",
                    "label",
                ],
                vec!["--triage-state", "open", "--dry-run", "--unhandled-only"],
                vec![
                    "--cursor",
                    "7",
                    "--cursor-epoch=e",
                    "--cursor-scope",
                    "s",
                    "--replay-limit=2",
                ],
                vec!["--start-at-tail"],
            ] {
                let args = std::iter::once(family)
                    .chain(tail)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                assert_eq!(rpc_required_scope(&args), IpcScope::Read, "{args:?}");
            }
            for tail in [
                vec!["annotate", "7", "--note", "note"],
                vec!["-v", "annotate", "7", "--note", "note"],
                vec!["--verbose", "triage", "7", "--state", "closed"],
                vec!["-vv", "label", "7", "--add", "reviewed"],
                vec!["--future-mutation"],
                vec!["--limit"],
                vec!["--", "annotate", "7"],
            ] {
                let args = std::iter::once(family)
                    .chain(tail)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                assert_eq!(rpc_required_scope(&args), IpcScope::Write, "{args:?}");
            }
        }
    }

    #[test]
    fn rpc_scope_read_tokens_cannot_authorize_configuration_or_event_mutations() {
        let reader = build_auth("scope-test", vec![IpcScope::Read], None);
        let writer = build_auth("scope-test", vec![IpcScope::Write], None);
        for args in [
            vec!["agents", "configure", "--scope", "global"],
            vec!["agents", "configure", "--dry-run"],
            vec!["agents", "--verbose", "configure", "--all"],
            vec!["events", "-v", "annotate", "7", "--note", "note"],
            vec!["events", "--verbose", "triage", "7", "--state", "closed"],
            vec!["event-log", "-vv", "label", "7", "--add", "reviewed"],
        ] {
            let envelope = serde_json::to_vec(&serde_json::json!({
                "token": "scope-test",
                "type": "rpc",
                "args": args,
            }))
            .unwrap();
            assert_eq!(
                __fuzz_parse_envelope_and_authorize(&envelope, &reader),
                "scope",
                "{args:?}"
            );
            assert_eq!(
                __fuzz_parse_envelope_and_authorize(&envelope, &writer),
                "allowed",
                "{args:?}"
            );
        }
        for args in [
            vec!["agents", "list"],
            vec!["agents", "running"],
            vec!["agents", "detect", "--refresh"],
            vec!["events", "--label", "annotate"],
            vec!["events", "--verbose", "--limit=10"],
            vec!["mission", "state"],
            vec!["mission", "decisions"],
            vec!["tx", "plan"],
            vec!["tx", "show"],
        ] {
            let envelope = serde_json::to_vec(&serde_json::json!({
                "token": "scope-test",
                "type": "rpc",
                "args": args,
            }))
            .unwrap();
            assert_eq!(
                __fuzz_parse_envelope_and_authorize(&envelope, &reader),
                "allowed",
                "{args:?}"
            );
        }
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
    fn ipc_request_required_scope_rpc_unknown_defaults_to_write() {
        let req = IpcRequest::Rpc {
            args: vec!["unknown-op".to_string()],
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

    // ── [sec] constant-time token compare ───────────────────────────────
    //
    // Behavioural pins for `ipc_constant_time_eq` and the scan-every-candidate
    // loop inside `IpcAuth::authorize`. We cannot make a proper statistical
    // timing assertion in a unit test (CI noise dominates), so these tests
    // lock the *contract* — equal-length inputs return equal bytes with
    // accumulator-based comparison, and mismatched-prefix inputs still miss
    // — and leave the timing property as a review-level invariant of the
    // loop shape above.

    #[test]
    fn ipc_constant_time_eq_matches_on_equal_strings() {
        assert!(ipc_constant_time_eq(
            "correct-horse-battery-staple",
            "correct-horse-battery-staple"
        ));
    }

    #[test]
    fn ipc_constant_time_eq_rejects_first_byte_mismatch() {
        assert!(!ipc_constant_time_eq("secret", "Xecret"));
    }

    #[test]
    fn ipc_constant_time_eq_rejects_last_byte_mismatch() {
        assert!(!ipc_constant_time_eq("secret", "secreX"));
    }

    #[test]
    fn ipc_constant_time_eq_rejects_different_lengths() {
        assert!(!ipc_constant_time_eq("short", "longer-string"));
        assert!(!ipc_constant_time_eq("longer-string", "short"));
    }

    #[test]
    fn ipc_constant_time_eq_empty_strings_match() {
        assert!(ipc_constant_time_eq("", ""));
        assert!(!ipc_constant_time_eq("", "a"));
        assert!(!ipc_constant_time_eq("a", ""));
    }

    #[test]
    fn ipc_auth_accepts_match_anywhere_in_vec() {
        // Regression: with short-circuiting `.find()`, a token at the end of
        // the vector took measurably longer than one at the front. The fix
        // always scans the full vector, so a back-of-vec match is equally
        // fast — functional side-effect: either ordering still authorizes.
        let auth = IpcAuth::new(vec![
            IpcAuthToken {
                token: "decoy-1".to_string(),
                scopes: vec![IpcScope::All],
                expires_at_ms: None,
            },
            IpcAuthToken {
                token: "decoy-2".to_string(),
                scopes: vec![IpcScope::All],
                expires_at_ms: None,
            },
            IpcAuthToken {
                token: "real".to_string(),
                scopes: vec![IpcScope::All],
                expires_at_ms: None,
            },
        ]);
        assert!(auth.authorize(Some("real"), IpcScope::Read).is_ok());
    }

    #[test]
    fn ipc_auth_rejects_prefix_of_valid_token() {
        // `==` on `String` would still fail to authorize here, but the
        // timing *profile* leaked byte-prefix equality. This test
        // co-locates with the fix as a functional regression guard: an
        // attacker-supplied prefix of the real token must NOT authorize,
        // AND the loop must scan every candidate (timing invariant is
        // enforced by code review of the loop, not by measurement here).
        let auth = build_auth("correct-horse", vec![IpcScope::All], None);
        assert!(
            auth.authorize(Some("correct-hors"), IpcScope::Read)
                .is_err()
        );
        assert!(auth.authorize(Some("correct-hor"), IpcScope::Read).is_err());
        assert!(auth.authorize(Some("c"), IpcScope::Read).is_err());
        assert!(auth.authorize(Some(""), IpcScope::Read).is_err());
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
        let IpcRequest::UserVar {
            pane_id,
            name,
            value,
        } = envelope.request
        else {
            assert!(false, "Expected UserVar request");
            return;
        };
        assert_eq!(pane_id, 42);
        assert_eq!(name, "FT_EVENT");
        assert_eq!(value, "abc");
    }

    #[test]
    fn watcher_control_envelope_is_strict_and_write_scoped() {
        let json = r#"{"type":"watcher_control","control":{"action":"reload","expected_instance_id":"0123456789abcdef0123456789abcdef"}}"#;
        let envelope: IpcEnvelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.request.required_scope(), IpcScope::Write);
        let IpcRequest::WatcherControl { control } = envelope.request else {
            assert!(false, "expected watcher-control request");
            return;
        };
        assert_eq!(control.action, WatcherControlAction::Reload);
        assert_eq!(
            control.expected_instance_id,
            "0123456789abcdef0123456789abcdef"
        );

        let unknown_field = r#"{"type":"watcher_control","control":{"action":"stop","expected_instance_id":"0123456789abcdef0123456789abcdef","pid":42}}"#;
        assert!(serde_json::from_str::<IpcEnvelope>(unknown_field).is_err());

        let unknown_action = r#"{"type":"watcher_control","control":{"action":"kill","expected_instance_id":"0123456789abcdef0123456789abcdef"}}"#;
        assert!(serde_json::from_str::<IpcEnvelope>(unknown_action).is_err());
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
        assert!(ctx.watcher_control_handler.is_none());
        assert!(ctx.search_config.is_none());
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
    fn watcher_control_dispatch_is_typed_and_fails_closed_without_handler() {
        run_async_test(async {
            let event_bus = Arc::new(EventBus::new(10));
            let ctx = IpcHandlerContext::new(event_bus);
            let response = handle_request_with_context(
                &crate::cx::for_testing(),
                IpcEnvelope {
                    token: None,
                    request_id: Some("control-unavailable".to_string()),
                    request: IpcRequest::WatcherControl {
                        control: WatcherControlRequest {
                            action: WatcherControlAction::Stop,
                            expected_instance_id: "0123456789abcdef0123456789abcdef".to_string(),
                        },
                    },
                },
                &ctx,
                resolve_limits(None),
            )
            .await;
            assert!(!response.ok);
            assert_eq!(
                response.error_code.as_deref(),
                Some("ipc.watcher_control.unavailable")
            );
        });
    }

    #[test]
    fn watcher_control_dispatch_preserves_action_identity_and_request_id() {
        run_async_test(async {
            let seen = Arc::new(std::sync::Mutex::new(None));
            let seen_by_handler = Arc::clone(&seen);
            let handler: IpcWatcherControlHandler = Arc::new(move |invocation| {
                let seen_by_handler = Arc::clone(&seen_by_handler);
                Box::pin(async move {
                    {
                        let mut seen = seen_by_handler
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        *seen = Some((invocation.control.clone(), invocation.request_id.clone()));
                    }
                    IpcResponse::ok_with_data(serde_json::json!({
                        "accepted": true,
                        "instance_id": invocation.control.expected_instance_id,
                    }))
                })
            });
            let ctx = IpcHandlerContext::with_auth_rpc_control_and_search_config(
                Arc::new(EventBus::new(10)),
                None,
                None,
                None,
                Some(handler),
                None,
            );
            let control = WatcherControlRequest {
                action: WatcherControlAction::Reload,
                expected_instance_id: "fedcba9876543210fedcba9876543210".to_string(),
            };
            let response = handle_request_with_context(
                &crate::cx::for_testing(),
                IpcEnvelope {
                    token: None,
                    request_id: Some("control-42".to_string()),
                    request: IpcRequest::WatcherControl {
                        control: control.clone(),
                    },
                },
                &ctx,
                resolve_limits(None),
            )
            .await;
            assert!(response.ok);
            assert_eq!(
                *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
                Some((control, Some("control-42".to_string())))
            );
        });
    }

    #[test]
    fn cx_registry_handlers_report_pre_cancelled_access_as_error() {
        run_async_test(async {
            let event_bus = Arc::new(EventBus::new(10));
            let registry = Arc::new(RwLock::new(PaneRegistry::new()));
            let ctx = IpcHandlerContext::with_registry(event_bus, registry);
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("pre-cancel IPC pane-registry request"),
            );

            let responses = [
                handle_pane_state_with_cx(&cx, 7, &ctx).await,
                handle_set_pane_priority_with_cx(&cx, 7, 9, None, &ctx).await,
                handle_clear_pane_priority_with_cx(&cx, 7, &ctx).await,
                handle_request_with_context_with_cx(
                    &cx,
                    status_envelope(),
                    &ctx,
                    resolve_limits(None),
                )
                .await,
            ];

            for response in responses {
                assert!(!response.ok, "cancelled registry access cannot succeed");
                assert_eq!(
                    response.error_code.as_deref(),
                    Some("ipc.request_cancelled")
                );
                assert!(response.data.is_none());
            }
        });
    }

    fn status_envelope() -> IpcEnvelope {
        IpcEnvelope {
            token: None,
            request_id: Some("status-registry-wait".to_string()),
            request: IpcRequest::Status,
        }
    }

    #[test]
    fn status_registry_wait_ends_on_cancellation_and_server_shutdown() {
        run_async_test(async {
            for server_shutdown in [false, true] {
                let witness_cx = crate::cx::for_testing();
                let request_cx = crate::cx::for_testing();
                let registry = Arc::new(RwLock::new(PaneRegistry::new()));
                let ctx = IpcHandlerContext::with_registry(
                    Arc::new(EventBus::new(10)),
                    Arc::clone(&registry),
                );
                let writer = registry
                    .write_with_cx(&witness_cx)
                    .await
                    .expect("hold the registry write lock");
                let mut request = Box::pin(handle_request_with_context_with_cx(
                    &request_cx,
                    status_envelope(),
                    &ctx,
                    resolve_limits(None),
                ));

                // Establish that the real dispatcher reached the held lock
                // before sending either signal; no sleep decides admission.
                std::future::poll_fn(|task_cx| {
                    assert!(
                        std::future::Future::poll(request.as_mut(), task_cx).is_pending(),
                        "status must wait while the writer owns the registry"
                    );
                    std::task::Poll::Ready(())
                })
                .await;
                if server_shutdown {
                    ctx.connection_shutdown.request();
                } else {
                    request_cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("cancel blocked status request"),
                    );
                }

                let response = crate::runtime_async::timeout_with_cx(
                    &witness_cx,
                    Duration::from_secs(2),
                    request,
                )
                .await
                .expect("blocked status must end without releasing the writer");
                assert!(!response.ok);
                assert_eq!(
                    response.error_code.as_deref(),
                    Some("ipc.request_cancelled")
                );
                assert!(response.data.is_none());
                assert!(!witness_cx.is_cancel_requested());
                assert_eq!(request_cx.is_cancel_requested(), !server_shutdown);

                drop(writer);
                let recovered = crate::runtime_async::timeout_with_cx(
                    &witness_cx,
                    Duration::from_secs(2),
                    registry.write_with_cx(&witness_cx),
                )
                .await
                .expect("cancelled status must dequeue its registry waiter")
                .expect("the registry remains usable");
                drop(recovered);
            }
        });
    }

    #[test]
    fn status_registry_wait_respects_server_limit_and_earlier_cx_deadline() {
        run_async_test(async {
            for caller_deadline in [false, true] {
                let witness_cx = crate::cx::for_testing();
                let request_cx = if caller_deadline {
                    let deadline = crate::runtime_async::timer_now_with_cx(&witness_cx)
                        + Duration::from_millis(100);
                    crate::cx::Cx::for_testing_with_budget(
                        crate::cx::Budget::new().with_deadline(deadline),
                    )
                } else {
                    crate::cx::for_testing()
                };
                let registry = Arc::new(RwLock::new(PaneRegistry::new()));
                let ctx = IpcHandlerContext::with_registry(
                    Arc::new(EventBus::new(10)),
                    Arc::clone(&registry),
                );
                let writer = registry
                    .write_with_cx(&witness_cx)
                    .await
                    .expect("hold the registry write lock");
                let mut limits = resolve_limits(None);
                limits.io_timeout_ms = if caller_deadline { 30_000 } else { 100 };

                let response = crate::runtime_async::timeout_with_cx(
                    &witness_cx,
                    Duration::from_secs(2),
                    handle_request_with_context_with_cx(
                        &request_cx,
                        status_envelope(),
                        &ctx,
                        limits,
                    ),
                )
                .await
                .expect("status must finish at the earlier finite deadline");
                assert!(!response.ok);
                assert_eq!(
                    response.error_code.as_deref(),
                    Some("ipc.request_deadline_exceeded")
                );
                assert!(response.data.is_none());
                assert!(!witness_cx.is_cancel_requested());
                assert!(!ctx.connection_shutdown.is_requested());

                drop(writer);
                let recovered = crate::runtime_async::timeout_with_cx(
                    &witness_cx,
                    Duration::from_secs(2),
                    registry.write_with_cx(&witness_cx),
                )
                .await
                .expect("expired status must dequeue its registry waiter")
                .expect("the registry remains usable");
                drop(recovered);
            }
        });
    }

    #[test]
    fn status_registry_deadline_releases_live_server_connection_capacity() {
        run_async_test(async {
            let temp_dir = TempDir::new().unwrap();
            let socket_path = temp_dir.path().join("status-registry.sock");
            let witness_cx = crate::cx::for_testing();
            let server_cx = crate::cx::for_testing();
            let mut limits = resolve_limits(None);
            limits.max_concurrent_connections = 1;
            limits.io_timeout_ms = 1_000;
            let server = IpcServer::bind_with_permissions_and_limits_with_cx(
                &server_cx,
                &socket_path,
                Some(0o600),
                limits,
            )
            .await
            .expect("bind the status test server");
            let registry = Arc::new(RwLock::new(PaneRegistry::new()));
            let mut writer = registry
                .write_with_cx(&witness_cx)
                .await
                .expect("hold the registry throughout the blocked request");
            writer.discovery_tick(vec![make_pane_info(7)]);
            let server_registry = Arc::clone(&registry);
            let event_bus = Arc::new(EventBus::new(10));
            let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
            let run_cx = server_cx.clone();
            let server_handle = crate::runtime_async::task::spawn(async move {
                server
                    .run_with_registry_with_cx(&run_cx, event_bus, server_registry, shutdown_rx)
                    .await;
            });

            let client = IpcClient::new(&socket_path);
            let response = crate::runtime_async::timeout_with_cx(
                &witness_cx,
                Duration::from_secs(5),
                client.status(),
            )
            .await
            .expect("the live handler must enforce its configured registry deadline")
            .expect("the server must return a typed status error");
            assert_eq!(
                response.error_code.as_deref(),
                Some("ipc.request_deadline_exceeded")
            );
            assert!(!server_cx.is_cancel_requested());

            // The registry writer is still held. A successful Ping proves the
            // expired status released the server's only admission slot.
            let ping = crate::runtime_async::timeout_with_cx(
                &witness_cx,
                Duration::from_secs(2),
                client.ping(),
            )
            .await
            .expect("the only connection slot must be reusable")
            .expect("ping after status deadline");
            assert!(ping.ok);

            drop(writer);
            let recovered = crate::runtime_async::timeout_with_cx(
                &witness_cx,
                Duration::from_secs(2),
                client.status(),
            )
            .await
            .expect("status must recover after registry contention ends")
            .expect("read the recovered status response");
            assert!(recovered.ok);
            let data = recovered.data.expect("status payload");
            assert_eq!(data["pane_arenas"]["active_count"], 1);
            assert_eq!(data["pane_arenas"]["panes"][0]["pane_id"], 7);

            send_shutdown(&shutdown_tx).await;
            crate::runtime_async::timeout_with_cx(
                &witness_cx,
                Duration::from_secs(2),
                server_handle,
            )
            .await
            .expect("server must stop after bounded status handling")
            .expect("server task must complete normally");
        });
    }

    #[test]
    fn ipc_handler_context_with_auth_sets_auth() {
        let event_bus = Arc::new(EventBus::new(10));
        let auth = IpcAuth::new(vec![]);
        let ctx = IpcHandlerContext::with_auth(event_bus, None, Some(auth));
        assert!(ctx.auth.is_some());
        assert!(ctx.registry.is_none());
        assert!(ctx.search_config.is_none());
    }

    #[test]
    fn ipc_handler_context_with_auth_and_rpc() {
        let event_bus = Arc::new(EventBus::new(10));
        let handler: IpcRpcHandler = Arc::new(|_req| Box::pin(async { IpcResponse::ok() }));
        let ctx = IpcHandlerContext::with_auth_and_rpc(event_bus, None, None, Some(handler));
        assert!(ctx.rpc_handler.is_some());
        assert!(ctx.search_config.is_none());
    }

    #[test]
    fn ipc_handler_context_with_auth_rpc_and_search_config() {
        let event_bus = Arc::new(EventBus::new(10));
        let handler: IpcRpcHandler = Arc::new(|_req| Box::pin(async { IpcResponse::ok() }));
        let search_config = SearchConfig::default();
        let ctx = IpcHandlerContext::with_auth_rpc_and_search_config(
            event_bus,
            None,
            None,
            Some(handler),
            Some(search_config),
        );
        assert!(ctx.rpc_handler.is_some());
        assert!(ctx.search_config.is_some());
    }

    #[test]
    fn build_search_status_payload_with_config_includes_index_metrics() {
        let temp_dir = TempDir::new().unwrap();
        let mut search_config = SearchConfig {
            enabled: true,
            ..SearchConfig::default()
        };
        search_config.daemon.enabled = true;
        search_config.indexing.index_dir = temp_dir
            .path()
            .join("search-index")
            .to_string_lossy()
            .into_owned();

        let (search_index, search_health) = build_search_status_payload(Some(&search_config));
        let search_index = search_index
            .as_object()
            .expect("search_index should be an object when config is present");
        assert_eq!(
            search_index.get("document_count"),
            Some(&serde_json::json!(0))
        );
        assert_eq!(
            search_index.get("segment_count"),
            Some(&serde_json::json!(0))
        );
        assert_eq!(
            search_index.get("index_size_bytes"),
            Some(&serde_json::json!(0))
        );
        assert!(search_index.get("embedder_tiers_available").is_some());

        let search_health = search_health
            .as_object()
            .expect("search_health should be an object");
        assert_eq!(
            search_health.get("background_job_status"),
            Some(&serde_json::json!("idle"))
        );
        assert_eq!(
            search_health.get("indexing_error_count"),
            Some(&serde_json::json!(0))
        );
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
        assert_eq!(
            client.limits.io_timeout_ms,
            crate::tuning_config::IpcTuning::DEFAULT_IO_TIMEOUT_MS
        );
    }

    #[test]
    fn ipc_client_runtime_limits_are_validated_before_use() {
        let mut valid = resolve_limits(None);
        valid.io_timeout_ms = 45_000;
        let client = IpcClient::new("/tmp/test.sock")
            .try_with_runtime_limits(valid)
            .expect("valid finite runtime limits");
        assert_eq!(client.limits.io_timeout_ms, 45_000);

        let mut invalid = valid;
        invalid.io_timeout_ms = 0;
        let error = IpcClient::new("/tmp/test.sock")
            .try_with_runtime_limits(invalid)
            .err()
            .expect("zero I/O timeout must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
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

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for IPC primitives (wa-16hou)
    //
    // Exercises the Cx-aware channel primitives (mpsc_recv_state_with_cx,
    // shutdown_signal_pending_with_cx) and pure-data IPC types under
    // LabRuntime virtual time. Real-socket IPC tests (accept loop,
    // handler scope, multi-client concurrency) run through the existing
    // CompatRuntime test suite above; these LabRuntime tests add the
    // deterministic scheduling dimension for the three mpsc_recv_state
    // outcomes (Value, Disconnected, Cancelled) and the two
    // shutdown_signal_pending outcomes (true = shutdown, false = continue).
    // -------------------------------------------------------------------------

    mod labruntime_ipc {
        use super::*;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, mut handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::Quiescent
                ),
                "LabRuntime did not finish; termination: {:?}",
                report.termination,
            );
            let outcome = handle.try_join();
            assert!(
                matches!(outcome, Ok(Some(()))),
                "LabRuntime root task did not complete successfully: {outcome:?}"
            );
        }

        /// 1. mpsc_recv_state_with_cx returns Value when a message is
        ///    available under LabRuntime scheduling.
        #[test]
        fn mpsc_recv_state_value_under_labruntime() {
            run_lab(1001, || async move {
                let cx = asupersync::Cx::current().expect("lab Cx");
                let (tx, mut rx) = mpsc::channel::<()>(1);
                tx.reserve(&cx).await.expect("reserve").send(());
                let state = mpsc_recv_state_with_cx(&mut rx, &cx).await;
                assert_eq!(state, MpscRecvState::Value(()));
            });
        }

        /// 2. mpsc_recv_state_with_cx returns Disconnected when the sender
        ///    is dropped before any message is sent.
        #[test]
        fn mpsc_recv_state_disconnected_under_labruntime() {
            run_lab(1002, || async move {
                let cx = asupersync::Cx::current().expect("lab Cx");
                let (tx, mut rx) = mpsc::channel::<()>(1);
                drop(tx);
                let state = mpsc_recv_state_with_cx(&mut rx, &cx).await;
                assert_eq!(state, MpscRecvState::Disconnected);
            });
        }

        /// 3. mpsc_recv_state_with_cx returns Cancelled when the Cx is
        ///    pre-cancelled, even if the sender is still alive.
        #[test]
        fn mpsc_recv_state_cancelled_under_labruntime() {
            run_lab(1003, || async move {
                let (_tx, mut rx) = mpsc::channel::<()>(1);
                let budget = crate::cx::Budget::new().with_poll_quota(0);
                let cx = crate::cx::Cx::for_testing_with_budget(budget);
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("wa-16hou cancelled recv"),
                );
                let state = mpsc_recv_state_with_cx(&mut rx, &cx).await;
                assert_eq!(state, MpscRecvState::Cancelled);
            });
        }

        /// 4. shutdown_signal_pending_with_cx returns true when the
        ///    shutdown channel is closed (sender dropped).
        #[test]
        fn shutdown_pending_on_closed_channel_under_labruntime() {
            run_lab(1004, || async move {
                let cx = asupersync::Cx::current().expect("lab Cx");
                let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
                drop(shutdown_tx);
                assert!(shutdown_signal_pending_with_cx(&mut shutdown_rx, &cx).await);
            });
        }

        /// 5. shutdown_signal_pending_with_cx returns false when the Cx is
        ///    cancelled — the IPC server must NOT shut down just because
        ///    the Cx was cancelled. ProudSalmon fixed this behavior in
        ///    the 2026-04-01 support slice; this LabRuntime test pins it
        ///    under deterministic scheduling.
        #[test]
        fn shutdown_not_pending_on_cancelled_cx_under_labruntime() {
            run_lab(1005, || async move {
                let (_shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
                let budget = crate::cx::Budget::new().with_poll_quota(0);
                let cx = crate::cx::Cx::for_testing_with_budget(budget);
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("wa-16hou shutdown cancel"),
                );
                assert!(!shutdown_signal_pending_with_cx(&mut shutdown_rx, &cx).await);
            });
        }

        /// 6. IpcResponse JSON serde roundtrip is stable under LabRuntime.
        #[test]
        fn ipc_response_serde_roundtrip_under_labruntime() {
            run_lab(1006, || async move {
                let resp = IpcResponse::ok_with_data(serde_json::json!({"pane_id": 42}));
                let json = serde_json::to_string(&resp).expect("serialize");
                let restored: IpcResponse = serde_json::from_str(&json).expect("deserialize");
                assert!(restored.ok);
                assert_eq!(
                    restored.data.as_ref().and_then(|d| d.get("pane_id")),
                    Some(&serde_json::json!(42))
                );
            });
        }

        /// 7. IpcAuth token validation holds under LabRuntime: a valid
        ///    token with Write scope can read and write; an unknown token
        ///    is rejected.
        #[test]
        fn ipc_auth_token_validation_under_labruntime() {
            run_lab(1007, || async move {
                let auth = IpcAuth::new(vec![IpcAuthToken {
                    token: "lab-token".into(),
                    scopes: vec![IpcScope::Write],
                    expires_at_ms: None,
                }]);
                assert!(auth.authorize(Some("lab-token"), IpcScope::Read).is_ok());
                assert!(auth.authorize(Some("lab-token"), IpcScope::Write).is_ok());
                assert!(auth.authorize(Some("bad-token"), IpcScope::Read).is_err());
                assert!(auth.authorize(None, IpcScope::Read).is_err());
            });
        }

        /// 8. IpcRequest serde roundtrip: the Status variant survives
        ///    JSON encode/decode under LabRuntime. Pins the JSON-line
        ///    protocol contract the bead calls "must be preserved."
        #[test]
        fn ipc_request_serde_roundtrip_under_labruntime() {
            run_lab(1008, || async move {
                let req = IpcRequest::Status;
                let json = serde_json::to_string(&req).expect("serialize");
                let restored: IpcRequest = serde_json::from_str(&json).expect("deserialize");
                assert_eq!(format!("{restored:?}"), format!("{req:?}"));
            });
        }
    }
}
