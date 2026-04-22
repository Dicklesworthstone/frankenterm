//! Native event listener for vendored WezTerm integrations.
//!
//! Listens on a Unix domain socket for newline-delimited JSON events emitted by
//! a vendored WezTerm build (feature-gated on the WezTerm side).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;

use crate::runtime_compat::mpsc;
use crate::runtime_compat::task::JoinSet;
use crate::runtime_compat::unix::{self as compat_unix, UnixListener, UnixStream};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

const MAX_EVENT_LINE_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const EVENT_SEND_TIMEOUT: Duration = Duration::from_millis(25);

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

/// Unix socket server that receives pane events from the frankenterm GUI process.
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
        let listener = compat_unix::bind(&socket_path).await?;
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
    /// [`crate::runtime_compat::timeout_with_cx`], so budget-driven
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

            match crate::runtime_compat::timeout_with_cx(
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
    #[cfg(not(unix))]
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

    #[cfg(not(unix))]
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
    let mut lines = compat_unix::lines(compat_unix::buffered(stream));

    while let Some(line) = compat_unix::next_line_with_cx(&cx, &mut lines).await? {
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
                        debug!(
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

#[cfg(test)]
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
    match crate::runtime_compat::timeout_with_cx(cx, send_timeout, event_tx.reserve(cx)).await {
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
            let bounded = if decoded.len() > MAX_OUTPUT_BYTES {
                decoded[..MAX_OUTPUT_BYTES].to_vec()
            } else {
                decoded
            };
            Ok(Some(NativeEvent::PaneOutput {
                pane_id,
                data: bounded,
                timestamp_ms: ts(ts_ms),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_compat::task;
    use crate::runtime_compat::unix::{self as compat_unix, AsyncWriteExt};
    use crate::runtime_compat::{CompatRuntime, RuntimeBuilder};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn decode_pane_output_event() {
        let payload = r#"{"type":"pane_output","pane_id":1,"data_b64":"aGVsbG8=","ts":123}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::PaneOutput {
                pane_id,
                data,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 1);
                assert_eq!(data, b"hello");
                assert_eq!(timestamp_ms, 123);
            }
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
            _ => panic!("wrong event type"),
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
    #[test]
    fn dispatch_event_runs_deterministically_under_labruntime() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const SEED: u64 = 0xF734_5E42_A11D_0515;
        const EVENT_COUNT: usize = 8;

        let wall_start = std::time::Instant::now();
        let delivered = Arc::new(AtomicUsize::new(0));
        let delivered_task = Arc::clone(&delivered);

        let mut runtime = asupersync::LabRuntime::new(
            asupersync::LabConfig::new(SEED)
                .with_auto_advance()
                .worker_count(1)
                .max_steps(50_000),
        );
        let region = runtime
            .state
            .create_root_region(asupersync::Budget::INFINITE);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, asupersync::Budget::INFINITE, async move {
                let (tx, mut rx) = mpsc::channel::<NativeEvent>(EVENT_COUNT);
                for i in 0..EVENT_COUNT {
                    let event = NativeEvent::PaneDestroyed {
                        pane_id: i as u64,
                        timestamp_ms: i as i64,
                    };
                    match dispatch_event_with_timeout(&tx, event, Duration::from_millis(50)).await {
                        EventDispatchOutcome::Sent => {}
                        other => panic!("expected Sent, got {other:?}"),
                    }
                }
                drop(tx);
                let mut observed = 0usize;
                while let Some(_evt) = recv_next(&mut rx).await {
                    observed += 1;
                }
                delivered_task.store(observed, Ordering::SeqCst);
            })
            .expect("spawn native-events dispatch task");
        runtime.scheduler.lock().schedule(task_id, 0);
        runtime.step_for_test();
        let _ = runtime.run_with_auto_advance();
        let report = runtime.run_until_quiescent_with_report();

        assert_eq!(
            delivered.load(Ordering::SeqCst),
            EVENT_COUNT,
            "all dispatched events must be observable under LabRuntime"
        );
        assert!(
            report.oracle_report.all_passed(),
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
            crate::runtime_compat::clear_runtime_handle();
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
        crate::runtime_compat::timeout(timeout, recv_next(event_rx))
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
                Err(other) => panic!("expected EmptySocketPath, got: {other}"),
                Ok(_) => panic!("expected error"),
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
                Err(e) => panic!("bind_with_cx should succeed on fresh cx: {e}"),
            }
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
                Err(other) => panic!("expected Io(Interrupted), got: {other}"),
                Ok(_) => panic!("bind_with_cx should fail on cancelled cx"),
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
                Err(other) => panic!("expected SocketAlreadyExists, got: {other}"),
                Ok(_) => panic!("expected error"),
            }
        });
    }

    #[test]
    fn bind_active_socket_returns_error() {
        run_async_test(async {
            let dir = tempfile::tempdir().expect("tempdir");
            let socket_path = dir.path().join("active.sock");
            let _active_listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind active socket");

            let result = NativeEventListener::bind(socket_path).await;
            assert!(result.is_err());
            match result {
                Err(NativeEventError::SocketAlreadyExists(_)) => {}
                Err(other) => panic!("expected SocketAlreadyExists, got: {other}"),
                Ok(_) => panic!("expected error"),
            }
        });
    }

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

            let mut stream = compat_unix::connect(socket_path).await.expect("connect");
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
                } => {
                    assert_eq!(pane_id, 7);
                    assert_eq!(data, b"hey");
                    assert_eq!(timestamp_ms, 42);
                }
                _ => panic!("unexpected event type"),
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

            let mut stream = compat_unix::connect(socket_path).await.expect("connect");

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

            let mut stream = compat_unix::connect(socket_path).await.expect("connect");

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
            let mut stream_one = compat_unix::connect(socket_path.clone())
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
            let mut stream_two = compat_unix::connect(socket_path)
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

            let mut stream = compat_unix::connect(socket_path).await.expect("connect");
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
            let result = crate::runtime_compat::timeout(Duration::from_secs(2), handle).await;
            assert!(result.is_ok(), "listener did not shut down in time");
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
        assert!(MAX_EVENT_LINE_BYTES > 0);
        assert!(MAX_OUTPUT_BYTES > 0);
        assert!(!ACCEPT_POLL_INTERVAL.is_zero());
        assert!(!EVENT_SEND_TIMEOUT.is_zero());
    }

    #[test]
    fn output_bytes_less_than_line_bytes() {
        assert!(
            MAX_OUTPUT_BYTES < MAX_EVENT_LINE_BYTES,
            "MAX_OUTPUT_BYTES should be less than MAX_EVENT_LINE_BYTES"
        );
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
            _ => panic!("wrong variant after roundtrip"),
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
            _ => panic!("wrong variant after roundtrip"),
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
            _ => panic!("wrong variant after roundtrip"),
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
            _ => panic!("wrong variant after roundtrip"),
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
            _ => panic!("wrong variant after roundtrip"),
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
            _ => panic!("wrong variant after roundtrip"),
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
            _ => panic!("wrong variant after roundtrip"),
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

            let mut stream = compat_unix::connect(socket_path).await.expect("connect");

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
                match crate::runtime_compat::timeout(deadline, recv_next(&mut event_rx)).await {
                    Ok(Some(_)) => received += 1,
                    Ok(None) => break,
                    Err(elapsed) => {
                        panic!("timeout after {elapsed}: received {received}/{event_count} events");
                    }
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
