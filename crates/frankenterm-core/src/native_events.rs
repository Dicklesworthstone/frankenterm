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

#[derive(Debug, Clone)]
pub struct NativePaneState {
    pub title: String,
    pub rows: u16,
    pub cols: u16,
    pub is_alt_screen: bool,
    pub cursor_row: u32,
    pub cursor_col: u32,
}

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

pub struct NativeEventListener {
    socket_path: PathBuf,
    listener: UnixListener,
}

impl NativeEventListener {
    pub async fn bind(socket_path: PathBuf) -> Result<Self, NativeEventError> {
        if socket_path.as_os_str().is_empty() {
            return Err(NativeEventError::EmptySocketPath);
        }

        maybe_cleanup_stale_socket(&socket_path)?;

        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = compat_unix::bind(&socket_path).await?;
        Ok(Self {
            socket_path,
            listener,
        })
    }

    pub async fn run(self, event_tx: mpsc::Sender<NativeEvent>, shutdown_flag: Arc<AtomicBool>) {
        let mut connection_tasks = JoinSet::new();

        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                break;
            }

            match crate::runtime_compat::timeout(ACCEPT_POLL_INTERVAL, self.listener.accept()).await
            {
                Ok(Ok((stream, _addr))) => {
                    let tx = event_tx.clone();
                    connection_tasks.spawn(async move {
                        if let Err(err) = handle_connection(stream, tx).await {
                            debug!(error = %err, "native event connection closed with error");
                        }
                    });
                }
                Ok(Err(err)) => {
                    warn!(error = %err, path = %self.socket_path.display(), "native event accept failed");
                }
                Err(_) => {} // timeout, loop to check shutdown flag
            }

            while let Some(join_result) = connection_tasks.try_join_next() {
                if let Err(err) = join_result {
                    debug!(error = %err, "native event connection task failed");
                }
            }
        }

        while let Some(join_result) = connection_tasks.join_next().await {
            if let Err(err) = join_result {
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

async fn handle_connection(
    stream: UnixStream,
    event_tx: mpsc::Sender<NativeEvent>,
) -> Result<(), std::io::Error> {
    debug!("native event connection accepted");
    let mut lines = compat_unix::lines(compat_unix::buffered(stream));

    while let Some(line) = compat_unix::next_line(&mut lines).await? {
        if line.len() > MAX_EVENT_LINE_BYTES {
            warn!(len = line.len(), "native event line too large; dropping");
            continue;
        }

        match decode_wire_event(&line) {
            Ok(Some(event)) => {
                let (event_kind, pane_id) = event_metadata(&event);
                match dispatch_event(&event_tx, event).await {
                    EventDispatchOutcome::Sent => {
                        debug!(event_kind, pane_id, "native event dispatched");
                    }
                    EventDispatchOutcome::Backpressure => {
                        debug!(
                            event_kind,
                            pane_id, "native event queue full; dropping event"
                        );
                    }
                    EventDispatchOutcome::Closed => {
                        debug!(event_kind, pane_id, "native event channel closed");
                        break;
                    }
                }
            }
            Ok(None) => {}
            Err(err) => {
                debug!(error = %err, "failed to decode native event");
            }
        }
    }

    debug!("native event connection closed");
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

async fn dispatch_event(
    event_tx: &mpsc::Sender<NativeEvent>,
    event: NativeEvent,
) -> EventDispatchOutcome {
    dispatch_event_with_timeout(event_tx, event, EVENT_SEND_TIMEOUT).await
}

async fn dispatch_event_with_timeout(
    event_tx: &mpsc::Sender<NativeEvent>,
    event: NativeEvent,
    send_timeout: Duration,
) -> EventDispatchOutcome {
    #[cfg(feature = "asupersync-runtime")]
    {
        let reserve_cx = crate::cx::for_testing();
        match crate::runtime_compat::timeout(send_timeout, event_tx.reserve(&reserve_cx)).await {
            Ok(Ok(permit)) => {
                permit.send(event);
                EventDispatchOutcome::Sent
            }
            Ok(Err(_)) => EventDispatchOutcome::Closed,
            Err(_) => EventDispatchOutcome::Backpressure,
        }
    }

    #[cfg(not(feature = "asupersync-runtime"))]
    {
        match crate::runtime_compat::timeout(send_timeout, event_tx.reserve()).await {
            Ok(Ok(permit)) => {
                permit.send(event);
                EventDispatchOutcome::Sent
            }
            Ok(Err(_)) => EventDispatchOutcome::Closed,
            Err(_) => EventDispatchOutcome::Backpressure,
        }
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

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for native events tests");
        runtime.block_on(future);
    }

    async fn recv_next<T>(rx: &mut mpsc::Receiver<T>) -> Option<T> {
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = crate::cx::for_testing();
            rx.recv(&cx).await.ok()
        }

        #[cfg(not(feature = "asupersync-runtime"))]
        {
            rx.recv().await
        }
    }

    async fn send_value<T>(tx: &mpsc::Sender<T>, value: T) -> Result<(), mpsc::SendError<T>> {
        #[cfg(feature = "asupersync-runtime")]
        {
            let cx = crate::cx::for_testing();
            tx.send(&cx, value).await
        }

        #[cfg(not(feature = "asupersync-runtime"))]
        {
            tx.send(value).await
        }
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
            let stale_listener = compat_unix::bind(&socket_path)
                .await
                .expect("bind stale socket");
            drop(stale_listener);
            assert!(
                socket_path.exists(),
                "socket path should persist after listener drop"
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
            NativeEventError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test")),
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
}
