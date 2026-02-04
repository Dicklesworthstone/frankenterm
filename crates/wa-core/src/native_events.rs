//! Native event listener for vendored WezTerm integrations.
//!
//! Listens on a Unix domain socket for newline-delimited JSON events emitted by
//! a vendored WezTerm build (feature-gated on the WezTerm side).

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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

        if socket_path.exists() {
            return Err(NativeEventError::SocketAlreadyExists(
                socket_path.display().to_string(),
            ));
        }

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

            match tokio::time::timeout(ACCEPT_POLL_INTERVAL, self.listener.accept()).await {
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
                Err(_) => {
                    // timeout, loop to check shutdown flag
                }
            }
        }
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
        let payload = r#"{"type":"user_var","pane_id":3,"name":"WA_EVENT","value":"abc","ts":789}"#;
        let event = decode_wire_event(payload).unwrap().unwrap();
        match event {
            NativeEvent::UserVarChanged {
                pane_id,
                name,
                value,
                timestamp_ms,
            } => {
                assert_eq!(pane_id, 3);
                assert_eq!(name, "WA_EVENT");
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

        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
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
}
