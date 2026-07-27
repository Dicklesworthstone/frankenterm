//! Native event listener for vendored WezTerm integrations.
//!
//! Listens on a local socket for newline-delimited JSON events emitted by a
//! vendored WezTerm build (feature-gated on the WezTerm side). Unix uses the
//! existing Unix-domain-socket path; Windows uses the in-tree `frankenterm-uds`
//! Windows transport.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;

use crate::runtime_async::mpsc;
use crate::runtime_async::task::JoinSet;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
#[cfg(any(unix, windows))]
use socket_transport::{UnixListener, UnixStream};
use tracing::{debug, warn};

const MAX_EVENT_LINE_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(25);

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
        UnixListener, UnixStream, bind, buffered, lines_with_max_length, next_line_with_cx,
    };

    #[cfg(windows)]
    mod windows {
        use std::io;
        use std::path::Path;
        use std::time::Duration;

        #[cfg(test)]
        pub use asupersync::io::AsyncWriteExt;
        pub use asupersync::io::{AsyncRead, BufReader};
        pub use frankenterm_uds::UnixStream;

        pub struct UnixListener {
            inner: frankenterm_uds::UnixListener,
        }

        pub type LineReader<T> = asupersync::io::Lines<BufReader<T>>;

        pub async fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
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

            cx.checkpoint().map_err(|err| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    format!("next_line cancelled: {err}"),
                )
            })?;
            match lines.next().await {
                Some(Ok(line)) => Ok(Some(line)),
                Some(Err(err)) => Err(err),
                None => Ok(None),
            }
        }
    }

    #[cfg(windows)]
    pub use windows::*;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventDispatchOutcome {
    Sent,
    Backpressure,
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
    #[error("socket path already exists: {0}")]
    SocketAlreadyExists(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
pub struct NativeEventListener {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl NativeEventListener {
    pub async fn bind(socket_path: PathBuf) -> Result<Self, NativeEventError> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        Self::bind_with_cx(&cx, socket_path).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`bind`].
    ///
    /// Multi-seam checkpoint structure before each filesystem /
    /// syscall boundary: entry, stale-socket cleanup, parent-dir
    /// creation, listener bind. Gives responsive cancellation
    /// during socket setup — a cx-driven caller cancelled
    /// mid-startup won't touch files it doesn't need to.
    /// Cancellation surfaces as `NativeEventError::Io(Interrupted)`
    /// so the caller's existing error-match arms continue to hold.
    pub async fn bind_with_cx(
        cx: &crate::cx::Cx,
        socket_path: PathBuf,
    ) -> Result<Self, NativeEventError> {
        let check = |stage: &str| -> Result<(), NativeEventError> {
            cx.checkpoint().map_err(|err| {
                NativeEventError::Io(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    format!("native_events bind cancelled {stage}: {err}"),
                ))
            })
        };

        check("at entry")?;

        if socket_path.as_os_str().is_empty() {
            return Err(NativeEventError::EmptySocketPath);
        }

        check("before stale-socket cleanup")?;
        maybe_cleanup_stale_socket(&socket_path)?;

        if let Some(parent) = socket_path.parent() {
            check("before parent-dir creation")?;
            std::fs::create_dir_all(parent)?;
        }

        check("before listener bind")?;
        let listener = socket_transport::bind(&socket_path).await?;
        Ok(Self {
            socket_path,
            listener,
        })
    }

    pub async fn run(self, event_tx: mpsc::Sender<NativeEvent>, shutdown_flag: Arc<AtomicBool>) {
        {
            // ft-xbnl0.2.3: route the legacy entry point through the
            // explicit-Cx accept loop so the listener keeps a single
            // request-rooted cancellation chain for its full lifetime.
            let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            self.run_with_cx(&cx, event_tx, shutdown_flag).await;
        }
    }

    /// Run the accept loop against the caller's asupersync capability
    /// context (ft-xbnl0.2.3 Cx-first entry point).
    ///
    /// Short-circuits before entering the loop if `cx` is already
    /// cancelled — an operator who has abandoned the watch should not
    /// bind subscribers or spawn per-connection tasks. While the loop
    /// runs each accept poll is bound to the caller's `Cx` via
    /// [`crate::runtime_async::timeout_with_cx`], so budget-driven
    /// cancellation from the outer scope cuts the poll wait
    /// deterministically under `LabRuntime` virtual time. Matches the
    /// Cx-first pattern landed by `EventWaiter::wait_with_cx`
    /// (event_stream.rs), `WorkflowRunner::handle_detection_with_cx`,
    /// and `SurvivalModel::run_cx`.
    ///
    /// The legacy [`run`](Self::run) entry point is preserved for
    /// non-migrated callers; this is strictly additive.
    pub async fn run_with_cx(
        self,
        cx: &crate::cx::Cx,
        event_tx: mpsc::Sender<NativeEvent>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        if cx.is_cancel_requested() {
            debug!(
                path = %self.socket_path.display(),
                "native event run aborted: capability context already cancelled"
            );
            return;
        }

        let mut connection_tasks = JoinSet::new();

        loop {
            if shutdown_flag.load(Ordering::SeqCst) || cx.is_cancel_requested() {
                break;
            }

            match crate::runtime_async::timeout_with_cx(
                cx,
                ACCEPT_POLL_INTERVAL,
                self.listener.accept(),
            )
            .await
            {
                Ok(Ok((stream, _addr))) => {
                    let tx = event_tx.clone();
                    let path = self.socket_path.display().to_string();
                    connection_tasks.spawn_with_cx(cx, move |child_cx| async move {
                        if let Err(err) = handle_connection_with_cx(child_cx, stream, tx).await {
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
                                    error = %err,
                                    path = %path,
                                    "native event connection cancelled"
                                );
                            } else {
                                warn!(
                                    error = %err,
                                    error_kind = ?err.kind(),
                                    path = %path,
                                    "native event connection closed with error"
                                );
                            }
                        }
                    });
                }
                Ok(Err(err)) => {
                    warn!(error = %err, path = %self.socket_path.display(), "native event accept failed");
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
                        debug!(
                            error = %err,
                            "native event connection task cancelled"
                        );
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

        while let Some(join_result) = connection_tasks.join_next().await {
            if let Err(err) = join_result {
                // Kept at debug! — after shutdown_flag or cx cancel fires,
                // outstanding tasks are intentionally cancelled and their
                // JoinError surface is expected noise.
                debug!(error = %err, "native event connection task failed during shutdown");
            }
        }
    }
}

impl Drop for NativeEventListener {
    fn drop(&mut self) {
        #[cfg(any(unix, windows))]
        if let Err(err) = std::fs::remove_file(&self.socket_path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                debug!(
                    error = %err,
                    path = %self.socket_path.display(),
                    "failed to remove native event socket path on drop"
                );
            }
        }
    }
}

fn maybe_cleanup_stale_socket(socket_path: &PathBuf) -> Result<(), NativeEventError> {
    let metadata = match std::fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(NativeEventError::Io(err)),
    };

    #[cfg(unix)]
    let is_socket = metadata.file_type().is_socket();
    #[cfg(windows)]
    let is_socket = false;

    if !is_socket {
        return Err(NativeEventError::SocketAlreadyExists(
            socket_path.display().to_string(),
        ));
    }

    #[cfg(unix)]
    match StdUnixStream::connect(socket_path) {
        Ok(_stream) => Err(NativeEventError::SocketAlreadyExists(
            socket_path.display().to_string(),
        )),
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(socket_path)?;
            debug!(
                path = %socket_path.display(),
                "removed stale native event socket path before bind"
            );
            Ok(())
        }
        Err(err) => Err(NativeEventError::Io(err)),
    }

    #[cfg(windows)]
    {
        Err(NativeEventError::SocketAlreadyExists(
            socket_path.display().to_string(),
        ))
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
async fn handle_connection_with_cx(
    cx: crate::cx::Cx,
    stream: UnixStream,
    event_tx: mpsc::Sender<NativeEvent>,
) -> Result<(), std::io::Error> {
    debug!("native event connection accepted (cx path)");
    cx.checkpoint().map_err(|err| {
        std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            format!("native handle_connection cancelled pre-read: {err}"),
        )
    })?;
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

    while let Some(line) = socket_transport::next_line_with_cx(&cx, &mut lines).await? {
        if line.len() > MAX_EVENT_LINE_BYTES {
            warn!(len = line.len(), "native event line too large; dropping");
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
                        warn!(
                            event_kind,
                            pane_id, "native event queue full; dropping event (cx path)"
                        );
                    }
                    EventDispatchOutcome::Closed => {
                        debug!(event_kind, pane_id, "native event channel closed (cx path)");
                        break;
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                // Promoted from debug! — a malformed wire event is a
                // protocol-level anomaly (version skew, corruption, or
                // a hostile client writing to the native-events socket)
                // and must not sink silently into a debug-level log
                // that operators routinely filter out.
                warn!(error = %err, "failed to decode native event (cx path)");
            }
        }
    }

    debug!("native event connection closed (cx path)");
    Ok(())
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
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    dispatch_event_with_timeout_with_cx(&cx, event_tx, event, send_timeout).await
}

/// ft-xbnl0.2.3 Cx-first sibling of [`dispatch_event_with_timeout`].
///
/// Threads the caller's cx into the `event_tx.reserve(cx)` wait,
/// replacing the orphan `cx::for_request()` that severed the
/// cancellation chain from `run_with_cx`'s parent. Also uses
/// `timeout_with_cx` so a cx-cancel unblocks the reserve wait
/// immediately rather than waiting for the full `send_timeout`.
async fn dispatch_event_with_timeout_with_cx(
    cx: &crate::cx::Cx,
    event_tx: &mpsc::Sender<NativeEvent>,
    event: NativeEvent,
    send_timeout: Duration,
) -> EventDispatchOutcome {
    match crate::runtime_async::timeout_with_cx(cx, send_timeout, event_tx.reserve(cx)).await {
        Ok(Ok(permit)) => {
            permit.send(event);
            EventDispatchOutcome::Sent
        }
        Ok(Err(_)) => EventDispatchOutcome::Closed,
        // `timeout_with_cx` Err surfaces as Backpressure to match
        // the legacy semantics — at this layer we can't
        // distinguish "queue full too long" from "parent cx
        // cancelled", and both warrant dropping the event.
        Err(_) => EventDispatchOutcome::Backpressure,
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

#[cfg(all(test, any(unix, windows), feature = "native-events-inline-tests"))]
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
            let result = crate::runtime_async::timeout(Duration::from_secs(2), handle).await;
            assert!(result.is_ok(), "native event listener did not shut down");
            assert!(
                !socket_path.exists(),
                "native event transport path should be removed after shutdown"
            );
        });
    }
}

#[cfg(all(test, unix, feature = "native-events-inline-tests"))]
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
                Some("pre-cancel native_events bind"),
            );

            let result = NativeEventListener::bind_with_cx(&cx, socket_path.clone()).await;
            match result {
                Err(NativeEventError::Io(err)) => {
                    assert_eq!(err.kind(), std::io::ErrorKind::Interrupted);
                    assert!(
                        err.to_string().contains("cancelled"),
                        "error should mention cancellation: {err}"
                    );
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
            let _active_listener = event_socket::bind(&socket_path)
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
                .await;

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
            let _ = handle.await;
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
            let _ = handle.await;
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
            let _ = handle.await;
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
            let _ = handle.await;
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
            let _ = handle.await;
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
            let result = crate::runtime_async::timeout(Duration::from_secs(2), handle).await;
            assert!(result.is_ok(), "listener did not shut down in time");
            #[cfg(unix)]
            assert!(
                !socket_path.exists(),
                "socket path should be removed after listener shutdown"
            );
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

            // Pre-cancelled cx causes channel reserve to fail (Closed) or
            // timeout to fire (Backpressure) — either way the event is dropped.
            assert!(
                matches!(
                    outcome,
                    EventDispatchOutcome::Backpressure | EventDispatchOutcome::Closed
                ),
                "pre-cancelled cx should drop the event, got {outcome:?}"
            );
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
            NativeEventError::SocketAlreadyExists("x".into()),
            NativeEventError::Io(std::io::Error::other("test")),
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
            let _ = handle.await;
        });
    }
}
