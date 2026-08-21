//! Native event listener for vendored WezTerm integrations.
//!
//! Listens on a local socket for newline-delimited JSON events emitted by a
//! vendored WezTerm build (feature-gated on the WezTerm side). Unix uses the
//! existing Unix-domain-socket path; supported Unix targets authenticate both
//! endpoint ownership and peer credentials. Other targets fail closed until
//! they provide an equivalent peer-identity contract.

#![forbid(unsafe_code)]

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
#[cfg(any(unix, windows))]
use std::sync::atomic::AtomicU64;
#[cfg(any(unix, windows))]
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;

use crate::runtime_async::mpsc;
#[cfg(any(unix, windows))]
use crate::runtime_async::task::{JoinErrorKind, JoinSet, JoinSetSettlement};
#[cfg(any(unix, windows))]
use crate::runtime_async::{AcquireError, Semaphore, TryAcquireError};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
#[cfg(any(unix, windows))]
use socket_transport::{UnixListener, UnixStream};
#[cfg(any(unix, windows))]
use tracing::{debug, warn};

const MAX_EVENT_LINE_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
#[cfg(any(unix, windows))]
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(any(unix, windows))]
const ACCEPT_ERROR_INITIAL_BACKOFF: Duration = Duration::from_millis(25);
#[cfg(any(unix, windows))]
const ACCEPT_ERROR_MAX_BACKOFF: Duration = Duration::from_secs(1);
#[cfg(any(unix, windows))]
const MAX_CONCURRENT_NATIVE_CONNECTIONS: usize = 64;
const EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(25);
#[cfg(any(unix, windows))]
const NATIVE_CONNECTION_TASK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(unix)]
const NATIVE_SOCKET_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const NATIVE_SOCKET_MODE: u32 = 0o600;

// `std::io::ErrorKind` does not expose a portable NotSocket variant. Keep the
// symbolic raw code local to the target classes FrankenTerm actively supports
// for the native bridge and performance campaign.
#[cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
const UNIX_ENOTSOCK_RAW: i32 = 38;
#[cfg(all(
    unix,
    any(target_os = "linux", target_os = "android"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const UNIX_ENOTSOCK_RAW: i32 = 88;

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeConnectionTaskDrainOutcome {
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

#[cfg(any(unix, windows))]
fn classify_native_connection_task_drain(
    timed_out: bool,
    settlement: JoinSetSettlement,
) -> NativeConnectionTaskDrainOutcome {
    match settlement {
        JoinSetSettlement::Settled => NativeConnectionTaskDrainOutcome::Settled,
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } if timed_out => NativeConnectionTaskDrainOutcome::TimedOut {
            active_tasks,
            unacknowledged_tasks,
        },
        JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } => NativeConnectionTaskDrainOutcome::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        },
    }
}

#[cfg(any(unix, windows))]
fn native_listener_terminal_result(
    listener_error: Option<NativeEventError>,
    drain_outcome: NativeConnectionTaskDrainOutcome,
) -> Result<(), NativeEventError> {
    match drain_outcome {
        NativeConnectionTaskDrainOutcome::Settled => listener_error.map_or(Ok(()), Err),
        // Terminal-settlement failure takes precedence over the earlier loop
        // failure because the listener no longer owns proof that every admitted
        // connection task was destroyed. The earlier failure is logged at its
        // source before the bounded drain begins.
        NativeConnectionTaskDrainOutcome::TimedOut { .. } => {
            Err(NativeEventError::ConnectionTaskDrainTimedOut)
        }
        NativeConnectionTaskDrainOutcome::Incomplete { .. } => {
            Err(NativeEventError::ConnectionTaskDrainIncomplete)
        }
    }
}

#[cfg(any(unix, windows))]
fn native_context_io_error(operation: &'static str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        format!("native_event_context_interrupted:{operation}"),
    )
}

#[cfg(any(unix, windows))]
fn record_native_connection_anomaly(counter: &mut u64) -> u64 {
    *counter = (*counter).saturating_add(1);
    *counter
}

#[cfg(any(unix, windows))]
fn record_native_listener_anomaly(counter: &AtomicU64) -> u64 {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == u64::MAX {
            return current;
        }
        let next = current.saturating_add(1);
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => current = observed,
        }
    }
}

#[cfg(any(unix, windows))]
#[derive(Debug, Default)]
struct NativeListenerAnomalyCounters {
    oversized_line_drops: AtomicU64,
    backpressure_drops: AtomicU64,
    context_ended_drops: AtomicU64,
    channel_closed_drops: AtomicU64,
    malformed_event_drops: AtomicU64,
    connections_with_drops: AtomicU64,
    post_accept_io_failures: AtomicU64,
    #[cfg(unix)]
    rejected_peers: AtomicU64,
}

#[cfg(any(unix, windows))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NativeConnectionDropCounts {
    oversized_line_drops: u64,
    backpressure_drops: u64,
    context_ended_drops: u64,
    channel_closed_drops: u64,
    malformed_event_drops: u64,
}

#[cfg(any(unix, windows))]
fn record_native_context_ended_drop(
    connection_counter: &mut u64,
    listener_counters: &NativeListenerAnomalyCounters,
) -> (u64, u64) {
    (
        record_native_connection_anomaly(connection_counter),
        record_native_listener_anomaly(&listener_counters.context_ended_drops),
    )
}

#[cfg(any(unix, windows))]
fn record_native_channel_closed_drop(
    connection_counter: &mut u64,
    listener_counters: &NativeListenerAnomalyCounters,
) -> (u64, u64) {
    (
        record_native_connection_anomaly(connection_counter),
        record_native_listener_anomaly(&listener_counters.channel_closed_drops),
    )
}

#[cfg(any(unix, windows))]
fn record_native_connection_drop_summary(
    listener_counters: &NativeListenerAnomalyCounters,
    drops: &NativeConnectionDropCounts,
) -> Option<u64> {
    (drops.oversized_line_drops > 0
        || drops.backpressure_drops > 0
        || drops.context_ended_drops > 0
        || drops.channel_closed_drops > 0
        || drops.malformed_event_drops > 0)
        .then(|| record_native_listener_anomaly(&listener_counters.connections_with_drops))
}

#[cfg(any(unix, windows))]
fn finalize_native_connection(
    listener_counters: &NativeListenerAnomalyCounters,
    drops: &NativeConnectionDropCounts,
    result: Result<(), std::io::Error>,
) -> Result<(), std::io::Error> {
    if let Some(anomalous_connections) =
        record_native_connection_drop_summary(listener_counters, drops)
    {
        if anomalous_connections.is_power_of_two() {
            warn!(
                anomalous_connections,
                oversized_line_drops = drops.oversized_line_drops,
                backpressure_drops = drops.backpressure_drops,
                context_ended_drops = drops.context_ended_drops,
                channel_closed_drops = drops.channel_closed_drops,
                malformed_event_drops = drops.malformed_event_drops,
                "native event connections with bounded event drops reached sampled threshold"
            );
        }
    }

    debug!(
        io_error = result.is_err(),
        "native event connection closed (cx path)"
    );
    result
}

#[cfg(any(unix, windows))]
fn native_spawn_error_code_is_transient(code: &str) -> bool {
    // Asupersync publishes ASUP-E006 as the stable machine-readable identity
    // of RegionAtCapacity. All other spawn failures require runtime repair or
    // shutdown and must stop this listener rather than retrying blindly.
    code == "ASUP-E006"
}

#[cfg(any(unix, windows))]
fn native_spawn_error_may_be_shutdown_race(code: &str) -> bool {
    matches!(code, "ASUP-E001" | "ASUP-E002" | "ASUP-E003")
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeSpawnRetryState {
    consecutive_capacity_errors: u64,
    backoff: Duration,
}

#[cfg(any(unix, windows))]
impl NativeSpawnRetryState {
    const fn new() -> Self {
        Self {
            consecutive_capacity_errors: 0,
            backoff: ACCEPT_ERROR_INITIAL_BACKOFF,
        }
    }

    fn record_success(&mut self) {
        self.consecutive_capacity_errors = 0;
        self.backoff = ACCEPT_ERROR_INITIAL_BACKOFF;
    }
}

#[cfg(any(unix, windows))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeSpawnAdmissionDecision {
    RetryCapacity {
        consecutive_capacity_errors: u64,
        retry_backoff: Duration,
    },
    CleanShutdown,
    Fatal,
}

#[cfg(any(unix, windows))]
fn classify_native_spawn_failure(
    error_code: &str,
    shutdown_observed: bool,
    retry_state: &mut NativeSpawnRetryState,
) -> NativeSpawnAdmissionDecision {
    let capacity_error = native_spawn_error_code_is_transient(error_code);
    if shutdown_observed && (capacity_error || native_spawn_error_may_be_shutdown_race(error_code))
    {
        return NativeSpawnAdmissionDecision::CleanShutdown;
    }

    if !capacity_error {
        return NativeSpawnAdmissionDecision::Fatal;
    }

    retry_state.consecutive_capacity_errors =
        retry_state.consecutive_capacity_errors.saturating_add(1);
    let retry_backoff = retry_state.backoff;
    retry_state.backoff = retry_state
        .backoff
        .saturating_mul(2)
        .min(ACCEPT_ERROR_MAX_BACKOFF);
    NativeSpawnAdmissionDecision::RetryCapacity {
        consecutive_capacity_errors: retry_state.consecutive_capacity_errors,
        retry_backoff,
    }
}

#[cfg(any(unix, windows))]
fn native_accept_error_is_permanent(error: &std::io::Error) -> bool {
    if matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::InvalidData
            | std::io::ErrorKind::Unsupported
            | std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::BrokenPipe
    ) {
        return true;
    }

    // `ErrorKind` deliberately folds several terminal descriptor/socket
    // failures into `Other`. Retrying these cannot repair the listener and
    // would otherwise leave the native path in a permanent one-second retry
    // loop. Resource exhaustion errors remain transient and use the bounded
    // backoff below.
    #[cfg(unix)]
    if error.raw_os_error() == Some(9) {
        // POSIX EBADF on every supported Unix target.
        return true;
    }
    #[cfg(all(
        unix,
        any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            all(
                any(target_os = "linux", target_os = "android"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        )
    ))]
    if error.raw_os_error() == Some(UNIX_ENOTSOCK_RAW) {
        return true;
    }
    #[cfg(windows)]
    if matches!(error.raw_os_error(), Some(6 | 10_038)) {
        // ERROR_INVALID_HANDLE / WSAENOTSOCK.
        return true;
    }

    false
}

/// Finite security failure classes for the local native-event transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NativeEventSecurityError {
    #[error("native event socket path must be absolute")]
    RelativeSocketPath,
    #[error("native event socket path has no parent directory")]
    MissingSocketParent,
    #[error("native event socket parent is not a real directory")]
    ParentNotDirectory,
    #[error(
        "native event socket parent owner mismatch: expected uid {expected_uid}, got {actual_uid}"
    )]
    ParentOwnerMismatch { expected_uid: u32, actual_uid: u32 },
    #[error("native event socket parent mode must be 0700, got {actual_mode:#o}")]
    ParentModeMismatch { actual_mode: u32 },
    #[error("native event endpoint is not a Unix socket")]
    EndpointNotSocket,
    #[error("native event endpoint owner mismatch: expected uid {expected_uid}, got {actual_uid}")]
    EndpointOwnerMismatch { expected_uid: u32, actual_uid: u32 },
    #[error("native event endpoint mode must be 0600, got {actual_mode:#o}")]
    EndpointModeMismatch { actual_mode: u32 },
    #[error("native event socket identity changed before cleanup")]
    SocketIdentityChanged,
    #[error("native event peer credentials are unavailable on this platform or build")]
    PeerCredentialsUnavailable,
    #[error("native event peer uid mismatch: expected {expected_uid}, got {actual_uid}")]
    PeerUidMismatch { expected_uid: u32, actual_uid: u32 },
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeSocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl NativeSocketIdentity {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Capture-gap reason prefix emitted when a native pane-output frame is
/// truncated at the [`MAX_OUTPUT_BYTES`] decode bound (ft-wtd5g). The
/// recorder/replay layer parses this prefix to identify a native-output
/// truncation gap and recover the dropped byte count, so the format is a
/// contract — pinned by the `native_output_truncation_gap_reason` golden test.
pub const NATIVE_OUTPUT_TRUNCATION_GAP_PREFIX: &str = "native_output_truncated:dropped_bytes=";

/// Build the explicit capture-gap reason for a truncated native pane-output
/// frame, carrying the count of tail bytes dropped at the decode bound. The
/// consumer injects this as a [`crate::ingest::CapturedSegmentKind::Gap`] so
/// replay records the loss instead of treating the holed stream as complete.
#[must_use]
pub fn native_output_truncation_gap_reason(dropped_bytes: u64) -> String {
    format!("{NATIVE_OUTPUT_TRUNCATION_GAP_PREFIX}{dropped_bytes}")
}

#[cfg(any(unix, windows))]
mod socket_transport {
    // Only the `native-events-inline-tests` test modules consume these; an
    // unconditional `all(test, unix)` gate leaves the re-export unused (and
    // warning) in default `cargo test` builds where that feature is off.
    #[cfg(all(test, unix, feature = "native-events-inline-tests"))]
    pub use crate::runtime_async::unix::{AsyncWriteExt, connect};
    #[cfg(unix)]
    pub use crate::runtime_async::unix::{
        LineReader, UnixListener, UnixStream, buffered, lines_with_max_length,
    };

    #[cfg(unix)]
    pub async fn bind_with_cx<P: AsRef<std::path::Path>>(
        cx: &crate::cx::Cx,
        path: P,
    ) -> std::io::Result<UnixListener> {
        cx.checkpoint()
            .map_err(|_| super::native_context_io_error("listener_bind"))?;
        UnixListener::bind(path).await
    }

    #[cfg(unix)]
    pub async fn next_line_with_cx<T>(
        cx: &crate::cx::Cx,
        lines: &mut LineReader<T>,
    ) -> std::io::Result<Option<String>>
    where
        T: crate::runtime_async::unix::AsyncRead + Unpin,
    {
        let result = crate::runtime_async::unix::next_line_with_cx(cx, lines).await;
        cx.checkpoint()
            .map_err(|_| super::native_context_io_error("connection_read"))?;
        result.map_err(|error| {
            if error.kind() == std::io::ErrorKind::Interrupted {
                super::native_context_io_error("connection_read")
            } else {
                error
            }
        })
    }

    #[cfg(windows)]
    mod windows {
        use std::io;
        use std::time::Duration;

        #[cfg(test)]
        pub use asupersync::io::AsyncWriteExt;
        pub use asupersync::io::{AsyncRead, BufReader};
        pub use frankenterm_uds::UnixStream;

        pub struct UnixListener {
            inner: frankenterm_uds::UnixListener,
        }

        pub type LineReader<T> = asupersync::io::Lines<BufReader<T>>;

        impl UnixListener {
            pub async fn accept_with_cx(&self, cx: &crate::cx::Cx) -> io::Result<(UnixStream, ())> {
                loop {
                    cx.checkpoint()
                        .map_err(|_| super::super::native_context_io_error("accept"))?;
                    match self.inner.accept() {
                        Ok((stream, _addr)) => {
                            let nonblocking_result = stream.set_nonblocking(true);
                            cx.checkpoint()
                                .map_err(|_| super::super::native_context_io_error("accept"))?;
                            nonblocking_result?;
                            return Ok((stream, ()));
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            let sleep_result =
                                crate::runtime_async::sleep_with_cx(cx, Duration::from_millis(1))
                                    .await;
                            cx.checkpoint().map_err(|_| {
                                super::super::native_context_io_error("accept_wait")
                            })?;
                            sleep_result.map_err(|_| {
                                super::super::native_context_io_error("accept_wait")
                            })?;
                        }
                        Err(err) => {
                            cx.checkpoint()
                                .map_err(|_| super::super::native_context_io_error("accept"))?;
                            return Err(err);
                        }
                    }
                }
            }
        }

        #[must_use]
        pub fn buffered<T: AsyncRead>(stream: T) -> BufReader<T> {
            BufReader::new(stream)
        }

        /// Line reader with an explicit maximum line length; mirrors
        /// `runtime_async::unix::lines_with_max_length` (ft-kccj8).
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

            cx.checkpoint()
                .map_err(|_| super::super::native_context_io_error("connection_read"))?;
            let result: io::Result<Option<String>> = match lines.next().await {
                Some(Ok(line)) => Ok(Some(line)),
                Some(Err(err)) => Err(err),
                None => Ok(None),
            };
            cx.checkpoint()
                .map_err(|_| super::super::native_context_io_error("connection_read"))?;
            result
        }
    }

    #[cfg(windows)]
    pub use windows::*;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventDispatchOutcome {
    Sent,
    Backpressure,
    ContextEnded,
    Closed,
}

/// In-memory pane state snapshot received from the native event bridge.
#[derive(Debug, Clone)]
pub struct NativePaneState {
    pub title: String,
    pub rows: u16,
    pub cols: u16,
    pub is_alt_screen: bool,
    pub cursor_row: u32,
    pub cursor_col: u32,
}

/// Deserialized event from the frankenterm-gui native bridge socket.
#[derive(Debug, Clone)]
pub enum NativeEvent {
    PaneOutput {
        pane_id: u64,
        data: Vec<u8>,
        timestamp_ms: i64,
        /// Bytes silently dropped from this frame's tail when the decoded
        /// payload exceeded `MAX_OUTPUT_BYTES` (ft-wtd5g). `0` means the frame
        /// was carried whole. A non-zero value MUST be turned into an explicit
        /// capture gap by the consumer so replay records the loss instead of
        /// treating the truncated stream as complete.
        dropped_bytes: u64,
    },
    StateChange {
        pane_id: u64,
        state: NativePaneState,
        timestamp_ms: i64,
    },
    UserVarChanged {
        pane_id: u64,
        name: String,
        value: String,
        timestamp_ms: i64,
    },
    PaneCreated {
        pane_id: u64,
        domain: String,
        cwd: Option<String>,
        timestamp_ms: i64,
    },
    PaneDestroyed {
        pane_id: u64,
        timestamp_ms: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum NativeEventError {
    #[error("socket path is empty")]
    EmptySocketPath,
    #[error("native event operation requires an ambient capability context")]
    ContextUnavailable,
    #[error("socket path already exists: {0}")]
    SocketAlreadyExists(String),
    #[error(transparent)]
    Security(#[from] NativeEventSecurityError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The runtime rejected an accepted connection task. The underlying spawn
    /// error is deliberately not retained because it is executor-specific and
    /// may contain data outside the finite listener telemetry contract.
    #[error("native event connection task admission failed")]
    ConnectionTaskAdmissionFailed,
    /// The bounded shutdown drain elapsed before every connection task reached
    /// terminal settlement.
    #[error("native event connection task drain timed out")]
    ConnectionTaskDrainTimedOut,
    /// Task settlement stopped after a nonterminal observation failure.
    #[error("native event connection task drain ended without terminal settlement")]
    ConnectionTaskDrainIncomplete,
}

#[cfg(all(unix, feature = "native-wezterm"))]
fn native_effective_uid() -> u32 {
    nix::unistd::Uid::effective().as_raw()
}

#[cfg(all(unix, not(feature = "native-wezterm")))]
fn native_effective_uid() -> Result<u32, NativeEventError> {
    Err(NativeEventSecurityError::PeerCredentialsUnavailable.into())
}

#[cfg(all(unix, feature = "native-wezterm"))]
fn native_peer_effective_uid<F: AsFd>(stream: &F) -> Result<u32, NativeEventError> {
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    {
        nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::LocalPeerCred)
            .map(|credentials| credentials.uid())
            .map_err(|_| NativeEventSecurityError::PeerCredentialsUnavailable.into())
    }

    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        nix::sys::socket::getsockopt(stream, nix::sys::socket::sockopt::PeerCredentials)
            .map(|credentials| credentials.uid())
            .map_err(|_| NativeEventSecurityError::PeerCredentialsUnavailable.into())
    }

    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "android",
        target_os = "linux",
        target_os = "freebsd",
        target_os = "dragonfly"
    )))]
    {
        let _ = stream;
        Err(NativeEventSecurityError::PeerCredentialsUnavailable.into())
    }
}

/// Whether this target exposes a peer-credential socket option supported by
/// the native-event authentication implementation.
#[cfg(unix)]
const fn native_peer_credentials_supported() -> bool {
    cfg!(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly",
    ))
}

#[cfg(all(unix, not(feature = "native-wezterm")))]
fn native_peer_effective_uid<F: AsFd>(_stream: &F) -> Result<u32, NativeEventError> {
    Err(NativeEventSecurityError::PeerCredentialsUnavailable.into())
}

/// Require a connected native-event peer to run under this process's
/// effective user ID. Credential lookup failures are fail-closed.
///
/// # Errors
///
/// Returns a finite [`NativeEventSecurityError`] when peer credentials are
/// unavailable or the peer's effective user ID differs from this process.
#[cfg(unix)]
pub fn validate_native_event_peer<F: AsFd>(stream: &F) -> Result<(), NativeEventError> {
    #[cfg(feature = "native-wezterm")]
    let expected_uid = native_effective_uid();
    #[cfg(not(feature = "native-wezterm"))]
    let expected_uid = native_effective_uid()?;
    let actual_uid = native_peer_effective_uid(stream)?;
    validate_native_event_peer_uids(expected_uid, actual_uid)
}

#[cfg(unix)]
fn validate_native_event_peer_uids(
    expected_uid: u32,
    actual_uid: u32,
) -> Result<(), NativeEventError> {
    if actual_uid != expected_uid {
        return Err(NativeEventSecurityError::PeerUidMismatch {
            expected_uid,
            actual_uid,
        }
        .into());
    }
    Ok(())
}

#[cfg(unix)]
fn validate_native_socket_parent(
    socket_path: &Path,
    expected_uid: u32,
) -> Result<(), NativeEventError> {
    if !socket_path.is_absolute() {
        return Err(NativeEventSecurityError::RelativeSocketPath.into());
    }
    let parent = socket_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(NativeEventSecurityError::MissingSocketParent)?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(NativeEventSecurityError::ParentNotDirectory.into());
    }
    if metadata.uid() != expected_uid {
        return Err(NativeEventSecurityError::ParentOwnerMismatch {
            expected_uid,
            actual_uid: metadata.uid(),
        }
        .into());
    }
    let actual_mode = metadata.permissions().mode() & 0o7777;
    if actual_mode != NATIVE_SOCKET_DIRECTORY_MODE {
        return Err(NativeEventSecurityError::ParentModeMismatch { actual_mode }.into());
    }
    Ok(())
}

#[cfg(unix)]
fn prepare_native_socket_parent(
    socket_path: &Path,
    expected_uid: u32,
) -> Result<(), NativeEventError> {
    if !socket_path.is_absolute() {
        return Err(NativeEventSecurityError::RelativeSocketPath.into());
    }
    let parent = socket_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(NativeEventSecurityError::MissingSocketParent)?;
    match std::fs::symlink_metadata(parent) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(NATIVE_SOCKET_DIRECTORY_MODE);
            builder.create(parent)?;
        }
        Err(error) => return Err(error.into()),
    }
    validate_native_socket_parent(socket_path, expected_uid)
}

#[cfg(unix)]
fn native_socket_identity(
    metadata: &std::fs::Metadata,
    expected_uid: u32,
    require_private_mode: bool,
) -> Result<NativeSocketIdentity, NativeEventError> {
    if !metadata.file_type().is_socket() {
        return Err(NativeEventSecurityError::EndpointNotSocket.into());
    }
    if metadata.uid() != expected_uid {
        return Err(NativeEventSecurityError::EndpointOwnerMismatch {
            expected_uid,
            actual_uid: metadata.uid(),
        }
        .into());
    }
    if require_private_mode {
        let actual_mode = metadata.permissions().mode() & 0o7777;
        if actual_mode != NATIVE_SOCKET_MODE {
            return Err(NativeEventSecurityError::EndpointModeMismatch { actual_mode }.into());
        }
    }
    Ok(NativeSocketIdentity::from_metadata(metadata))
}

/// Validate the filesystem half of the GUI-to-listener trust boundary before
/// connection. The connected peer credential is validated separately after
/// connect, closing the endpoint-replacement race.
///
/// # Errors
///
/// Returns a finite [`NativeEventSecurityError`] when the path, private parent
/// directory, endpoint type, owner, or mode violates the native-event trust
/// contract. Filesystem lookup failures are returned as [`NativeEventError::Io`].
#[cfg(unix)]
pub fn validate_native_event_socket_endpoint(socket_path: &Path) -> Result<(), NativeEventError> {
    #[cfg(feature = "native-wezterm")]
    let expected_uid = native_effective_uid();
    #[cfg(not(feature = "native-wezterm"))]
    let expected_uid = native_effective_uid()?;
    validate_native_socket_parent(socket_path, expected_uid)?;
    let metadata = std::fs::symlink_metadata(socket_path)?;
    native_socket_identity(&metadata, expected_uid, true)?;
    Ok(())
}

#[cfg(unix)]
fn remove_native_socket_if_identity_matches(
    socket_path: &Path,
    expected_uid: u32,
    expected_identity: NativeSocketIdentity,
) -> Result<bool, NativeEventError> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let observed_identity = native_socket_identity(&metadata, expected_uid, false)?;
    if observed_identity != expected_identity {
        return Err(NativeEventSecurityError::SocketIdentityChanged.into());
    }
    std::fs::remove_file(socket_path)?;
    Ok(true)
}

/// Pane state snapshot sent over the native event wire protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WirePaneState {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub is_alt_screen: bool,
    #[serde(default)]
    pub cursor_row: u32,
    #[serde(default)]
    pub cursor_col: u32,
}

/// Wire-protocol event type for the native event bridge.
///
/// Emitted by frankenterm-gui and consumed by `NativeEventListener`.
/// Serialized as newline-delimited JSON with `{"type":"variant_name",...}` format.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    Hello {
        #[serde(default)]
        proto: Option<u32>,
        #[serde(default)]
        wezterm_version: Option<String>,
        #[serde(default)]
        ts: Option<u64>,
    },
    PaneOutput {
        pane_id: u64,
        data_b64: String,
        ts: u64,
    },
    StateChange {
        pane_id: u64,
        state: WirePaneState,
        ts: u64,
    },
    UserVar {
        pane_id: u64,
        name: String,
        value: String,
        ts: u64,
    },
    PaneCreated {
        pane_id: u64,
        domain: String,
        cwd: Option<String>,
        ts: u64,
    },
    PaneDestroyed {
        pane_id: u64,
        ts: u64,
    },
}

/// Local socket server that receives pane events from the frankenterm GUI process.
#[cfg(any(unix, windows))]
pub struct NativeEventListener {
    socket_path: PathBuf,
    listener: UnixListener,
    #[cfg(unix)]
    socket_identity: NativeSocketIdentity,
    #[cfg(unix)]
    effective_uid: u32,
}

#[cfg(any(unix, windows))]
impl NativeEventListener {
    /// Bind the authenticated native-event listener using the ambient
    /// capability context.
    ///
    /// # Errors
    ///
    /// Returns [`NativeEventError::ContextUnavailable`] when no ambient
    /// context exists, a finite security error when the endpoint contract is
    /// violated, or [`NativeEventError::Io`] for a filesystem/transport fault.
    pub async fn bind(socket_path: PathBuf) -> Result<Self, NativeEventError> {
        let cx = crate::cx::Cx::current().ok_or(NativeEventError::ContextUnavailable)?;
        Self::bind_with_cx(&cx, socket_path).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`bind`].
    ///
    /// Multi-seam checkpoint structure surrounds parent-directory validation,
    /// identity-pinned stale-socket cleanup, listener bind, and post-bind
    /// permission hardening. A caller cancelled mid-startup does not receive
    /// newly-created authority; cancellation surfaces as
    /// `NativeEventError::Io(Interrupted)`.
    ///
    /// # Errors
    ///
    /// Returns a finite security error when an authenticated local transport
    /// cannot be established, or [`NativeEventError::Io`] for cancellation and
    /// filesystem/transport faults.
    pub async fn bind_with_cx(
        cx: &crate::cx::Cx,
        socket_path: PathBuf,
    ) -> Result<Self, NativeEventError> {
        #[cfg(windows)]
        {
            let _ = (cx, socket_path);
            return Err(NativeEventSecurityError::PeerCredentialsUnavailable.into());
        }

        #[cfg(unix)]
        {
            return Self::bind_secure_unix_with_cx(cx, socket_path).await;
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = (cx, socket_path);
            Err(NativeEventSecurityError::PeerCredentialsUnavailable.into())
        }
    }

    #[cfg(unix)]
    async fn bind_secure_unix_with_cx(
        cx: &crate::cx::Cx,
        socket_path: PathBuf,
    ) -> Result<Self, NativeEventError> {
        let check = |stage: &str| -> Result<(), NativeEventError> {
            cx.checkpoint().map_err(|_| {
                NativeEventError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    format!("native_event_context_interrupted:bind_{stage}"),
                ))
            })
        };

        check("entry")?;

        // Do not create a socket on Unix targets where the accepted peer could
        // never be authenticated. Failing before parent validation/stale-path
        // cleanup also guarantees this unsupported-target result has no
        // filesystem side effects.
        if !native_peer_credentials_supported() {
            return Err(NativeEventSecurityError::PeerCredentialsUnavailable.into());
        }

        if socket_path.as_os_str().is_empty() {
            return Err(NativeEventError::EmptySocketPath);
        }

        #[cfg(feature = "native-wezterm")]
        let effective_uid = native_effective_uid();
        #[cfg(not(feature = "native-wezterm"))]
        let effective_uid = native_effective_uid()?;

        check("before_parent_directory_creation")?;
        prepare_native_socket_parent(&socket_path, effective_uid)?;
        check("after_parent_directory_creation")?;

        check("before_stale_socket_cleanup")?;
        maybe_cleanup_stale_socket(&socket_path, effective_uid)?;
        check("after_stale_socket_cleanup")?;

        check("before_listener_bind")?;
        let listener = socket_transport::bind_with_cx(cx, &socket_path).await?;
        let metadata = std::fs::symlink_metadata(&socket_path)?;
        let socket_identity = native_socket_identity(&metadata, effective_uid, false)?;
        let bound = Self {
            socket_path,
            listener,
            socket_identity,
            effective_uid,
        };
        std::fs::set_permissions(
            &bound.socket_path,
            std::fs::Permissions::from_mode(NATIVE_SOCKET_MODE),
        )?;
        let secured_metadata = std::fs::symlink_metadata(&bound.socket_path)?;
        let secured_identity =
            native_socket_identity(&secured_metadata, bound.effective_uid, true)?;
        if secured_identity != bound.socket_identity {
            return Err(NativeEventSecurityError::SocketIdentityChanged.into());
        }
        // If cancellation wins concurrently with listener creation, dropping
        // `bound` closes the listener and executes the socket-path cleanup
        // contract instead of returning newly-created authority to the caller.
        check("after_listener_bind")?;
        Ok(bound)
    }

    /// Run the listener using the ambient capability context.
    ///
    /// # Errors
    ///
    /// Returns [`NativeEventError::ContextUnavailable`] when no ambient
    /// context is installed. Otherwise propagates the finite listener failures
    /// documented by [`run_with_cx`](Self::run_with_cx).
    pub async fn run(
        self,
        event_tx: mpsc::Sender<NativeEvent>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Result<(), NativeEventError> {
        let cx = crate::cx::Cx::current().ok_or(NativeEventError::ContextUnavailable)?;
        self.run_with_cx(&cx, event_tx, shutdown_flag).await
    }

    /// Run the accept loop against the caller's asupersync capability
    /// context (ft-xbnl0.2.3 Cx-first entry point).
    ///
    /// Short-circuits before entering the loop if `cx` is already cancelled
    /// or budget-exhausted — an operator who has abandoned the watch should not
    /// bind subscribers or spawn per-connection tasks. While the loop
    /// runs each accept poll is bounded via
    /// [`crate::runtime_async::timeout_with_cx`]. The loop checks the caller's
    /// context before each accept and again after a successful accept, so it
    /// cannot spawn a connection task after observed cancellation. Direct
    /// mid-wait cancellation is observed on a subsequent poll; it is not by
    /// itself a wakeup source. Matches the
    /// Cx-first pattern landed by `EventWaiter::wait_with_cx`
    /// (event_stream.rs), `WorkflowRunner::handle_detection_with_cx`,
    /// and `SurvivalModel::run_cx`.
    ///
    /// [`run`](Self::run) is the ambient-context adapter and returns a typed
    /// [`NativeEventError::ContextUnavailable`] instead of silently disabling
    /// native events when no runtime capability context is installed.
    ///
    /// # Errors
    ///
    /// Returns a typed I/O error after a permanent accept failure, a finite
    /// admission error when the runtime rejects a connection task, or a finite
    /// drain error when admitted connection tasks cannot be terminally settled.
    /// The shared result contract also lets unsupported targets return
    /// [`NativeEventSecurityError::PeerCredentialsUnavailable`] instead of
    /// presenting a cfg-dependent API or silently claiming listener success.
    pub async fn run_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_tx: mpsc::Sender<NativeEvent>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Result<(), NativeEventError> {
        if cx.checkpoint().is_err() {
            debug!(
                path = %self.socket_path.display(),
                error_class = "native_event_context_interrupted",
                "native event run aborted before accept loop"
            );
            return Ok(());
        }

        let mut connection_tasks = JoinSet::new();
        let connection_permits = Arc::new(Semaphore::new(MAX_CONCURRENT_NATIVE_CONNECTIONS));
        let mut connection_capacity_waits = 0_u64;
        let mut accept_error_backoff = ACCEPT_ERROR_INITIAL_BACKOFF;
        let mut consecutive_accept_errors = 0_u64;
        let mut spawn_retry_state = NativeSpawnRetryState::new();
        let mut listener_error = None;
        let anomaly_counters = Arc::new(NativeListenerAnomalyCounters::default());

        loop {
            if shutdown_flag.load(Ordering::SeqCst) || cx.checkpoint().is_err() {
                break;
            }

            // Reserve task/memory capacity before accepting another client.
            // A connection may retain a bounded line buffer while it waits for
            // input, so accepting first would let a same-UID reconnect storm
            // allocate an unbounded number of tasks. Contention stays in the
            // kernel backlog and wakes promptly when a task releases a permit.
            let connection_permit = match Arc::clone(&connection_permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(TryAcquireError::NoPermits) => {
                    connection_capacity_waits = connection_capacity_waits.saturating_add(1);
                    if connection_capacity_waits.is_power_of_two() {
                        warn!(
                            connection_capacity_waits,
                            max_connections = MAX_CONCURRENT_NATIVE_CONNECTIONS,
                            "native event listener waiting for bounded connection capacity"
                        );
                    }
                    match crate::runtime_async::timeout_with_cx(
                        cx,
                        ACCEPT_POLL_INTERVAL,
                        Arc::clone(&connection_permits).acquire_owned_with_cx(cx),
                    )
                    .await
                    {
                        Ok(Ok(permit)) => permit,
                        Ok(Err(AcquireError::Closed)) => {
                            listener_error = Some(NativeEventError::ConnectionTaskAdmissionFailed);
                            break;
                        }
                        Ok(Err(_)) if cx.checkpoint().is_err() => break,
                        Ok(Err(_)) => {
                            listener_error = Some(NativeEventError::ConnectionTaskAdmissionFailed);
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                Err(TryAcquireError::Closed) => {
                    listener_error = Some(NativeEventError::ConnectionTaskAdmissionFailed);
                    break;
                }
            };

            match crate::runtime_async::timeout_with_cx(cx, ACCEPT_POLL_INTERVAL, async {
                #[cfg(unix)]
                {
                    self.listener.accept().await
                }
                #[cfg(windows)]
                {
                    self.listener.accept_with_cx(cx).await
                }
            })
            .await
            {
                Ok(Ok((stream, _addr))) => {
                    accept_error_backoff = ACCEPT_ERROR_INITIAL_BACKOFF;
                    consecutive_accept_errors = 0;
                    // The accept future can become ready in the same scheduler
                    // turn as shutdown/cancellation. Revalidate before handing
                    // the accepted stream to a new owned task.
                    if !native_connection_spawn_allowed(cx, &shutdown_flag) {
                        drop(stream);
                        break;
                    }
                    #[cfg(unix)]
                    if let Err(error) = validate_native_event_peer(stream.as_std()) {
                        let rejected_peers =
                            record_native_listener_anomaly(&anomaly_counters.rejected_peers);
                        if rejected_peers.is_power_of_two() {
                            warn!(
                                failure_class = ?error,
                                rejected_peers,
                                path = %self.socket_path.display(),
                                "rejected unauthenticated native event peers reached sampled threshold"
                            );
                        }
                        drop(stream);
                        continue;
                    }
                    let tx = event_tx.clone();
                    let path = self.socket_path.display().to_string();
                    let connection_anomalies = Arc::clone(&anomaly_counters);
                    match crate::runtime_async::task::try_spawn_with_cx(
                        cx,
                        move |child_cx| async move {
                            let _connection_permit = connection_permit;
                            if let Err(err) = handle_connection_with_cx(
                                child_cx,
                                stream,
                                tx,
                                &connection_anomalies,
                            )
                            .await
                            {
                                // Split by error kind: clean client disconnects
                                // exit the handler loop via `Ok(None)` from
                                // `next_line_with_cx`, so they never reach this
                                // arm — but `Interrupted` here means the cx was
                                // cancelled (shutdown / budget), which is
                                // expected noise. Every other kind is a real
                                // post-accept I/O fault that operators need to
                                // see in production, where `debug!` is routinely
                                // filtered. Promote those to `warn!` with
                                // structured context.
                                if err.kind() == std::io::ErrorKind::Interrupted {
                                    debug!(
                                        error_class = "native_event_context_interrupted",
                                        path = %path,
                                        "native event connection cancelled"
                                    );
                                } else {
                                    let failures = record_native_listener_anomaly(
                                        &connection_anomalies.post_accept_io_failures,
                                    );
                                    if failures.is_power_of_two() {
                                        warn!(
                                            failure_count = failures,
                                            error_kind = ?err.kind(),
                                            path = %path,
                                            "native event post-accept I/O failures reached sampled threshold"
                                        );
                                    }
                                }
                            }
                        },
                    ) {
                        Ok(handle) => {
                            spawn_retry_state.record_success();
                            connection_tasks.insert_handle(handle);
                        }
                        Err(error) => {
                            let shutdown_observed = shutdown_flag.load(Ordering::SeqCst)
                                || (native_spawn_error_may_be_shutdown_race(error.code())
                                    && cx.checkpoint().is_err());
                            match classify_native_spawn_failure(
                                error.code(),
                                shutdown_observed,
                                &mut spawn_retry_state,
                            ) {
                                NativeSpawnAdmissionDecision::RetryCapacity {
                                    consecutive_capacity_errors,
                                    retry_backoff,
                                } => {
                                    if consecutive_capacity_errors.is_power_of_two() {
                                        warn!(
                                            error_code = error.code(),
                                            consecutive_spawn_capacity_errors = consecutive_capacity_errors,
                                            retry_backoff_ms = retry_backoff.as_millis(),
                                            path = %self.socket_path.display(),
                                            "native event connection task region is at capacity; applying bounded retry backoff"
                                        );
                                    }
                                    if crate::runtime_async::sleep_with_cx(cx, retry_backoff)
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                NativeSpawnAdmissionDecision::CleanShutdown => {
                                    debug!(
                                        error_code = error.code(),
                                        "native event connection spawn lost the normal runtime-shutdown race"
                                    );
                                    break;
                                }
                                NativeSpawnAdmissionDecision::Fatal => {
                                    warn!(
                                        error_code = error.code(),
                                        path = %self.socket_path.display(),
                                        "native event connection task admission failed"
                                    );
                                    // Continuing would accept and drop an unbounded
                                    // stream of clients while task admission remains
                                    // unavailable. Fail the listener loop closed;
                                    // polling remains active and a runtime restart is
                                    // required to attempt a fresh listener.
                                    listener_error =
                                        Some(NativeEventError::ConnectionTaskAdmissionFailed);
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok(Err(err)) => {
                    if native_accept_error_is_permanent(&err) {
                        warn!(
                            error = %err,
                            error_kind = ?err.kind(),
                            raw_os_error = ?err.raw_os_error(),
                            path = %self.socket_path.display(),
                            "native event listener stopped after permanent accept failure"
                        );
                        listener_error = Some(NativeEventError::Io(err));
                        break;
                    }
                    consecutive_accept_errors = consecutive_accept_errors.saturating_add(1);
                    if consecutive_accept_errors.is_power_of_two() {
                        warn!(
                            error = %err,
                            error_kind = ?err.kind(),
                            raw_os_error = ?err.raw_os_error(),
                            consecutive_accept_errors,
                            retry_backoff_ms = accept_error_backoff.as_millis(),
                            path = %self.socket_path.display(),
                            "native event accept failed; applying bounded retry backoff"
                        );
                    }
                    if crate::runtime_async::sleep_with_cx(cx, accept_error_backoff)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    accept_error_backoff = accept_error_backoff
                        .saturating_mul(2)
                        .min(ACCEPT_ERROR_MAX_BACKOFF);
                }
                // `timeout_with_cx` returns Err on either the poll
                // interval elapsing OR the Cx being cancelled. Either
                // way we want to loop and re-evaluate shutdown /
                // cancellation on the next iteration — no need to
                // disambiguate here.
                Err(_) => {}
            }

            while let Some(join_result) = connection_tasks.try_join_next() {
                if let Err(err) = join_result {
                    // Split by error kind: cancellation during the
                    // steady-state accept loop is expected (cx-driven
                    // budget) and stays at `debug!`; a panic propagating
                    // out of `handle_connection_with_cx` is a real fault
                    // that must not sink below warn-level for operators.
                    if err.is_cancelled() {
                        debug!(error = %err, "native event connection task cancelled");
                    } else {
                        warn!(
                            error = %err,
                            path = %self.socket_path.display(),
                            "native event connection task failed"
                        );
                    }
                }
            }
        }

        // A shutdown flag is independent from the parent Cx. Explicitly abort
        // every per-connection task before draining acknowledgements so a
        // quiet client blocked in a line read cannot hang listener shutdown.
        connection_tasks.abort_all();
        let drain_cx = crate::cx::for_request();
        let drain_result = crate::runtime_async::timeout_with_cx(
            &drain_cx,
            NATIVE_CONNECTION_TASK_DRAIN_TIMEOUT,
            async {
                loop {
                    match connection_tasks.drain_next_with_cx(&drain_cx).await {
                        Ok(Some(join_result)) => {
                            if let Err(err) = join_result {
                                match err.kind() {
                                    JoinErrorKind::Aborted
                                    | JoinErrorKind::ContextCancelled => {
                                        debug!(
                                            failure_class = ?err.kind(),
                                            "native event connection task stopped during shutdown"
                                        );
                                    }
                                    _ => {
                                        warn!(
                                            failure_class = ?err.kind(),
                                            "native event connection task failure remained observable during trusted shutdown drain"
                                        );
                                    }
                                }
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
            warn!(
                failure_class = ?drain_error.kind(),
                "native event connection task drain context failed before terminal settlement"
            );
        }
        let drain_outcome = classify_native_connection_task_drain(
            drain_result.is_err(),
            connection_tasks.settlement(),
        );
        match drain_outcome {
            NativeConnectionTaskDrainOutcome::Settled => {}
            NativeConnectionTaskDrainOutcome::TimedOut {
                active_tasks,
                unacknowledged_tasks,
            } => {
                warn!(
                    event = "native_event_connection_task_drain_timeout",
                    active_tasks,
                    unacknowledged_tasks,
                    remaining_tasks = connection_tasks.len(),
                    orphan_risk = true,
                    "native event connection tasks missed bounded terminal settlement"
                );
            }
            NativeConnectionTaskDrainOutcome::Incomplete {
                active_tasks,
                unacknowledged_tasks,
            } => {
                warn!(
                    event = "native_event_connection_task_settlement_incomplete",
                    active_tasks,
                    unacknowledged_tasks,
                    remaining_tasks = connection_tasks.len(),
                    orphan_risk = true,
                    "native event connection task drain stopped after a nonterminal observation failure"
                );
            }
        }
        native_listener_terminal_result(listener_error, drain_outcome)
    }
}

/// Fail-closed listener surface for targets without either Unix-domain sockets
/// or the in-tree Windows transport. Keeping the type available lets
/// `native-wezterm` remain a coherent feature while making admission
/// impossible on unsupported targets.
#[cfg(not(any(unix, windows)))]
pub struct NativeEventListener {
    _private: (),
}

#[cfg(not(any(unix, windows)))]
impl NativeEventListener {
    /// Refuse listener creation on a target without authenticated transport.
    pub async fn bind(socket_path: PathBuf) -> Result<Self, NativeEventError> {
        let cx = crate::cx::Cx::current().ok_or(NativeEventError::ContextUnavailable)?;
        Self::bind_with_cx(&cx, socket_path).await
    }

    /// Refuse listener creation on a target without authenticated transport.
    pub async fn bind_with_cx(
        _cx: &crate::cx::Cx,
        _socket_path: PathBuf,
    ) -> Result<Self, NativeEventError> {
        Err(NativeEventSecurityError::PeerCredentialsUnavailable.into())
    }

    /// This value cannot be constructed through the public API.
    pub async fn run(
        self,
        event_tx: mpsc::Sender<NativeEvent>,
        shutdown_flag: Arc<AtomicBool>,
    ) -> Result<(), NativeEventError> {
        let cx = crate::cx::Cx::current().ok_or(NativeEventError::ContextUnavailable)?;
        self.run_with_cx(&cx, event_tx, shutdown_flag).await
    }

    /// This value cannot be constructed through the public API.
    pub async fn run_with_cx(
        self,
        _cx: &crate::cx::Cx,
        _event_tx: mpsc::Sender<NativeEvent>,
        _shutdown_flag: Arc<AtomicBool>,
    ) -> Result<(), NativeEventError> {
        let _ = self;
        Err(NativeEventSecurityError::PeerCredentialsUnavailable.into())
    }
}

#[cfg(any(unix, windows))]
fn native_connection_spawn_allowed(cx: &crate::cx::Cx, shutdown_flag: &AtomicBool) -> bool {
    !shutdown_flag.load(Ordering::SeqCst) && cx.checkpoint().is_ok()
}

#[cfg(any(unix, windows))]
impl Drop for NativeEventListener {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Err(error) = remove_native_socket_if_identity_matches(
            &self.socket_path,
            self.effective_uid,
            self.socket_identity,
        ) {
            warn!(
                error = %error,
                path = %self.socket_path.display(),
                "refused to remove replaced native event socket path on drop"
            );
        }
    }
}

#[cfg(unix)]
fn maybe_cleanup_stale_socket(
    socket_path: &Path,
    effective_uid: u32,
) -> Result<(), NativeEventError> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(NativeEventError::Io(err)),
    };
    let stale_identity = match native_socket_identity(&metadata, effective_uid, false) {
        Ok(identity) => identity,
        Err(NativeEventError::Security(NativeEventSecurityError::EndpointNotSocket)) => {
            return Err(NativeEventError::SocketAlreadyExists(
                socket_path.display().to_string(),
            ));
        }
        Err(error) => return Err(error),
    };

    match StdUnixStream::connect(socket_path) {
        Ok(stream) => {
            validate_native_event_peer(&stream)?;
            Err(NativeEventError::SocketAlreadyExists(
                socket_path.display().to_string(),
            ))
        }
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            if remove_native_socket_if_identity_matches(socket_path, effective_uid, stale_identity)?
            {
                debug!(
                    path = %socket_path.display(),
                    "removed identity-pinned stale native event socket before bind"
                );
            }
            Ok(())
        }
        Err(err) => Err(NativeEventError::Io(err)),
    }
}

/// ft-xbnl0.2.3 Cx-first sibling of [`handle_connection`].
///
/// Plugs the orphan-cx hole documented in tick 165's lesson:
/// `dispatch_event_with_timeout` (called indirectly from this
/// handler via `dispatch_event`) used `crate::cx::for_request()`
/// for its mpsc reserve, severing the cancellation chain from
/// `run_with_cx`'s parent cx. The cx-first variant routes the
/// dispatch through `dispatch_event_with_cx`, which threads the
/// caller's cx all the way into the `event_tx.reserve(&cx)` wait.
///
/// `next_line_with_cx` (tick 160) replaces the line-read so a
/// cancelled parent can also interrupt a slow client. The pre-
/// flight checkpoint gates the handler's first iteration.
#[cfg(any(unix, windows))]
async fn handle_connection_with_cx(
    cx: crate::cx::Cx,
    stream: UnixStream,
    event_tx: mpsc::Sender<NativeEvent>,
    listener_anomalies: &NativeListenerAnomalyCounters,
) -> Result<(), std::io::Error> {
    debug!("native event connection accepted (cx path)");
    let mut drops = NativeConnectionDropCounts::default();
    // Keep every fallible read-loop exit inside this result. The single
    // finalizer below must observe accumulated drops before returning the
    // original success or I/O error to the listener task.
    let connection_result: Result<(), std::io::Error> = async {
        cx.checkpoint()
            .map_err(|_| native_context_io_error("connection_pre_read"))?;
        // ft-kccj8: the default lines() cap is 64 KiB — far below
        // MAX_EVENT_LINE_BYTES (512 KiB) — so the warn-and-skip branch below
        // was unreachable and any 64 KiB..512 KiB event killed the whole
        // connection with InvalidData. Cap at 2× so the skip branch has a
        // real window; lines beyond that still hard-close the connection
        // (memory stays bounded at ~1 MiB).
        let mut lines = socket_transport::lines_with_max_length(
            socket_transport::buffered(stream),
            MAX_EVENT_LINE_BYTES.saturating_mul(2),
        );

        'connection: loop {
            let line = match socket_transport::next_line_with_cx(&cx, &mut lines).await? {
                Some(line) => line,
                None => break 'connection Ok(()),
            };
            if line.len() > MAX_EVENT_LINE_BYTES {
                let connection_drop_count =
                    record_native_connection_anomaly(&mut drops.oversized_line_drops);
                let listener_drop_count =
                    record_native_listener_anomaly(&listener_anomalies.oversized_line_drops);
                if listener_drop_count.is_power_of_two() {
                    warn!(
                        connection_drop_count,
                        listener_drop_count,
                        line_bytes = line.len(),
                        "native event oversized-line drops reached sampled threshold"
                    );
                }
                continue;
            }

            match decode_wire_event(&line) {
                Ok(Some(event)) => {
                    let (event_kind, pane_id) = event_metadata(&event);
                    match dispatch_event_with_cx(&cx, &event_tx, event).await {
                        EventDispatchOutcome::Sent => {
                            debug!(event_kind, pane_id, "native event dispatched (cx path)");
                        }
                        EventDispatchOutcome::Backpressure => {
                            // ft-wtd5g: a dropped event here is silent data loss
                            // (the read loop holds no capture-pipeline handle, so it
                            // cannot inject a per-pane gap from this layer). Promote
                            // to warn so the loss is at least operator-visible rather
                            // than sinking into a filtered debug line; full per-pane
                            // gap injection for this path is tracked as a follow-up.
                            let connection_drop_count =
                                record_native_connection_anomaly(&mut drops.backpressure_drops);
                            let listener_drop_count = record_native_listener_anomaly(
                                &listener_anomalies.backpressure_drops,
                            );
                            if listener_drop_count.is_power_of_two() {
                                warn!(
                                    connection_drop_count,
                                    listener_drop_count,
                                    event_kind,
                                    pane_id,
                                    "native event backpressure drops reached sampled threshold"
                                );
                            }
                        }
                        EventDispatchOutcome::ContextEnded => {
                            // The frame was already decoded, so this is a real
                            // dropped event even though it is not queue pressure.
                            // Retain a separate bounded counter and sampled warning
                            // rather than silently hiding it in shutdown control
                            // flow or polluting backpressure telemetry.
                            let (connection_drop_count, listener_drop_count) =
                                record_native_context_ended_drop(
                                    &mut drops.context_ended_drops,
                                    listener_anomalies,
                                );
                            if listener_drop_count.is_power_of_two() {
                                warn!(
                                    connection_drop_count,
                                    listener_drop_count,
                                    event_kind,
                                    pane_id,
                                    "native event context-ended drops reached sampled threshold"
                                );
                            }
                            break 'connection Ok(());
                        }
                        EventDispatchOutcome::Closed => {
                            // The receiver disappeared after this frame was
                            // decoded. Record the resulting loss separately
                            // from both queue pressure and capability expiry.
                            let (connection_drop_count, listener_drop_count) =
                                record_native_channel_closed_drop(
                                    &mut drops.channel_closed_drops,
                                    listener_anomalies,
                                );
                            if listener_drop_count.is_power_of_two() {
                                warn!(
                                    connection_drop_count,
                                    listener_drop_count,
                                    event_kind,
                                    pane_id,
                                    "native event channel-closed drops reached sampled threshold"
                                );
                            }
                            break 'connection Ok(());
                        }
                    }
                }
                Ok(None) => {
                    debug!(
                        event = "native_event_hello_received",
                        "native event protocol hello received"
                    );
                }
                Err(_) => {
                    // Promoted from debug! — a malformed wire event is a
                    // protocol-level anomaly (version skew, corruption, or
                    // a hostile client writing to the native-events socket)
                    // and must not sink silently into a debug-level log
                    // that operators routinely filter out.
                    let connection_drop_count =
                        record_native_connection_anomaly(&mut drops.malformed_event_drops);
                    let listener_drop_count =
                        record_native_listener_anomaly(&listener_anomalies.malformed_event_drops);
                    if listener_drop_count.is_power_of_two() {
                        warn!(
                            connection_drop_count,
                            listener_drop_count,
                            failure_class = "wire_decode_failure",
                            "native event malformed-frame drops reached sampled threshold"
                        );
                    }
                }
            }
        }
    }
    .await;

    finalize_native_connection(listener_anomalies, &drops, connection_result)
}

fn event_metadata(event: &NativeEvent) -> (&'static str, u64) {
    match event {
        NativeEvent::PaneOutput { pane_id, .. } => ("pane_output", *pane_id),
        NativeEvent::StateChange { pane_id, .. } => ("state_change", *pane_id),
        NativeEvent::UserVarChanged { pane_id, .. } => ("user_var", *pane_id),
        NativeEvent::PaneCreated { pane_id, .. } => ("pane_created", *pane_id),
        NativeEvent::PaneDestroyed { pane_id, .. } => ("pane_destroyed", *pane_id),
    }
}

/// ft-xbnl0.2.3 Cx-first sibling of [`dispatch_event`].
async fn dispatch_event_with_cx(
    cx: &crate::cx::Cx,
    event_tx: &mpsc::Sender<NativeEvent>,
    event: NativeEvent,
) -> EventDispatchOutcome {
    dispatch_event_with_timeout_with_cx(cx, event_tx, event, EVENT_SEND_TIMEOUT).await
}

#[cfg(all(test, feature = "native-events-inline-tests"))]
async fn dispatch_event_with_timeout(
    event_tx: &mpsc::Sender<NativeEvent>,
    event: NativeEvent,
    send_timeout: Duration,
) -> EventDispatchOutcome {
    let cx = crate::cx::for_testing();
    dispatch_event_with_timeout_with_cx(&cx, event_tx, event, send_timeout).await
}

/// Return whether the caller capability has ended through cancellation or a
/// finite budget. A timeout can be driven either by the explicit send limit or
/// by an earlier capability deadline; `timeout_with_cx_typed` intentionally
/// reports both as `Elapsed`, so the caller must inspect the Cx to preserve the
/// distinction. The budget snapshot catches exhaustion before a timer has
/// materialized it as a root cancellation cause.
fn native_dispatch_context_ended(cx: &crate::cx::Cx) -> bool {
    if cx.checkpoint().is_err() || cx.root_cancel_cause().is_some() {
        return true;
    }

    let budget = cx.budget_stats();
    (budget.deadline.at.is_some() && budget.deadline.remaining.is_none())
        || budget.polls.remaining == Some(0)
        || budget.cost.remaining == Some(0)
}

/// ft-xbnl0.2.3 Cx-first sibling of [`dispatch_event_with_timeout`].
///
/// Threads the caller's cx into the `event_tx.reserve(cx)` wait,
/// replacing the orphan `cx::for_request()` that severed the
/// cancellation chain from `run_with_cx`'s parent. The explicit Cx is checked
/// again after reserve, so a ready permit is never committed after an observed
/// cancellation. Prompt mid-wait cancellation still depends on the reserve or
/// timeout future being repolled; direct cancellation alone is not a wakeup.
async fn dispatch_event_with_timeout_with_cx(
    cx: &crate::cx::Cx,
    event_tx: &mpsc::Sender<NativeEvent>,
    event: NativeEvent,
    send_timeout: Duration,
) -> EventDispatchOutcome {
    match crate::runtime_async::timeout_with_cx_typed(cx, send_timeout, event_tx.reserve(cx)).await
    {
        Ok(Ok(permit)) => {
            // Reserving capacity is an await boundary. Do not commit an event
            // if cancellation/budget exhaustion won concurrently with the
            // permit becoming ready.
            if native_dispatch_context_ended(cx) {
                return EventDispatchOutcome::ContextEnded;
            }
            permit.send(event);
            EventDispatchOutcome::Sent
        }
        Ok(Err(mpsc::SendError::Disconnected(()))) => EventDispatchOutcome::Closed,
        Ok(Err(mpsc::SendError::Cancelled(()))) => EventDispatchOutcome::ContextEnded,
        Ok(Err(mpsc::SendError::Full(()))) => EventDispatchOutcome::Backpressure,
        Err(crate::runtime_async::TimeoutError::Elapsed) => {
            if native_dispatch_context_ended(cx) {
                EventDispatchOutcome::ContextEnded
            } else {
                EventDispatchOutcome::Backpressure
            }
        }
    }
}

/// Test- and fuzz-only re-export of [`decode_wire_event`] (ft-he0w7).
///
/// `decode_wire_event` itself is private to keep the wire-format
/// surface pinned to the runtime event loop. The
/// `frankenterm-fuzz` crate's `native_events_wire` target needs a
/// public entry point to drive the parser with libfuzzer-generated
/// inputs; this thin wrapper provides one without leaking the wire
/// format to other consumers. Gated behind `cfg(any(test, feature =
/// "fuzz"))` so production builds do not see the export.
#[cfg(any(test, feature = "fuzz"))]
pub fn decode_wire_event_for_fuzz(line: &str) -> Result<Option<NativeEvent>, String> {
    decode_wire_event(line)
}

fn decode_wire_event(line: &str) -> Result<Option<NativeEvent>, String> {
    let wire: WireEvent = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let ts = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);

    match wire {
        WireEvent::Hello { .. } => Ok(None),
        WireEvent::PaneOutput {
            pane_id,
            data_b64,
            ts: ts_ms,
        } => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data_b64.as_bytes())
                .map_err(|e| format!("invalid base64: {e}"))?;
            // ft-wtd5g: bound the per-frame payload for memory safety, but
            // record how many tail bytes we dropped so the consumer can inject
            // an explicit capture gap. Previously the tail was discarded
            // silently and replay recorded a holed stream as if complete.
            let dropped_bytes = decoded.len().saturating_sub(MAX_OUTPUT_BYTES) as u64;
            let bounded = if decoded.len() > MAX_OUTPUT_BYTES {
                decoded[..MAX_OUTPUT_BYTES].to_vec()
            } else {
                decoded
            };
            Ok(Some(NativeEvent::PaneOutput {
                pane_id,
                data: bounded,
                timestamp_ms: ts(ts_ms),
                dropped_bytes,
            }))
        }
        WireEvent::StateChange {
            pane_id,
            state,
            ts: ts_ms,
        } => Ok(Some(NativeEvent::StateChange {
            pane_id,
            state: NativePaneState {
                title: state.title,
                rows: state.rows,
                cols: state.cols,
                is_alt_screen: state.is_alt_screen,
                cursor_row: state.cursor_row,
                cursor_col: state.cursor_col,
            },
            timestamp_ms: ts(ts_ms),
        })),
        WireEvent::UserVar {
            pane_id,
            name,
            value,
            ts: ts_ms,
        } => Ok(Some(NativeEvent::UserVarChanged {
            pane_id,
            name,
            value,
            timestamp_ms: ts(ts_ms),
        })),
        WireEvent::PaneCreated {
            pane_id,
            domain,
            cwd,
            ts: ts_ms,
        } => Ok(Some(NativeEvent::PaneCreated {
            pane_id,
            domain,
            cwd,
            timestamp_ms: ts(ts_ms),
        })),
        WireEvent::PaneDestroyed { pane_id, ts: ts_ms } => Ok(Some(NativeEvent::PaneDestroyed {
            pane_id,
            timestamp_ms: ts(ts_ms),
        })),
    }
}

#[cfg(all(test, unix))]
mod native_socket_security_classifier_tests {
    use super::*;

    #[test]
    fn peer_uid_classifier_accepts_only_exact_effective_uid() {
        assert!(validate_native_event_peer_uids(501, 501).is_ok());
        assert!(matches!(
            validate_native_event_peer_uids(501, 502),
            Err(NativeEventError::Security(
                NativeEventSecurityError::PeerUidMismatch {
                    expected_uid: 501,
                    actual_uid: 502
                }
            ))
        ));
    }

    #[test]
    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    fn same_process_unix_pair_has_authenticated_peer_credentials() {
        let (first, second) = StdUnixStream::pair().expect("create Unix stream pair");
        validate_native_event_peer(&first).expect("first peer should match effective uid");
        validate_native_event_peer(&second).expect("second peer should match effective uid");
    }
}

#[cfg(all(test, any(unix, windows)))]
mod native_accept_error_classifier_tests {
    use super::*;

    #[test]
    fn permanent_accept_failures_stop_instead_of_hot_spinning() {
        for kind in [
            std::io::ErrorKind::InvalidInput,
            std::io::ErrorKind::InvalidData,
            std::io::ErrorKind::Unsupported,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotConnected,
            std::io::ErrorKind::BrokenPipe,
        ] {
            let error = std::io::Error::from(kind);
            assert!(native_accept_error_is_permanent(&error), "kind={kind:?}");
        }

        for kind in [
            std::io::ErrorKind::Interrupted,
            std::io::ErrorKind::WouldBlock,
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::Other,
        ] {
            let error = std::io::Error::from(kind);
            assert!(!native_accept_error_is_permanent(&error), "kind={kind:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn invalid_unix_listener_descriptor_is_classified_from_raw_code() {
        let error = std::io::Error::from_raw_os_error(9);
        assert!(native_accept_error_is_permanent(&error));

        #[cfg(any(
            target_vendor = "apple",
            target_os = "freebsd",
            target_os = "dragonfly",
            target_os = "netbsd",
            target_os = "openbsd",
            all(
                any(target_os = "linux", target_os = "android"),
                any(target_arch = "x86_64", target_arch = "aarch64")
            )
        ))]
        {
            let not_a_socket = std::io::Error::from_raw_os_error(UNIX_ENOTSOCK_RAW);
            assert!(
                native_accept_error_is_permanent(&not_a_socket),
                "ENOTSOCK cannot recover through accept retry"
            );
        }

        let descriptor_exhaustion = std::io::Error::from_raw_os_error(24);
        assert!(!native_accept_error_is_permanent(&descriptor_exhaustion));
    }

    #[test]
    #[cfg(windows)]
    fn invalid_windows_listener_handles_are_classified_from_raw_codes() {
        for raw_os_error in [6, 10_038] {
            let error = std::io::Error::from_raw_os_error(raw_os_error);
            assert!(
                native_accept_error_is_permanent(&error),
                "raw_os_error={raw_os_error}"
            );
        }

        let descriptor_exhaustion = std::io::Error::from_raw_os_error(10_024);
        assert!(!native_accept_error_is_permanent(&descriptor_exhaustion));
    }

    #[test]
    fn permanent_accept_error_survives_a_successful_connection_task_drain() {
        let result = native_listener_terminal_result(
            Some(NativeEventError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "synthetic permanent accept failure",
            ))),
            NativeConnectionTaskDrainOutcome::Settled,
        );

        assert!(matches!(
            result,
            Err(NativeEventError::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }

    #[test]
    fn task_admission_error_survives_a_successful_connection_task_drain() {
        let result = native_listener_terminal_result(
            Some(NativeEventError::ConnectionTaskAdmissionFailed),
            NativeConnectionTaskDrainOutcome::Settled,
        );

        assert!(matches!(
            result,
            Err(NativeEventError::ConnectionTaskAdmissionFailed)
        ));
        assert_eq!(
            NativeEventError::ConnectionTaskAdmissionFailed.to_string(),
            "native event connection task admission failed"
        );
    }

    #[test]
    fn incomplete_connection_task_drain_has_finite_terminal_precedence() {
        let timed_out = native_listener_terminal_result(
            None,
            NativeConnectionTaskDrainOutcome::TimedOut {
                active_tasks: 1,
                unacknowledged_tasks: 0,
            },
        );
        assert!(matches!(
            timed_out,
            Err(NativeEventError::ConnectionTaskDrainTimedOut)
        ));

        let incomplete = native_listener_terminal_result(
            Some(NativeEventError::Io(std::io::Error::other(
                "earlier listener failure must not hide orphan risk",
            ))),
            NativeConnectionTaskDrainOutcome::Incomplete {
                active_tasks: 0,
                unacknowledged_tasks: 1,
            },
        );
        assert!(matches!(
            incomplete,
            Err(NativeEventError::ConnectionTaskDrainIncomplete)
        ));
    }

    #[test]
    fn connection_anomaly_counter_saturates_for_long_lived_clients() {
        let mut counter = u64::MAX - 1;
        assert_eq!(record_native_connection_anomaly(&mut counter), u64::MAX);
        assert_eq!(record_native_connection_anomaly(&mut counter), u64::MAX);
    }

    #[test]
    fn listener_anomaly_counter_saturates_for_long_lived_listeners() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(record_native_listener_anomaly(&counter), u64::MAX);
        assert_eq!(record_native_listener_anomaly(&counter), u64::MAX);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    #[test]
    fn context_ended_drop_telemetry_stays_distinct_from_backpressure() {
        let counters = NativeListenerAnomalyCounters::default();
        let mut drops = NativeConnectionDropCounts::default();

        assert_eq!(
            record_native_context_ended_drop(&mut drops.context_ended_drops, &counters),
            (1, 1)
        );
        assert_eq!(drops.context_ended_drops, 1);
        assert_eq!(counters.context_ended_drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            counters.backpressure_drops.load(Ordering::Relaxed),
            0,
            "capability termination must not be counted as queue pressure"
        );
        assert_eq!(
            record_native_connection_drop_summary(
                &counters,
                &NativeConnectionDropCounts::default(),
            ),
            None,
            "a drop-free connection must not affect anomaly summaries"
        );
        assert_eq!(
            record_native_connection_drop_summary(&counters, &drops),
            Some(1),
            "a context-ended event loss must appear in the connection summary"
        );
        assert_eq!(counters.connections_with_drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn channel_closed_drop_telemetry_is_terminal_but_not_pressure_or_context() {
        let counters = NativeListenerAnomalyCounters::default();
        let mut drops = NativeConnectionDropCounts::default();

        assert_eq!(
            record_native_channel_closed_drop(&mut drops.channel_closed_drops, &counters),
            (1, 1)
        );
        assert_eq!(drops.channel_closed_drops, 1);
        assert_eq!(counters.channel_closed_drops.load(Ordering::Relaxed), 1);
        assert_eq!(counters.backpressure_drops.load(Ordering::Relaxed), 0);
        assert_eq!(counters.context_ended_drops.load(Ordering::Relaxed), 0);
        assert_eq!(
            record_native_connection_drop_summary(&counters, &drops),
            Some(1)
        );
        assert_eq!(counters.connections_with_drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn connection_error_finalization_preserves_error_and_summarizes_prior_drops_once() {
        let counters = NativeListenerAnomalyCounters::default();
        let mut drops = NativeConnectionDropCounts::default();
        assert_eq!(
            record_native_connection_anomaly(&mut drops.malformed_event_drops),
            1
        );
        assert_eq!(
            record_native_listener_anomaly(&counters.malformed_event_drops),
            1
        );

        let propagated = finalize_native_connection(
            &counters,
            &drops,
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "synthetic line-read failure",
            )),
        )
        .expect_err("line-read failure must remain an error after drop summarization");

        assert_eq!(propagated.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(propagated.to_string(), "synthetic line-read failure");
        assert_eq!(
            counters.connections_with_drops.load(Ordering::Relaxed),
            1,
            "the single finalization point must emit one connection summary"
        );
    }

    #[test]
    fn listener_anomaly_sampling_persists_across_connection_boundaries() {
        let counters = NativeListenerAnomalyCounters::default();
        let mut listener_counts = Vec::new();

        for _connection in 0..4 {
            // Each handler owns a fresh local counter. The listener counter is
            // deliberately shared across reconnects, so a reconnect storm
            // cannot turn every first malformed frame into a warning.
            let mut connection_count = 0;
            assert_eq!(record_native_connection_anomaly(&mut connection_count), 1);
            listener_counts.push(record_native_listener_anomaly(
                &counters.malformed_event_drops,
            ));
            assert_eq!(
                record_native_listener_anomaly(&counters.connections_with_drops),
                u64::try_from(listener_counts.len()).expect("four reconnects fit u64"),
            );
        }

        assert_eq!(listener_counts, [1, 2, 3, 4]);
        assert!(listener_counts[0].is_power_of_two());
        assert!(listener_counts[1].is_power_of_two());
        assert!(!listener_counts[2].is_power_of_two());
        assert!(listener_counts[3].is_power_of_two());
        assert_eq!(counters.malformed_event_drops.load(Ordering::Relaxed), 4);
        assert_eq!(counters.connections_with_drops.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn only_region_capacity_spawn_errors_are_retry_safe() {
        assert!(native_spawn_error_code_is_transient("ASUP-E006"));
        for permanent in [
            "ASUP-E001",
            "ASUP-E002",
            "ASUP-E003",
            "ASUP-E004",
            "ASUP-E005",
            "ASUP-E007",
            "ASUP-E008",
            "unknown",
        ] {
            assert!(!native_spawn_error_code_is_transient(permanent));
        }
    }

    #[test]
    fn runtime_lifecycle_spawn_errors_are_clean_only_after_shutdown_observation() {
        for lifecycle_code in ["ASUP-E001", "ASUP-E002", "ASUP-E003"] {
            assert!(native_spawn_error_may_be_shutdown_race(lifecycle_code));
            assert!(!native_spawn_error_code_is_transient(lifecycle_code));
        }
        for non_lifecycle_code in [
            "ASUP-E004",
            "ASUP-E005",
            "ASUP-E006",
            "ASUP-E007",
            "ASUP-E008",
            "unknown",
        ] {
            assert!(!native_spawn_error_may_be_shutdown_race(non_lifecycle_code));
        }
    }

    #[test]
    fn spawn_admission_decision_owns_retry_growth_reset_and_terminal_classes() {
        let mut retry_state = NativeSpawnRetryState::new();

        assert_eq!(
            classify_native_spawn_failure("ASUP-E006", false, &mut retry_state),
            NativeSpawnAdmissionDecision::RetryCapacity {
                consecutive_capacity_errors: 1,
                retry_backoff: ACCEPT_ERROR_INITIAL_BACKOFF,
            }
        );
        assert_eq!(
            classify_native_spawn_failure("ASUP-E006", false, &mut retry_state),
            NativeSpawnAdmissionDecision::RetryCapacity {
                consecutive_capacity_errors: 2,
                retry_backoff: ACCEPT_ERROR_INITIAL_BACKOFF.saturating_mul(2),
            }
        );

        for _ in 0..16 {
            let NativeSpawnAdmissionDecision::RetryCapacity { retry_backoff, .. } =
                classify_native_spawn_failure("ASUP-E006", false, &mut retry_state)
            else {
                panic!("RegionAtCapacity must remain retryable before shutdown");
            };
            assert!(retry_backoff <= ACCEPT_ERROR_MAX_BACKOFF);
        }
        assert_eq!(retry_state.backoff, ACCEPT_ERROR_MAX_BACKOFF);

        retry_state.record_success();
        assert_eq!(retry_state, NativeSpawnRetryState::new());
        assert_eq!(
            classify_native_spawn_failure("ASUP-E006", false, &mut retry_state),
            NativeSpawnAdmissionDecision::RetryCapacity {
                consecutive_capacity_errors: 1,
                retry_backoff: ACCEPT_ERROR_INITIAL_BACKOFF,
            },
            "one successful admission must reset count and backoff"
        );

        for lifecycle_code in ["ASUP-E001", "ASUP-E002", "ASUP-E003"] {
            assert_eq!(
                classify_native_spawn_failure(lifecycle_code, true, &mut retry_state),
                NativeSpawnAdmissionDecision::CleanShutdown,
                "lifecycle failure after shutdown observation must be clean: {lifecycle_code}",
            );
            assert_eq!(
                classify_native_spawn_failure(lifecycle_code, false, &mut retry_state),
                NativeSpawnAdmissionDecision::Fatal,
                "lifecycle failure while still running must fail closed: {lifecycle_code}",
            );
        }
        assert_eq!(
            classify_native_spawn_failure("ASUP-E006", true, &mut retry_state),
            NativeSpawnAdmissionDecision::CleanShutdown,
            "shutdown must not sleep through a concurrent capacity rejection"
        );

        for fatal_code in ["ASUP-E004", "ASUP-E005", "ASUP-E007", "unknown"] {
            assert_eq!(
                classify_native_spawn_failure(fatal_code, false, &mut retry_state),
                NativeSpawnAdmissionDecision::Fatal,
                "non-retryable admission error must stop the listener: {fatal_code}",
            );
        }
    }

    #[test]
    fn native_connection_admission_is_strictly_bounded() {
        let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_NATIVE_CONNECTIONS));
        let mut held = Vec::with_capacity(MAX_CONCURRENT_NATIVE_CONNECTIONS);
        for _ in 0..MAX_CONCURRENT_NATIVE_CONNECTIONS {
            held.push(
                Arc::clone(&permits)
                    .try_acquire_owned()
                    .expect("configured native connection permit"),
            );
        }
        assert!(matches!(
            Arc::clone(&permits).try_acquire_owned(),
            Err(TryAcquireError::NoPermits)
        ));
        drop(held.pop().expect("one held permit must be available"));
        assert!(Arc::clone(&permits).try_acquire_owned().is_ok());
    }
}

#[cfg(all(
    test,
    unix,
    not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))
))]
mod unsupported_unix_native_socket_tests {
    use super::*;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};

    #[test]
    fn listener_fails_before_creating_endpoint_without_peer_credentials() {
        assert!(!native_peer_credentials_supported());
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build unsupported-Unix native-event test runtime");
        runtime.block_on(async {
            let directory = tempfile::tempdir().expect("create private test directory");
            let socket_path = directory.path().join("unsupported-native-events.sock");
            let cx = crate::cx::for_testing();
            let result = NativeEventListener::bind_with_cx(&cx, socket_path.clone()).await;
            assert!(matches!(
                result,
                Err(NativeEventError::Security(
                    NativeEventSecurityError::PeerCredentialsUnavailable
                ))
            ));
            assert!(
                !socket_path.exists(),
                "unsupported target must fail before creating a socket"
            );
        });
    }
}

#[cfg(all(
    test,
    unix,
    feature = "native-events-inline-tests",
    any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly"
    )
))]
mod transport_roundtrip_tests {
    use super::socket_transport::{self as event_socket, AsyncWriteExt};
    use super::*;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for native event transport tests");
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

    async fn recv_next<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.ok()
    }

    fn fail(message: impl Into<String>) -> ! {
        std::panic::panic_any(message.into())
    }

    #[test]
    fn native_event_platform_transport_roundtrip() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join(format!(
                "native-platform-roundtrip-{}.sock",
                std::process::id()
            ));
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind native event listener");
            let (event_tx, mut event_rx) = mpsc::channel(4);
            let shutdown = Arc::new(AtomicBool::new(false));
            let handle =
                crate::runtime_async::task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            crate::runtime_async::sleep(Duration::from_millis(25)).await;

            let mut stream = event_socket::connect(socket_path.clone())
                .await
                .expect("connect native event transport");
            let payload = r#"{"type":"pane_output","pane_id":7,"data_b64":"aGV5","ts":42}"#;
            stream
                .write_all(format!("{payload}\n").as_bytes())
                .await
                .expect("write native event payload");
            stream.flush().await.expect("flush native event payload");
            drop(stream);

            let event = crate::runtime_async::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(event) = recv_next(&mut event_rx).await {
                        break event;
                    }
                    crate::runtime_async::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("native event transport roundtrip timed out");

            match event {
                NativeEvent::PaneOutput {
                    pane_id,
                    data,
                    timestamp_ms,
                    ..
                } => {
                    assert_eq!(pane_id, 7);
                    assert_eq!(data, b"hey");
                    assert_eq!(timestamp_ms, 42);
                }
                other => fail(format!("unexpected native event: {other:?}")),
            }

            shutdown.store(true, Ordering::SeqCst);
            crate::runtime_async::timeout(Duration::from_secs(2), handle)
                .await
                .expect("native event listener shutdown timed out")
                .expect("native event listener task failed")
                .expect("native event listener returned a terminal error");
            assert!(
                !socket_path.exists(),
                "native event transport path should be removed after shutdown"
            );
        });
    }
}

#[cfg(all(test, windows, feature = "native-events-inline-tests"))]
mod unauthenticated_platform_tests {
    use super::*;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};

    #[test]
    fn windows_native_event_listener_fails_closed_without_peer_credentials() {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("build native-event fail-closed test runtime");
        runtime.block_on(async {
            let result = NativeEventListener::bind(PathBuf::from("events.sock")).await;
            assert!(matches!(
                result,
                Err(NativeEventError::Security(
                    NativeEventSecurityError::PeerCredentialsUnavailable
                ))
            ));
        });
    }
}

#[cfg(all(
    test,
    unix,
    feature = "native-events-inline-tests",
    any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly"
    )
))]
mod tests {
    use super::socket_transport::{self as event_socket, AsyncWriteExt};
    use super::*;
    use crate::runtime_async::task;
    use crate::runtime_async::{CompatRuntime, RuntimeBuilder};
    use proptest::prelude::*;
    use std::sync::atomic::AtomicBool;

    fn fail(message: impl Into<String>) -> ! {
        std::panic::panic_any(message.into())
    }

    #[test]
    fn decode_pane_output_event() {
        let payload = r#"{"type":"pane_output","pane_id":1,"data_b64":"aGVsbG8=","ts":123}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput {
                pane_id,
                data,
                timestamp_ms,
                ..
            } => {
                assert_eq!(pane_id, 1);
                assert_eq!(data, b"hello");
                assert_eq!(timestamp_ms, 123);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_pane_output_under_bound_reports_zero_dropped() {
        let payload = r#"{"type":"pane_output","pane_id":1,"data_b64":"aGVsbG8=","ts":123}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput {
                data,
                dropped_bytes,
                ..
            } => {
                assert_eq!(data, b"hello");
                assert_eq!(dropped_bytes, 0, "in-bound frame must report no loss");
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_pane_output_over_bound_truncates_and_reports_dropped_bytes() {
        // ft-wtd5g: a single well-formed frame whose decoded payload exceeds
        // MAX_OUTPUT_BYTES must be bounded AND report the dropped tail so the
        // consumer can inject an explicit capture gap (no more silent loss).
        let raw = vec![b'a'; MAX_OUTPUT_BYTES + 4096];
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let payload =
            format!(r#"{{"type":"pane_output","pane_id":9,"data_b64":"{data_b64}","ts":7}}"#);
        // The base64 line stays under MAX_EVENT_LINE_BYTES so it is NOT dropped
        // by the read-loop guard; the truncation path is what we exercise.
        assert!(payload.len() < MAX_EVENT_LINE_BYTES);
        let event = decode_wire_event(&payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput {
                data,
                dropped_bytes,
                ..
            } => {
                assert_eq!(data.len(), MAX_OUTPUT_BYTES, "payload must be bounded");
                assert_eq!(
                    dropped_bytes, 4096,
                    "dropped tail must be reported for explicit-gap injection"
                );
            }
            _ => fail("wrong event type"),
        }
    }

    // ── ft-gaudf: golden + property tests for the gap-injection contract ──

    /// Decode a synthetic `pane_output` frame carrying `n` filler bytes.
    fn decode_output_of_len(n: usize) -> NativeEvent {
        let raw = vec![b'a'; n];
        let data_b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let payload =
            format!(r#"{{"type":"pane_output","pane_id":5,"data_b64":"{data_b64}","ts":1}}"#);
        decode_wire_event(&payload)
            .expect("decode must not error")
            .expect("pane_output event must be present")
    }

    #[test]
    fn decode_output_accounts_every_byte_no_silent_drop() {
        // PROPERTY (boundary table): no original byte is silently lost — it is
        // either retained in `data` or counted in `dropped_bytes`, so
        // data.len() + dropped_bytes == original_len for every size class around
        // the decode bound.
        for n in [
            0usize,
            1,
            100,
            MAX_OUTPUT_BYTES - 1,
            MAX_OUTPUT_BYTES,
            MAX_OUTPUT_BYTES + 1,
            MAX_OUTPUT_BYTES + 4096,
            MAX_OUTPUT_BYTES * 2 + 7,
        ] {
            match decode_output_of_len(n) {
                NativeEvent::PaneOutput {
                    data,
                    dropped_bytes,
                    ..
                } => {
                    assert_eq!(
                        data.len() as u64 + dropped_bytes,
                        n as u64,
                        "byte accounting broke (silent loss) at n={n}"
                    );
                    assert!(
                        data.len() <= MAX_OUTPUT_BYTES,
                        "payload must stay within the decode bound at n={n}"
                    );
                    assert_eq!(
                        dropped_bytes > 0,
                        n > MAX_OUTPUT_BYTES,
                        "dropped flag must fire iff the frame was truncated at n={n}"
                    );
                }
                _ => fail(format!("expected PaneOutput at n={n}")),
            }
        }
    }

    #[test]
    fn truncated_decode_drives_recoverable_gap_marker() {
        // End-to-end (unit): a truncated frame's dropped count feeds the explicit
        // gap marker the consumer injects, and the recorder can recover the count.
        let n = MAX_OUTPUT_BYTES + 1234;
        let event = decode_output_of_len(n);
        let NativeEvent::PaneOutput { dropped_bytes, .. } = event else {
            fail("expected PaneOutput");
        };
        assert_eq!(dropped_bytes, 1234);
        let marker = native_output_truncation_gap_reason(dropped_bytes);
        let recovered = marker
            .strip_prefix(NATIVE_OUTPUT_TRUNCATION_GAP_PREFIX)
            .and_then(|s| s.parse::<u64>().ok());
        assert_eq!(
            recovered,
            Some(1234),
            "gap marker must carry the exact dropped count for replay"
        );
    }

    #[test]
    fn native_output_truncation_gap_reason_golden_and_round_trips() {
        // GOLDEN: the marker format is a recorder/replay contract — pin it
        // exactly, and prove the dropped count round-trips back out.
        assert_eq!(
            native_output_truncation_gap_reason(4096),
            "native_output_truncated:dropped_bytes=4096"
        );
        assert_eq!(
            native_output_truncation_gap_reason(0),
            "native_output_truncated:dropped_bytes=0"
        );
        for dropped in [0u64, 1, 4096, 65_536, u64::MAX] {
            let reason = native_output_truncation_gap_reason(dropped);
            assert!(
                reason.starts_with(NATIVE_OUTPUT_TRUNCATION_GAP_PREFIX),
                "marker must carry the contract prefix: {reason}"
            );
            let parsed = reason
                .strip_prefix(NATIVE_OUTPUT_TRUNCATION_GAP_PREFIX)
                .and_then(|s| s.parse::<u64>().ok());
            assert_eq!(
                parsed,
                Some(dropped),
                "marker must round-trip the dropped count"
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(40))]

        /// PROPERTY (randomized): the no-silent-drop accounting invariant holds
        /// for arbitrary payload sizes spanning the decode bound.
        #[test]
        fn decode_output_byte_accounting_holds_for_any_size(
            n in 0usize..(MAX_OUTPUT_BYTES + 4096),
        ) {
            if let NativeEvent::PaneOutput { data, dropped_bytes, .. } = decode_output_of_len(n) {
                prop_assert_eq!(data.len() as u64 + dropped_bytes, n as u64);
                prop_assert!(data.len() <= MAX_OUTPUT_BYTES);
                prop_assert_eq!(dropped_bytes > 0, n > MAX_OUTPUT_BYTES);
            } else {
                prop_assert!(false, "expected PaneOutput");
            }
        }
    }

    #[test]
    fn decode_state_change_event() {
        let payload = r#"{"type":"state_change","pane_id":2,"state":{"title":"zsh","rows":24,"cols":80,"is_alt_screen":false,"cursor_row":1,"cursor_col":2},"ts":456}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::StateChange {
                pane_id,
                state,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 2);
                assert_eq!(state.title, "zsh");
                assert_eq!(state.rows, 24);
                assert_eq!(state.cols, 80);
                assert!(!state.is_alt_screen);
                assert_eq!(state.cursor_row, 1);
                assert_eq!(state.cursor_col, 2);
                assert_eq!(timestamp_ms, 456);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_user_var_event() {
        let payload = r#"{"type":"user_var","pane_id":3,"name":"FT_EVENT","value":"abc","ts":789}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::UserVarChanged {
                pane_id,
                name,
                value,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 3);
                assert_eq!(name, "FT_EVENT");
                assert_eq!(value, "abc");
                assert_eq!(timestamp_ms, 789);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_hello_is_ignored() {
        let payload = r#"{"type":"hello","proto":1,"wezterm_version":"2026.01.30","ts":1}"#;
        let event = decode_wire_event(payload).unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn decode_hello_minimal_is_ignored() {
        let payload = r#"{"type":"hello"}"#;
        let event = decode_wire_event(payload).unwrap();
        assert!(event.is_none());
    }

    #[test]
    fn decode_pane_created_event() {
        let payload =
            r#"{"type":"pane_created","pane_id":10,"domain":"local","cwd":"/home/user","ts":555}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneCreated {
                pane_id,
                domain,
                cwd,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 10);
                assert_eq!(domain, "local");
                assert_eq!(cwd, Some("/home/user".to_string()));
                assert_eq!(timestamp_ms, 555);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_pane_created_without_cwd() {
        let payload = r#"{"type":"pane_created","pane_id":11,"domain":"remote","ts":600}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneCreated { cwd, .. } => {
                assert!(cwd.is_none());
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_pane_destroyed_event() {
        let payload = r#"{"type":"pane_destroyed","pane_id":99,"ts":777}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneDestroyed {
                pane_id,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 99);
                assert_eq!(timestamp_ms, 777);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_invalid_json_returns_error() {
        let result = decode_wire_event("not json at all");
        assert!(result.is_err());
    }

    #[test]
    fn decode_unknown_type_returns_error() {
        let payload = r#"{"type":"unknown_thing","pane_id":1,"ts":1}"#;
        let result = decode_wire_event(payload);
        assert!(result.is_err());
    }

    #[test]
    fn decode_invalid_base64_returns_error() {
        let payload = r#"{"type":"pane_output","pane_id":1,"data_b64":"!!!invalid!!!","ts":1}"#;
        let result = decode_wire_event(payload);
        assert!(result.is_err());
        let err_msg = result.unwrap_err();
        assert!(
            err_msg.contains("base64"),
            "expected base64 error, got: {err_msg}"
        );
    }

    #[test]
    fn decode_pane_output_truncates_large_data() {
        // Create base64 data that decodes to > MAX_OUTPUT_BYTES (64KB)
        let large_data = vec![b'A'; MAX_OUTPUT_BYTES + 1000];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&large_data);
        let payload = format!(
            r#"{{"type":"pane_output","pane_id":1,"data_b64":"{}","ts":1}}"#,
            encoded
        );
        let event = decode_wire_event(&payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput { data, .. } => {
                assert_eq!(data.len(), MAX_OUTPUT_BYTES);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_pane_output_preserves_small_data() {
        let small_data = vec![b'B'; 100];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&small_data);
        let payload = format!(
            r#"{{"type":"pane_output","pane_id":1,"data_b64":"{}","ts":1}}"#,
            encoded
        );
        let event = decode_wire_event(&payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput { data, .. } => {
                assert_eq!(data.len(), 100);
                assert!(data.iter().all(|&b| b == b'B'));
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_timestamp_overflow_clamps_to_i64_max() {
        let payload = format!(
            r#"{{"type":"pane_destroyed","pane_id":1,"ts":{}}}"#,
            u64::MAX
        );
        let event = decode_wire_event(&payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneDestroyed { timestamp_ms, .. } => {
                assert_eq!(timestamp_ms, i64::MAX);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_state_change_with_defaults() {
        // All state fields are `serde(default)` so missing fields should produce zeros/defaults
        let payload = r#"{"type":"state_change","pane_id":5,"state":{},"ts":100}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::StateChange { state, .. } => {
                assert_eq!(state.title, "");
                assert_eq!(state.rows, 0);
                assert_eq!(state.cols, 0);
                assert!(!state.is_alt_screen);
                assert_eq!(state.cursor_row, 0);
                assert_eq!(state.cursor_col, 0);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_state_change_alt_screen_true() {
        let payload = r#"{"type":"state_change","pane_id":6,"state":{"is_alt_screen":true,"title":"vim","rows":40,"cols":120,"cursor_row":10,"cursor_col":5},"ts":200}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::StateChange { state, .. } => {
                assert!(state.is_alt_screen);
                assert_eq!(state.title, "vim");
                assert_eq!(state.rows, 40);
                assert_eq!(state.cols, 120);
            }
            _ => fail("wrong event type"),
        }
    }

    #[test]
    fn decode_empty_string_is_error() {
        assert!(decode_wire_event("").is_err());
    }

    #[test]
    fn decode_pane_output_empty_base64() {
        let payload = r#"{"type":"pane_output","pane_id":1,"data_b64":"","ts":1}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput { data, .. } => {
                assert!(data.is_empty());
            }
            _ => fail("wrong event type"),
        }
    }

    // ── NativeEventError ───────────────────────────────────────────

    #[test]
    fn error_display_empty_socket_path() {
        let err = NativeEventError::EmptySocketPath;
        assert_eq!(err.to_string(), "socket path is empty");
    }

    #[test]
    fn error_display_socket_already_exists() {
        let err = NativeEventError::SocketAlreadyExists("/tmp/test.sock".into());
        assert!(err.to_string().contains("/tmp/test.sock"));
    }

    #[test]
    fn error_display_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = NativeEventError::Io(io_err);
        assert!(err.to_string().contains("denied"));
    }

    /// LabRuntime-based deterministic test (wa-k0tk5): prove that the
    /// native-events dispatch path runs under seed-locked virtual-time
    /// scheduling without wall clock dependence. This is the finish-line
    /// evidence that the module is cleanly on asupersync — if the dispatch
    /// ever re-acquires a tokio-shaped assumption, LabRuntime will either
    /// deadlock or step-explode and fail the test.
    ///
    /// br-ft-c8x87 migration: replaces ~30 lines of LabRuntime
    /// boilerplate with the lab_runtime fixture's
    /// `lab_runtime_test_with_seed`. The fixture's `LabReport` now
    /// surfaces `oracles_passed` so the oracle assertion still runs.
    #[test]
    fn dispatch_event_runs_deterministically_under_labruntime() {
        use crate::test_fixtures::lab_runtime::{
            assert_ran_to_completion, lab_runtime_test_with_seed,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const SEED: u64 = 0xF734_5E42_A11D_0515;
        const EVENT_COUNT: usize = 8;

        let wall_start = std::time::Instant::now();
        let delivered = Arc::new(AtomicUsize::new(0));
        let delivered_task = Arc::clone(&delivered);

        let report = lab_runtime_test_with_seed(SEED, move |_cx| async move {
            let (tx, mut rx) = mpsc::channel::<NativeEvent>(EVENT_COUNT);
            for i in 0..EVENT_COUNT {
                let event = NativeEvent::PaneDestroyed {
                    pane_id: i as u64,
                    timestamp_ms: i as i64,
                };
                match dispatch_event_with_timeout(&tx, event, Duration::from_millis(50)).await {
                    EventDispatchOutcome::Sent => {}
                    other => fail(format!("expected Sent, got {other:?}")),
                }
            }
            drop(tx);
            let mut observed = 0usize;
            while let Some(_evt) = recv_next(&mut rx).await {
                observed += 1;
            }
            delivered_task.store(observed, Ordering::SeqCst);
        });

        assert_ran_to_completion(&report);
        assert_eq!(
            delivered.load(Ordering::SeqCst),
            EVENT_COUNT,
            "all dispatched events must be observable under LabRuntime"
        );
        assert!(
            report.oracles_passed,
            "LabRuntime oracles must all pass: {report:?}"
        );
        assert!(
            wall_start.elapsed() < Duration::from_secs(1),
            "virtual-time scheduling must not consume a real second; took {:?}",
            wall_start.elapsed()
        );
    }

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for native events tests");
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

    async fn recv_next<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
        let cx = crate::cx::for_testing();
        rx.recv(&cx).await.ok()
    }

    async fn send_value<T>(tx: &mpsc::Sender<T>, value: T) -> Result<(), mpsc::SendError<T>> {
        let cx = crate::cx::for_testing();
        tx.send(&cx, value).await
    }

    async fn recv_event(
        event_rx: &mut mpsc::Receiver<NativeEvent>,
        timeout: Duration,
        label: &'static str,
    ) -> NativeEvent {
        crate::runtime_async::timeout(timeout, recv_next(event_rx))
            .await
            .expect("timeout")
            .expect(label)
    }

    async fn assert_listener_shutdown(handle: task::JoinHandle<Result<(), NativeEventError>>) {
        crate::runtime_async::timeout(Duration::from_secs(2), handle)
            .await
            .expect("listener shutdown timed out")
            .expect("listener task failed")
            .expect("listener returned a terminal error");
    }

    // ── NativeEventListener ────────────────────────────────────────

    #[test]
    fn bind_empty_path_returns_error() {
        run_async_test(async {
            let result = NativeEventListener::bind(PathBuf::from("")).await;
            assert!(result.is_err());
            match result {
                Err(NativeEventError::EmptySocketPath) => {}
                Err(other) => fail(format!("expected EmptySocketPath, got: {other}")),
                Ok(_) => fail("expected error"),
            }
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `bind_with_cx` must successfully
    /// bind when given a fresh, uncancelled cx — producing a
    /// listener on the same socket path as the legacy `bind`.
    #[test]
    fn bind_with_cx_succeeds_on_fresh_cx() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("bind-cx-fresh.sock");
            let cx = crate::cx::for_testing();

            let result = NativeEventListener::bind_with_cx(&cx, socket_path.clone()).await;
            match &result {
                Ok(_) => {}
                Err(e) => fail(format!("bind_with_cx should succeed on fresh cx: {e}")),
            }
            #[cfg(unix)]
            assert!(socket_path.exists(), "socket should exist after bind");
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `bind_with_cx` must return an
    /// `Io(Interrupted)` error on a pre-cancelled cx, without
    /// creating the socket file.
    #[test]
    fn bind_with_precancelled_cx_returns_interrupted() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("bind-cx-cancelled.sock");
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET pre-cancel native_events bind detail"),
            );

            let result = NativeEventListener::bind_with_cx(&cx, socket_path.clone()).await;
            match result {
                Err(NativeEventError::Io(err)) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
                    let message = err.to_string();
                    assert_eq!(message, "native_event_context_interrupted:bind_entry");
                    assert!(!message.contains("SECRET"), "{message}");
                }
                Err(other) => fail(format!("expected Io(Interrupted), got: {other}")),
                Ok(_) => fail("bind_with_cx should fail on cancelled cx"),
            }
            assert!(
                !socket_path.exists(),
                "socket must not exist after cancelled bind"
            );
        });
    }

    #[test]
    fn accepted_connection_requires_live_context_and_open_shutdown_gate() {
        let active_cx = crate::cx::for_testing();
        let shutdown = AtomicBool::new(false);
        assert!(native_connection_spawn_allowed(&active_cx, &shutdown));

        shutdown.store(true, Ordering::SeqCst);
        assert!(!native_connection_spawn_allowed(&active_cx, &shutdown));

        shutdown.store(false, Ordering::SeqCst);
        let cancelled_cx = crate::cx::for_testing();
        cancelled_cx.cancel_with(
            crate::outcome::CancelKind::User,
            Some("SECRET post-accept cancellation detail"),
        );
        assert!(!native_connection_spawn_allowed(&cancelled_cx, &shutdown));
    }

    #[test]
    fn native_connection_task_drain_never_claims_quarantined_authority_settled() {
        assert_eq!(
            classify_native_connection_task_drain(false, JoinSetSettlement::Settled),
            NativeConnectionTaskDrainOutcome::Settled
        );
        assert_eq!(
            classify_native_connection_task_drain(
                false,
                JoinSetSettlement::Incomplete {
                    active_tasks: 0,
                    unacknowledged_tasks: 1,
                },
            ),
            NativeConnectionTaskDrainOutcome::Incomplete {
                active_tasks: 0,
                unacknowledged_tasks: 1,
            }
        );
        assert_eq!(
            classify_native_connection_task_drain(true, JoinSetSettlement::Settled),
            NativeConnectionTaskDrainOutcome::Settled
        );
        assert_eq!(
            classify_native_connection_task_drain(
                true,
                JoinSetSettlement::Incomplete {
                    active_tasks: 2,
                    unacknowledged_tasks: 1,
                },
            ),
            NativeConnectionTaskDrainOutcome::TimedOut {
                active_tasks: 2,
                unacknowledged_tasks: 1,
            }
        );
    }

    #[test]
    fn cancelled_dispatch_never_commits_a_reserved_event() {
        run_async_test(async {
            let (event_tx, mut event_rx) = mpsc::channel(1);
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("SECRET dispatch cancellation detail"),
            );
            let event = NativeEvent::PaneDestroyed {
                pane_id: 7,
                timestamp_ms: 11,
            };

            assert_eq!(
                dispatch_event_with_timeout_with_cx(
                    &cx,
                    &event_tx,
                    event,
                    Duration::from_millis(50),
                )
                .await,
                EventDispatchOutcome::ContextEnded
            );
            assert!(
                event_rx.try_recv().is_err(),
                "cancelled dispatch must not publish the event"
            );
        });
    }

    #[test]
    fn bind_existing_regular_file_returns_error() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("exists.sock");
            // Create the file first
            std::fs::write(&socket_path, b"").expect("create file");

            let result = NativeEventListener::bind(socket_path).await;
            assert!(result.is_err());
            match result {
                Err(NativeEventError::SocketAlreadyExists(_)) => {}
                Err(other) => fail(format!("expected SocketAlreadyExists, got: {other}")),
                Ok(_) => fail("expected error"),
            }
        });
    }

    #[test]
    fn bind_active_socket_returns_error() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("active.sock");
            let active_cx = crate::cx::for_testing();
            let _active_listener = event_socket::bind_with_cx(&active_cx, &socket_path)
                .await
                .expect("bind active socket");

            let result = NativeEventListener::bind(socket_path).await;
            assert!(result.is_err());
            match result {
                Err(NativeEventError::SocketAlreadyExists(_)) => {}
                Err(other) => fail(format!("expected SocketAlreadyExists, got: {other}")),
                Ok(_) => fail("expected error"),
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn bind_replaces_stale_socket_path() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("stale.sock");
            // Create a stale socket file via std::os::unix::net (no cleanup on drop)
            let std_listener =
                std::os::unix::net::UnixListener::bind(&socket_path).expect("bind std socket");
            drop(std_listener);
            assert!(
                socket_path.exists(),
                "socket path should persist after std listener drop"
            );

            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind replaces stale socket path");
            assert!(
                socket_path.exists(),
                "rebound listener should recreate socket"
            );

            drop(listener);
        });
    }

    #[cfg(unix)]
    #[test]
    fn listener_drop_removes_socket_file() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("drop-cleanup.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            assert!(socket_path.exists(), "socket should exist after bind");

            drop(listener);

            assert!(
                !socket_path.exists(),
                "socket path should be cleaned up on drop"
            );
        });
    }

    #[test]
    fn bind_creates_parent_directories() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("sub").join("dir").join("deep.sock");
            let result = NativeEventListener::bind(socket_path).await;
            assert!(result.is_ok());
        });
    }

    /// ft-xbnl0.2.3 Cx-first: `run_with_cx` must return promptly when the
    /// caller's capability context is already cancelled on entry. A
    /// pre-cancelled wait should not enter the accept loop, spawn any
    /// connection tasks, or consume a real `ACCEPT_POLL_INTERVAL` tick.
    ///
    /// This mirrors the short-circuit contract landed for
    /// `EventWaiter::wait_with_cx` (event_stream.rs) — operators who have
    /// abandoned the watch should not pay the subscribe / accept cost.
    #[test]
    fn run_with_cx_pre_cancelled_exits_immediately() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("precancel.sock");
            let listener = NativeEventListener::bind(socket_path)
                .await
                .expect("bind listener");
            let (event_tx, _event_rx) = mpsc::channel::<NativeEvent>(4);
            let shutdown_flag = Arc::new(AtomicBool::new(false));

            // Pre-cancel the Cx. No shutdown flag will ever be set, so a
            // Cx-unaware run loop would block forever on the accept poll.
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("ft-xbnl0.2.3 pre-cancel native_events"),
            );

            let wall_start = std::time::Instant::now();
            listener
                .run_with_cx(&cx, event_tx, Arc::clone(&shutdown_flag))
                .await
                .expect("supported native event listener run must terminate cleanly");

            assert!(
                wall_start.elapsed() < Duration::from_secs(1),
                "pre-cancelled run_with_cx must not consume a full accept \
                 poll tick; took {:?}",
                wall_start.elapsed()
            );
            assert!(
                !shutdown_flag.load(Ordering::SeqCst),
                "run_with_cx must return via Cx cancellation path, not \
                 shutdown flag"
            );
        });
    }

    // ── Integration: listener + multiple events ────────────────────

    #[test]
    fn listener_emits_events() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("native.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(8);
            let shutdown = Arc::new(AtomicBool::new(false));

            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            let mut stream = event_socket::connect(socket_path).await.expect("connect");
            let payload = r#"{"type":"pane_output","pane_id":7,"data_b64":"aGV5","ts":42}"#;
            stream
                .write_all(format!("{payload}\n").as_bytes())
                .await
                .expect("write");

            let event = recv_event(&mut event_rx, Duration::from_secs(2), "event").await;

            match event {
                NativeEvent::PaneOutput {
                    pane_id,
                    data,
                    timestamp_ms,
                    ..
                } => {
                    assert_eq!(pane_id, 7);
                    assert_eq!(data, b"hey");
                    assert_eq!(timestamp_ms, 42);
                }
                _ => fail("unexpected event type"),
            }

            drop(stream);
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
        });
    }

    #[test]
    fn listener_handles_multiple_events_on_one_connection() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("multi.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(16);
            let shutdown = Arc::new(AtomicBool::new(false));

            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            let mut stream = event_socket::connect(socket_path).await.expect("connect");

            // Send hello (ignored) + two real events
            let lines = [
                r#"{"type":"hello","proto":1}"#,
                r#"{"type":"pane_created","pane_id":1,"domain":"local","ts":100}"#,
                r#"{"type":"pane_destroyed","pane_id":1,"ts":200}"#,
            ];
            for line in &lines {
                stream
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .expect("write");
            }

            // Should receive exactly 2 events (hello is filtered)
            let ev1 = recv_event(&mut event_rx, Duration::from_secs(2), "event 1").await;
            assert!(matches!(ev1, NativeEvent::PaneCreated { pane_id: 1, .. }));

            let ev2 = recv_event(&mut event_rx, Duration::from_secs(2), "event 2").await;
            assert!(matches!(ev2, NativeEvent::PaneDestroyed { pane_id: 1, .. }));

            drop(stream);
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
        });
    }

    #[test]
    fn listener_skips_invalid_json_lines() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("invalid.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(16);
            let shutdown = Arc::new(AtomicBool::new(false));

            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            let mut stream = event_socket::connect(socket_path).await.expect("connect");

            // Send invalid JSON followed by valid event
            let lines = [
                "this is not json",
                r#"{"type":"pane_destroyed","pane_id":42,"ts":999}"#,
            ];
            for line in &lines {
                stream
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .expect("write");
            }

            // Should receive only the valid event
            let event = recv_event(&mut event_rx, Duration::from_secs(2), "event").await;
            assert!(matches!(
                event,
                NativeEvent::PaneDestroyed {
                    pane_id: 42,
                    timestamp_ms: 999
                }
            ));

            drop(stream);
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
        });
    }

    #[test]
    fn listener_accepts_reconnect_after_disconnect() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("reconnect.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(16);
            let shutdown = Arc::new(AtomicBool::new(false));

            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            // First connection sends one event and disconnects.
            let mut stream_one = event_socket::connect(socket_path.clone())
                .await
                .expect("connect first stream");
            stream_one
                .write_all(r#"{"type":"pane_destroyed","pane_id":41,"ts":100}"#.as_bytes())
                .await
                .expect("write first event");
            stream_one.write_all(b"\n").await.expect("write newline");
            drop(stream_one);

            let first = recv_event(&mut event_rx, Duration::from_secs(2), "first event").await;
            assert!(matches!(
                first,
                NativeEvent::PaneDestroyed {
                    pane_id: 41,
                    timestamp_ms: 100
                }
            ));

            // Second connection should still be accepted and delivered.
            let mut stream_two = event_socket::connect(socket_path)
                .await
                .expect("connect second stream");
            stream_two
                .write_all(
                    r#"{"type":"pane_created","pane_id":42,"domain":"local","ts":200}"#.as_bytes(),
                )
                .await
                .expect("write second event");
            stream_two.write_all(b"\n").await.expect("write newline");

            let second = recv_event(&mut event_rx, Duration::from_secs(2), "second event").await;
            assert!(matches!(
                second,
                NativeEvent::PaneCreated {
                    pane_id: 42,
                    ref domain,
                    timestamp_ms: 200,
                    ..
                } if domain == "local"
            ));

            drop(stream_two);
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
        });
    }

    #[test]
    fn listener_drops_oversized_line_and_continues() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("oversized.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(16);
            let shutdown = Arc::new(AtomicBool::new(false));

            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            let mut stream = event_socket::connect(socket_path).await.expect("connect");
            let oversized = "x".repeat(MAX_EVENT_LINE_BYTES + 1);
            stream
                .write_all(oversized.as_bytes())
                .await
                .expect("write oversized line");
            stream.write_all(b"\n").await.expect("write newline");
            stream
                .write_all(r#"{"type":"pane_destroyed","pane_id":9,"ts":777}"#.as_bytes())
                .await
                .expect("write valid line");
            stream.write_all(b"\n").await.expect("write newline");

            let event = recv_event(&mut event_rx, Duration::from_secs(2), "event").await;
            assert!(matches!(
                event,
                NativeEvent::PaneDestroyed {
                    pane_id: 9,
                    timestamp_ms: 777
                }
            ));

            drop(stream);
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
        });
    }

    #[test]
    fn shutdown_flag_stops_listener() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("shutdown.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, _event_rx) = mpsc::channel(8);
            let shutdown = Arc::new(AtomicBool::new(false));

            let shutdown_clone = Arc::clone(&shutdown);
            let handle = task::spawn(listener.run(event_tx, shutdown_clone));

            // Set shutdown flag
            shutdown.store(true, Ordering::SeqCst);

            // Listener should exit within a few poll intervals
            assert_listener_shutdown(handle).await;
            #[cfg(unix)]
            assert!(
                !socket_path.exists(),
                "socket path should be removed after listener shutdown"
            );
        });
    }

    #[test]
    fn shutdown_aborts_and_drains_a_quiet_accepted_connection() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("shutdown-quiet-connection.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(8);
            let shutdown = Arc::new(AtomicBool::new(false));
            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            let mut stream = event_socket::connect(socket_path)
                .await
                .expect("connect quiet stream");
            stream
                .write_all(r#"{"type":"pane_destroyed","pane_id":73,"ts":900}"#.as_bytes())
                .await
                .expect("write event");
            stream.write_all(b"\n").await.expect("write newline");
            let delivered = recv_event(
                &mut event_rx,
                Duration::from_secs(2),
                "accepted connection event",
            )
            .await;
            assert!(matches!(
                delivered,
                NativeEvent::PaneDestroyed {
                    pane_id: 73,
                    timestamp_ms: 900
                }
            ));

            // Keep `stream` open and silent. Without explicit child abort the
            // listener's join drain blocks forever in the next line read.
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
            drop(stream);
        });
    }

    fn pane_destroyed_event(pane_id: u64) -> NativeEvent {
        NativeEvent::PaneDestroyed {
            pane_id,
            timestamp_ms: 1,
        }
    }

    #[test]
    fn dispatch_event_sends_when_capacity_available() {
        run_async_test(async {
            let (tx, mut rx) = mpsc::channel(1);

            let outcome = dispatch_event_with_timeout(
                &tx,
                pane_destroyed_event(7),
                Duration::from_millis(20),
            )
            .await;

            assert_eq!(outcome, EventDispatchOutcome::Sent);
            let event = recv_next(&mut rx).await.expect("event should be delivered");
            assert!(matches!(
                event,
                NativeEvent::PaneDestroyed { pane_id: 7, .. }
            ));
        });
    }

    #[test]
    fn dispatch_event_reports_closed_when_receiver_dropped() {
        run_async_test(async {
            let (tx, rx) = mpsc::channel(1);
            drop(rx);

            let outcome = dispatch_event_with_timeout(
                &tx,
                pane_destroyed_event(8),
                Duration::from_millis(20),
            )
            .await;

            assert_eq!(outcome, EventDispatchOutcome::Closed);
        });
    }

    #[test]
    fn dispatch_event_reports_backpressure_when_queue_full() {
        run_async_test(async {
            let (tx, _rx) = mpsc::channel(1);
            send_value(&tx, pane_destroyed_event(1))
                .await
                .expect("first send should fit in queue");

            let outcome = dispatch_event_with_timeout(
                &tx,
                pane_destroyed_event(2),
                Duration::from_millis(10),
            )
            .await;

            assert_eq!(outcome, EventDispatchOutcome::Backpressure);
        });
    }

    #[test]
    fn dispatch_event_with_timeout_with_precancelled_cx_drops_event() {
        run_async_test(async {
            let (tx, _rx) = mpsc::channel(1);
            let cx = crate::cx::for_testing();
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("ft-xbnl0.2.3 dispatch cancel"),
            );

            let outcome = dispatch_event_with_timeout_with_cx(
                &cx,
                &tx,
                pane_destroyed_event(3),
                Duration::from_millis(20),
            )
            .await;

            assert_eq!(
                outcome,
                EventDispatchOutcome::ContextEnded,
                "pre-cancelled cx must not be reported as closure or queue pressure"
            );
        });
    }

    #[test]
    fn dispatch_event_reports_expired_capability_budget_separately_from_backpressure() {
        run_async_test(async {
            let (tx, _rx) = mpsc::channel(1);
            send_value(&tx, pane_destroyed_event(1))
                .await
                .expect("first send should fill the queue");
            let cx = crate::cx::Cx::for_testing_with_budget(
                crate::cx::Budget::new().with_deadline(crate::runtime_async::RuntimeTime::ZERO),
            );

            let outcome = dispatch_event_with_timeout_with_cx(
                &cx,
                &tx,
                pane_destroyed_event(2),
                Duration::from_secs(1),
            )
            .await;

            assert_eq!(outcome, EventDispatchOutcome::ContextEnded);
        });
    }

    // --- NativePaneState ---

    #[test]
    fn native_pane_state_clone() {
        let s = NativePaneState {
            title: "test pane".to_string(),
            rows: 24,
            cols: 80,
            is_alt_screen: true,
            cursor_row: 5,
            cursor_col: 10,
        };
        let s2 = s.clone();
        assert_eq!(s2.title, "test pane");
        assert_eq!(s2.rows, 24);
        assert_eq!(s2.cols, 80);
        assert!(s2.is_alt_screen);
    }

    #[test]
    fn native_pane_state_debug() {
        let s = NativePaneState {
            title: "t".to_string(),
            rows: 1,
            cols: 1,
            is_alt_screen: false,
            cursor_row: 0,
            cursor_col: 0,
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("NativePaneState"));
    }

    #[test]
    fn native_pane_state_max_values() {
        let s = NativePaneState {
            title: "x".repeat(1000),
            rows: u16::MAX,
            cols: u16::MAX,
            is_alt_screen: true,
            cursor_row: u32::MAX,
            cursor_col: u32::MAX,
        };
        assert_eq!(s.rows, u16::MAX);
        assert_eq!(s.cursor_row, u32::MAX);
    }

    // --- NativeEvent variant tests ---

    #[test]
    fn native_event_clone_pane_output() {
        let e = NativeEvent::PaneOutput {
            pane_id: 1,
            data: vec![65, 66, 67],
            timestamp_ms: 1000,
            dropped_bytes: 0,
        };
        let e2 = e.clone();
        assert!(matches!(e2, NativeEvent::PaneOutput { pane_id: 1, .. }));
    }

    #[test]
    fn native_event_clone_state_change() {
        let e = NativeEvent::StateChange {
            pane_id: 2,
            state: NativePaneState {
                title: "t".to_string(),
                rows: 24,
                cols: 80,
                is_alt_screen: false,
                cursor_row: 0,
                cursor_col: 0,
            },
            timestamp_ms: 2000,
        };
        let e2 = e.clone();
        assert!(matches!(e2, NativeEvent::StateChange { pane_id: 2, .. }));
    }

    #[test]
    fn native_event_clone_user_var() {
        let e = NativeEvent::UserVarChanged {
            pane_id: 3,
            name: "TERM".to_string(),
            value: "xterm".to_string(),
            timestamp_ms: 3000,
        };
        let e2 = e.clone();
        assert!(matches!(e2, NativeEvent::UserVarChanged { pane_id: 3, .. }));
    }

    #[test]
    fn native_event_clone_pane_created() {
        let e = NativeEvent::PaneCreated {
            pane_id: 4,
            domain: "local".to_string(),
            cwd: Some("/tmp".to_string()),
            timestamp_ms: 4000,
        };
        let e2 = e.clone();
        assert!(matches!(e2, NativeEvent::PaneCreated { pane_id: 4, .. }));
    }

    #[test]
    fn native_event_clone_pane_destroyed() {
        let e = NativeEvent::PaneDestroyed {
            pane_id: 5,
            timestamp_ms: 5000,
        };
        let e2 = e.clone();
        assert!(matches!(e2, NativeEvent::PaneDestroyed { pane_id: 5, .. }));
    }

    #[test]
    fn native_event_debug_variants() {
        let events: Vec<NativeEvent> = vec![
            NativeEvent::PaneOutput {
                pane_id: 1,
                data: vec![],
                timestamp_ms: 0,
                dropped_bytes: 0,
            },
            NativeEvent::StateChange {
                pane_id: 2,
                state: NativePaneState {
                    title: String::new(),
                    rows: 0,
                    cols: 0,
                    is_alt_screen: false,
                    cursor_row: 0,
                    cursor_col: 0,
                },
                timestamp_ms: 0,
            },
            NativeEvent::UserVarChanged {
                pane_id: 3,
                name: String::new(),
                value: String::new(),
                timestamp_ms: 0,
            },
            NativeEvent::PaneCreated {
                pane_id: 4,
                domain: String::new(),
                cwd: None,
                timestamp_ms: 0,
            },
            NativeEvent::PaneDestroyed {
                pane_id: 5,
                timestamp_ms: 0,
            },
        ];
        for e in &events {
            let dbg = format!("{:?}", e);
            assert!(!dbg.is_empty());
        }
    }

    // --- event_metadata ---

    #[test]
    fn event_metadata_pane_output() {
        let e = NativeEvent::PaneOutput {
            pane_id: 42,
            data: vec![],
            timestamp_ms: 0,
            dropped_bytes: 0,
        };
        let (kind, id) = event_metadata(&e);
        assert_eq!(kind, "pane_output");
        assert_eq!(id, 42);
    }

    #[test]
    fn event_metadata_state_change() {
        let e = NativeEvent::StateChange {
            pane_id: 10,
            state: NativePaneState {
                title: String::new(),
                rows: 0,
                cols: 0,
                is_alt_screen: false,
                cursor_row: 0,
                cursor_col: 0,
            },
            timestamp_ms: 0,
        };
        let (kind, id) = event_metadata(&e);
        assert_eq!(kind, "state_change");
        assert_eq!(id, 10);
    }

    #[test]
    fn event_metadata_user_var_changed() {
        let e = NativeEvent::UserVarChanged {
            pane_id: 7,
            name: "k".to_string(),
            value: "v".to_string(),
            timestamp_ms: 0,
        };
        let (kind, id) = event_metadata(&e);
        assert_eq!(kind, "user_var");
        assert_eq!(id, 7);
    }

    #[test]
    fn event_metadata_pane_created() {
        let e = NativeEvent::PaneCreated {
            pane_id: 99,
            domain: "d".to_string(),
            cwd: None,
            timestamp_ms: 0,
        };
        let (kind, id) = event_metadata(&e);
        assert_eq!(kind, "pane_created");
        assert_eq!(id, 99);
    }

    #[test]
    fn event_metadata_pane_destroyed() {
        let e = NativeEvent::PaneDestroyed {
            pane_id: 55,
            timestamp_ms: 0,
        };
        let (kind, id) = event_metadata(&e);
        assert_eq!(kind, "pane_destroyed");
        assert_eq!(id, 55);
    }

    // --- NativeEventError extras ---

    #[test]
    fn error_empty_socket_path_exact_message() {
        let e = NativeEventError::EmptySocketPath;
        assert_eq!(format!("{e}"), "socket path is empty");
    }

    #[test]
    fn error_socket_already_exists_contains_path() {
        let e = NativeEventError::SocketAlreadyExists("/tmp/test.sock".to_string());
        let msg = format!("{e}");
        assert!(msg.contains("/tmp/test.sock"));
    }

    #[test]
    fn error_io_permission_denied() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e = NativeEventError::Io(io_err);
        let msg = format!("{e}");
        assert!(msg.contains("denied"));
    }

    #[test]
    fn error_debug_all_variants() {
        let errors: Vec<NativeEventError> = vec![
            NativeEventError::EmptySocketPath,
            NativeEventError::ContextUnavailable,
            NativeEventError::SocketAlreadyExists("x".into()),
            NativeEventError::Security(NativeEventSecurityError::PeerCredentialsUnavailable),
            NativeEventError::Io(std::io::Error::other("test")),
            NativeEventError::ConnectionTaskAdmissionFailed,
            NativeEventError::ConnectionTaskDrainTimedOut,
            NativeEventError::ConnectionTaskDrainIncomplete,
        ];
        for e in &errors {
            let dbg = format!("{:?}", e);
            assert!(!dbg.is_empty());
        }
    }

    // --- EventDispatchOutcome ---

    #[test]
    fn dispatch_outcome_equality() {
        assert_eq!(EventDispatchOutcome::Sent, EventDispatchOutcome::Sent);
        assert_ne!(
            EventDispatchOutcome::Sent,
            EventDispatchOutcome::Backpressure
        );
        assert_ne!(
            EventDispatchOutcome::Backpressure,
            EventDispatchOutcome::Closed
        );
        assert_ne!(
            EventDispatchOutcome::ContextEnded,
            EventDispatchOutcome::Backpressure
        );
        assert_ne!(
            EventDispatchOutcome::ContextEnded,
            EventDispatchOutcome::Closed
        );
    }

    #[test]
    fn dispatch_outcome_copy() {
        let o = EventDispatchOutcome::Sent;
        let o2 = o;
        assert_eq!(o, o2);
    }

    #[test]
    fn dispatch_outcome_debug() {
        let dbg = format!("{:?}", EventDispatchOutcome::Backpressure);
        assert!(dbg.contains("Backpressure"));
    }

    // --- decode_wire_event edge cases ---

    #[test]
    fn decode_user_var_empty_name_value() {
        let json = r#"{"type":"user_var","pane_id":1,"name":"","value":"","ts":100}"#;
        let result = decode_wire_event(json).unwrap();
        assert!(result.is_some());
        if let Some(NativeEvent::UserVarChanged { name, value, .. }) = result {
            assert!(name.is_empty());
            assert!(value.is_empty());
        }
    }

    #[test]
    fn decode_pane_created_empty_domain() {
        let json = r#"{"type":"pane_created","pane_id":1,"domain":"","ts":100}"#;
        let result = decode_wire_event(json).unwrap();
        assert!(result.is_some());
        if let Some(NativeEvent::PaneCreated { domain, cwd, .. }) = result {
            assert!(domain.is_empty());
            assert!(cwd.is_none());
        }
    }

    #[test]
    fn decode_timestamp_zero() {
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hi");
        let json = format!(
            r#"{{"type":"pane_output","pane_id":1,"data_b64":"{}","ts":0}}"#,
            b64
        );
        let result = decode_wire_event(&json).unwrap();
        assert!(result.is_some());
        if let Some(NativeEvent::PaneOutput { timestamp_ms, .. }) = result {
            assert_eq!(timestamp_ms, 0);
        }
    }

    // --- Constants validation ---

    #[test]
    fn constants_are_positive() {
        const {
            assert!(MAX_EVENT_LINE_BYTES > 0);
            assert!(MAX_OUTPUT_BYTES > 0);
        };
        assert!(!ACCEPT_POLL_INTERVAL.is_zero());
        assert!(!EVENT_SEND_TIMEOUT.is_zero());
    }

    #[test]
    fn output_bytes_less_than_line_bytes() {
        const {
            assert!(
                MAX_OUTPUT_BYTES < MAX_EVENT_LINE_BYTES,
                "MAX_OUTPUT_BYTES should be less than MAX_EVENT_LINE_BYTES"
            );
        };
    }

    // --- WireEvent serialize → deserialize roundtrip tests ---

    #[test]
    fn wire_event_hello_roundtrip() {
        let event = WireEvent::Hello {
            proto: Some(1),
            wezterm_version: Some("FrankenTerm 0.1.0".into()),
            ts: Some(1234567890),
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::Hello {
                proto,
                wezterm_version,
                ts,
            } => {
                assert_eq!(proto, Some(1));
                assert_eq!(wezterm_version.as_deref(), Some("FrankenTerm 0.1.0"));
                assert_eq!(ts, Some(1234567890));
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_hello_minimal_roundtrip() {
        let event = WireEvent::Hello {
            proto: None,
            wezterm_version: None,
            ts: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            parsed,
            WireEvent::Hello {
                proto: None,
                wezterm_version: None,
                ts: None
            }
        ));
    }

    #[test]
    fn wire_event_pane_output_roundtrip() {
        let event = WireEvent::PaneOutput {
            pane_id: 42,
            data_b64: base64::engine::general_purpose::STANDARD.encode(b"hello world"),
            ts: 9999,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::PaneOutput {
                pane_id,
                data_b64,
                ts,
            } => {
                assert_eq!(pane_id, 42);
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&data_b64)
                    .unwrap();
                assert_eq!(decoded, b"hello world");
                assert_eq!(ts, 9999);
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_state_change_roundtrip() {
        let event = WireEvent::StateChange {
            pane_id: 7,
            state: WirePaneState {
                title: "vim".into(),
                rows: 40,
                cols: 120,
                is_alt_screen: true,
                cursor_row: 10,
                cursor_col: 5,
            },
            ts: 555,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::StateChange { pane_id, state, ts } => {
                assert_eq!(pane_id, 7);
                assert_eq!(state.title, "vim");
                assert_eq!(state.rows, 40);
                assert_eq!(state.cols, 120);
                assert!(state.is_alt_screen);
                assert_eq!(state.cursor_row, 10);
                assert_eq!(state.cursor_col, 5);
                assert_eq!(ts, 555);
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_user_var_roundtrip() {
        let event = WireEvent::UserVar {
            pane_id: 3,
            name: "FT_STATUS".into(),
            value: "active".into(),
            ts: 888,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::UserVar {
                pane_id,
                name,
                value,
                ts,
            } => {
                assert_eq!(pane_id, 3);
                assert_eq!(name, "FT_STATUS");
                assert_eq!(value, "active");
                assert_eq!(ts, 888);
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_pane_created_roundtrip() {
        let event = WireEvent::PaneCreated {
            pane_id: 10,
            domain: "local".into(),
            cwd: Some("/home/user".into()),
            ts: 1000,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::PaneCreated {
                pane_id,
                domain,
                cwd,
                ts,
            } => {
                assert_eq!(pane_id, 10);
                assert_eq!(domain, "local");
                assert_eq!(cwd, Some("/home/user".into()));
                assert_eq!(ts, 1000);
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_pane_created_no_cwd_roundtrip() {
        let event = WireEvent::PaneCreated {
            pane_id: 11,
            domain: "remote".into(),
            cwd: None,
            ts: 2000,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::PaneCreated { cwd, .. } => {
                assert!(cwd.is_none());
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_pane_destroyed_roundtrip() {
        let event = WireEvent::PaneDestroyed {
            pane_id: 99,
            ts: 3000,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: WireEvent = serde_json::from_str(&json).unwrap();
        match parsed {
            WireEvent::PaneDestroyed { pane_id, ts } => {
                assert_eq!(pane_id, 99);
                assert_eq!(ts, 3000);
            }
            _ => fail("wrong variant after roundtrip"),
        }
    }

    #[test]
    fn wire_event_all_variants_roundtrip() {
        let events = vec![
            WireEvent::Hello {
                proto: Some(1),
                wezterm_version: Some("v1".into()),
                ts: Some(100),
            },
            WireEvent::PaneOutput {
                pane_id: 1,
                data_b64: base64::engine::general_purpose::STANDARD.encode(b"test"),
                ts: 200,
            },
            WireEvent::StateChange {
                pane_id: 2,
                state: WirePaneState {
                    title: "zsh".into(),
                    rows: 24,
                    cols: 80,
                    is_alt_screen: false,
                    cursor_row: 0,
                    cursor_col: 0,
                },
                ts: 300,
            },
            WireEvent::UserVar {
                pane_id: 3,
                name: "k".into(),
                value: "v".into(),
                ts: 400,
            },
            WireEvent::PaneCreated {
                pane_id: 4,
                domain: "local".into(),
                cwd: Some("/tmp".into()),
                ts: 500,
            },
            WireEvent::PaneDestroyed {
                pane_id: 5,
                ts: 600,
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: WireEvent = serde_json::from_str(&json).unwrap();
            // Verify the JSON roundtrip produces valid output
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2, "double roundtrip should be stable");
        }
    }

    #[test]
    fn wire_event_malformed_json_no_panic() {
        let garbage_inputs = [
            "",
            "not json",
            "{",
            "{}",
            r#"{"type":"hello""#,
            "null",
            "42",
            "[1,2,3]",
            r#"{"type":null}"#,
        ];
        for input in &garbage_inputs {
            let result = serde_json::from_str::<WireEvent>(input);
            assert!(result.is_err(), "expected error for input: {input}");
        }
    }

    #[test]
    fn wire_pane_state_roundtrip() {
        let state = WirePaneState {
            title: "nvim main.rs".into(),
            rows: 50,
            cols: 200,
            is_alt_screen: true,
            cursor_row: 25,
            cursor_col: 42,
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: WirePaneState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.title, "nvim main.rs");
        assert_eq!(parsed.rows, 50);
        assert_eq!(parsed.cols, 200);
        assert!(parsed.is_alt_screen);
        assert_eq!(parsed.cursor_row, 25);
        assert_eq!(parsed.cursor_col, 42);
    }

    // --- Throughput / rapid events test ---

    #[test]
    fn listener_handles_rapid_events() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("rapid.sock");
            let listener = NativeEventListener::bind(socket_path.clone())
                .await
                .expect("bind listener");
            let (event_tx, mut event_rx) = mpsc::channel(4096);
            let shutdown = Arc::new(AtomicBool::new(false));

            let handle = task::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

            let mut stream = event_socket::connect(socket_path).await.expect("connect");

            // Send 1000 events rapidly (enough to test throughput without being
            // too slow for CI)
            let event_count = 1000u64;
            for i in 0..event_count {
                let line = format!(
                    r#"{{"type":"pane_destroyed","pane_id":{},"ts":{}}}"#,
                    i,
                    i * 10
                );
                stream
                    .write_all(line.as_bytes())
                    .await
                    .expect("write event");
                stream.write_all(b"\n").await.expect("write newline");
            }

            // Receive all events
            let mut received = 0u64;
            let deadline = Duration::from_secs(10);
            while received < event_count {
                match crate::runtime_async::timeout(deadline, recv_next(&mut event_rx)).await {
                    Ok(Some(_)) => received += 1,
                    Ok(None) => break,
                    Err(elapsed) => fail(format!(
                        "timeout after {elapsed}: received {received}/{event_count} events"
                    )),
                }
            }

            assert_eq!(
                received, event_count,
                "all events should be delivered without loss"
            );

            drop(stream);
            shutdown.store(true, Ordering::SeqCst);
            assert_listener_shutdown(handle).await;
        });
    }
}
