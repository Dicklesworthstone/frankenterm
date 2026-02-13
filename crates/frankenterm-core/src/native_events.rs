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

use base64::Engine as _;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{debug, warn};

const MAX_EVENT_LINE_BYTES: usize = 512 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(250);

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

#[derive(Debug, Deserialize)]
struct WirePaneState {
    #[serde(default)]
    title: String,
    #[serde(default)]
    rows: u16,
    #[serde(default)]
    cols: u16,
    #[serde(default)]
    is_alt_screen: bool,
    #[serde(default)]
    cursor_row: u32,
    #[serde(default)]
    cursor_col: u32,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
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

        let listener = UnixListener::bind(&socket_path)?;
        Ok(Self {
            socket_path,
            listener,
        })
    }

    pub async fn run(self, event_tx: mpsc::Sender<NativeEvent>, shutdown_flag: Arc<AtomicBool>) {
        loop {
            if shutdown_flag.load(Ordering::SeqCst) {
                break;
            }

            match crate::runtime_compat::timeout(ACCEPT_POLL_INTERVAL, self.listener.accept()).await
            {
                Ok(Ok((stream, _addr))) => {
                    let tx = event_tx.clone();
                    tokio::spawn(async move {
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
    stream: tokio::net::UnixStream,
    event_tx: mpsc::Sender<NativeEvent>,
) -> Result<(), std::io::Error> {
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        if line.len() > MAX_EVENT_LINE_BYTES {
            warn!(len = line.len(), "native event line too large; dropping");
            continue;
        }

        match decode_wire_event(&line) {
            Ok(Some(event)) => {
                if event_tx.try_send(event).is_err() {
                    debug!("native event queue full; dropping event");
                }
            }
            Ok(None) => {}
            Err(err) => {
                debug!(error = %err, "failed to decode native event");
            }
        }
    }

    Ok(())
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
    use std::sync::atomic::AtomicBool;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

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

    // ── NativeEventListener ────────────────────────────────────────

    #[tokio::test]
    async fn bind_empty_path_returns_error() {
        let result = NativeEventListener::bind(PathBuf::from("")).await;
        assert!(result.is_err());
        match result {
            Err(NativeEventError::EmptySocketPath) => {}
            Err(other) => panic!("expected EmptySocketPath, got: {other}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn bind_existing_regular_file_returns_error() {
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
    }

    #[tokio::test]
    async fn bind_active_socket_returns_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("active.sock");
        let _active_listener = UnixListener::bind(&socket_path).expect("bind active socket");

        let result = NativeEventListener::bind(socket_path).await;
        assert!(result.is_err());
        match result {
            Err(NativeEventError::SocketAlreadyExists(_)) => {}
            Err(other) => panic!("expected SocketAlreadyExists, got: {other}"),
            Ok(_) => panic!("expected error"),
        }
    }

    #[tokio::test]
    async fn bind_replaces_stale_socket_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("stale.sock");
        let stale_listener = UnixListener::bind(&socket_path).expect("bind stale socket");
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
    }

    #[tokio::test]
    async fn listener_drop_removes_socket_file() {
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
    }

    #[tokio::test]
    async fn bind_creates_parent_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("sub").join("dir").join("deep.sock");
        let result = NativeEventListener::bind(socket_path).await;
        assert!(result.is_ok());
    }

    // ── Integration: listener + multiple events ────────────────────

    #[tokio::test]
    async fn listener_emits_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("native.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = tokio::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        let mut stream = UnixStream::connect(socket_path).await.expect("connect");
        let payload = r#"{"type":"pane_output","pane_id":7,"data_b64":"aGV5","ts":42}"#;
        stream
            .write_all(format!("{payload}\n").as_bytes())
            .await
            .expect("write");

        let event = crate::runtime_compat::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("event");

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

        shutdown.store(true, Ordering::SeqCst);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn listener_handles_multiple_events_on_one_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("multi.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = tokio::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        let mut stream = UnixStream::connect(socket_path).await.expect("connect");

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
        let ev1 = crate::runtime_compat::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("event 1");
        assert!(matches!(ev1, NativeEvent::PaneCreated { pane_id: 1, .. }));

        let ev2 = crate::runtime_compat::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("event 2");
        assert!(matches!(ev2, NativeEvent::PaneDestroyed { pane_id: 1, .. }));

        shutdown.store(true, Ordering::SeqCst);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn listener_skips_invalid_json_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("invalid.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let shutdown = Arc::new(AtomicBool::new(false));

        let handle = tokio::spawn(listener.run(event_tx, Arc::clone(&shutdown)));

        let mut stream = UnixStream::connect(socket_path).await.expect("connect");

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
        let event = crate::runtime_compat::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("timeout")
            .expect("event");
        assert!(matches!(
            event,
            NativeEvent::PaneDestroyed {
                pane_id: 42,
                timestamp_ms: 999
            }
        ));

        shutdown.store(true, Ordering::SeqCst);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn shutdown_flag_stops_listener() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("shutdown.sock");
        let listener = NativeEventListener::bind(socket_path.clone())
            .await
            .expect("bind listener");
        let (event_tx, _event_rx) = mpsc::channel(8);
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_clone = Arc::clone(&shutdown);
        let handle = tokio::spawn(listener.run(event_tx, shutdown_clone));

        // Set shutdown flag
        shutdown.store(true, Ordering::SeqCst);

        // Listener should exit within a few poll intervals
        let result = crate::runtime_compat::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "listener did not shut down in time");
        assert!(
            !socket_path.exists(),
            "socket path should be removed after listener shutdown"
        );
    }
}
