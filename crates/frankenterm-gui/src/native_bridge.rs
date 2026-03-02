//! Native event bridge — emitter side.
//!
//! Connects to the ft watch daemon's Unix domain socket and pushes
//! mux events as newline-delimited JSON [`WireEvent`] messages.
//! This replaces the polling-based capture loop with real-time push.

use base64::Engine as _;
use frankenterm_core::native_events::{WireEvent, WirePaneState};
use mux::pane::{CachePolicy, PaneId};
use mux::{Mux, MuxNotification};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Channel capacity for the bounded event queue.
/// Events are dropped when the channel is full (backpressure).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// How long the sender thread waits for events before checking shutdown.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// Reconnect backoff parameters.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A mux-to-wire event that's ready to serialize.
enum BridgeEvent {
    PaneOutput { pane_id: u64, data: Vec<u8> },
    StateChange { pane_id: u64, state: WirePaneState },
    UserVar { pane_id: u64, name: String, value: String },
    PaneCreated { pane_id: u64, domain: String, cwd: Option<String> },
    PaneDestroyed { pane_id: u64 },
}

impl BridgeEvent {
    fn into_wire_event(self) -> WireEvent {
        let ts = now_ms();
        match self {
            BridgeEvent::PaneOutput { pane_id, data } => WireEvent::PaneOutput {
                pane_id,
                data_b64: base64::engine::general_purpose::STANDARD.encode(&data),
                ts,
            },
            BridgeEvent::StateChange { pane_id, state } => WireEvent::StateChange {
                pane_id,
                state,
                ts,
            },
            BridgeEvent::UserVar {
                pane_id,
                name,
                value,
            } => WireEvent::UserVar {
                pane_id,
                name,
                value,
                ts,
            },
            BridgeEvent::PaneCreated {
                pane_id,
                domain,
                cwd,
            } => WireEvent::PaneCreated {
                pane_id,
                domain,
                cwd,
                ts,
            },
            BridgeEvent::PaneDestroyed { pane_id } => WireEvent::PaneDestroyed { pane_id, ts },
        }
    }
}

/// The native event bridge. Owns the sender thread and mux subscription.
pub struct NativeEventBridge {
    shutdown: Arc<AtomicBool>,
    _sender_thread: std::thread::JoinHandle<()>,
    _subscription_id: usize,
}

impl NativeEventBridge {
    /// Start the native event bridge.
    ///
    /// Connects to the socket at `socket_path`, subscribes to mux events,
    /// and forwards them as newline-delimited JSON.
    ///
    /// Returns `None` if the socket path doesn't exist or can't be connected to
    /// (graceful degradation when ft watch is not running).
    pub fn start(socket_path: &Path) -> Option<Self> {
        let socket_path = socket_path.to_path_buf();

        // Try initial connection to verify the socket exists
        match UnixStream::connect(&socket_path) {
            Ok(stream) => {
                drop(stream);
                log::info!(
                    "Native event bridge: socket found at {}",
                    socket_path.display()
                );
            }
            Err(e) => {
                log::info!(
                    "Native event bridge: socket not available at {} ({}), \
                     running without native events",
                    socket_path.display(),
                    e
                );
                return None;
            }
        }

        let (tx, rx) = std_mpsc::sync_channel::<BridgeEvent>(EVENT_CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // Sender thread: connects to socket, writes events
        let sender_thread = std::thread::Builder::new()
            .name("native-event-bridge".into())
            .spawn(move || {
                sender_loop(&socket_path, rx, &shutdown_clone);
            })
            .expect("failed to spawn native-event-bridge thread");

        // Subscribe to mux notifications
        let subscription_id = {
            let mux = Mux::get();
            let tx_clone = tx.clone();
            mux.subscribe(move |notification| {
                handle_mux_notification(&notification, &tx_clone);
                true // keep listening
            })
        };

        Some(Self {
            shutdown,
            _sender_thread: sender_thread,
            _subscription_id: subscription_id,
        })
    }
}

impl Drop for NativeEventBridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // The sender thread will exit on next recv timeout
        // The mux subscription will be cleaned up when Mux shuts down
    }
}

/// Background thread that reads events from the channel and writes to the socket.
fn sender_loop(
    socket_path: &Path,
    rx: std_mpsc::Receiver<BridgeEvent>,
    shutdown: &AtomicBool,
) {
    let mut backoff = INITIAL_BACKOFF;
    let mut stream: Option<UnixStream> = None;

    // Send Hello on first connect
    let mut sent_hello = false;

    while !shutdown.load(Ordering::Acquire) {
        // Ensure we have a connection
        if stream.is_none() {
            match UnixStream::connect(socket_path) {
                Ok(s) => {
                    log::debug!("Native event bridge: connected to {}", socket_path.display());
                    stream = Some(s);
                    backoff = INITIAL_BACKOFF;
                    sent_hello = false;
                }
                Err(e) => {
                    log::debug!(
                        "Native event bridge: connect failed ({}), backoff {:?}",
                        e,
                        backoff
                    );
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        }

        // Send Hello handshake if needed
        if !sent_hello {
            if let Some(ref mut s) = stream {
                let hello = WireEvent::Hello {
                    proto: Some(1),
                    wezterm_version: Some(concat!("FrankenTerm ", env!("CARGO_PKG_VERSION")).into()),
                    ts: Some(now_ms()),
                };
                if write_event(s, &hello).is_err() {
                    log::warn!("Native event bridge: failed to send Hello, reconnecting");
                    stream = None;
                    continue;
                }
                sent_hello = true;
            }
        }

        // Wait for an event from the channel
        match rx.recv_timeout(RECV_TIMEOUT) {
            Ok(event) => {
                let wire = event.into_wire_event();
                if let Some(ref mut s) = stream {
                    if write_event(s, &wire).is_err() {
                        log::warn!("Native event bridge: write failed, reconnecting");
                        stream = None;
                    }
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Normal: just loop back and check shutdown
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                log::info!("Native event bridge: channel closed, shutting down");
                break;
            }
        }
    }

    log::debug!("Native event bridge: sender thread exiting");
}

/// Write a single WireEvent as a JSON line to the stream.
fn write_event(stream: &mut UnixStream, event: &WireEvent) -> Result<(), std::io::Error> {
    let json = serde_json::to_string(event).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, e)
    })?;
    stream.write_all(json.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Convert a MuxNotification into a BridgeEvent and send it.
fn handle_mux_notification(
    notification: &MuxNotification,
    tx: &std_mpsc::SyncSender<BridgeEvent>,
) {
    let event = match notification {
        MuxNotification::PaneOutput(pane_id) => {
            // PaneOutput only gives us the pane ID — we need to read the output.
            // For now, emit a state change notification since reading output
            // from here is complex (requires async pane access).
            build_state_change_event(*pane_id)
        }

        MuxNotification::PaneAdded(pane_id) => {
            let mux = Mux::get();
            let (domain, cwd) = if let Some(pane) = mux.get_pane(*pane_id) {
                let domain_id = pane.domain_id();
                let domain_name = mux
                    .get_domain(domain_id)
                    .map(|d| d.domain_name().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                let cwd = pane
                    .get_current_working_dir(CachePolicy::AllowStale)
                    .map(|url| url.path().to_string());
                (domain_name, cwd)
            } else {
                ("unknown".to_string(), None)
            };
            Some(BridgeEvent::PaneCreated {
                pane_id: *pane_id as u64,
                domain,
                cwd,
            })
        }

        MuxNotification::PaneRemoved(pane_id) => Some(BridgeEvent::PaneDestroyed {
            pane_id: *pane_id as u64,
        }),

        MuxNotification::TabTitleChanged { tab_id, .. } => {
            // Title change → emit state change for all panes in the tab
            let mux = Mux::get();
            if let Some(tab) = mux.get_tab(*tab_id) {
                if let Some(pane) = tab.get_active_pane() {
                    return emit_state_change(pane.pane_id(), tx);
                }
            }
            None
        }

        MuxNotification::Alert {
            pane_id,
            alert: wezterm_term::Alert::CurrentWorkingDirectoryChanged,
        } => build_state_change_event(*pane_id),

        MuxNotification::Alert {
            pane_id,
            alert: wezterm_term::Alert::SetUserVar { name, value },
        } => Some(BridgeEvent::UserVar {
            pane_id: *pane_id as u64,
            name: name.clone(),
            value: value.clone(),
        }),

        // Ignore other notifications for now
        _ => None,
    };

    if let Some(event) = event {
        // Try to send; drop if channel is full (backpressure)
        let _ = tx.try_send(event);
    }
}

fn build_state_change_event(pane_id: PaneId) -> Option<BridgeEvent> {
    let mux = Mux::get();
    let pane = mux.get_pane(pane_id)?;
    let dims = pane.get_dimensions();
    let cursor = pane.get_cursor_position();

    Some(BridgeEvent::StateChange {
        pane_id: pane_id as u64,
        state: WirePaneState {
            title: pane.get_title(),
            rows: dims.viewport_rows as u16,
            cols: dims.cols as u16,
            is_alt_screen: pane.is_alt_screen_active(),
            cursor_row: cursor.y as u32,
            cursor_col: cursor.x as u32,
        },
    })
}

fn emit_state_change(pane_id: PaneId, tx: &std_mpsc::SyncSender<BridgeEvent>) {
    if let Some(event) = build_state_change_event(pane_id) {
        let _ = tx.try_send(event);
    }
}
