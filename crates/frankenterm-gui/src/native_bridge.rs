//! Native event bridge — emitter side.
//!
//! Connects to the ft watch daemon's authenticated Unix-domain socket and
//! pushes best-effort mux state/lifecycle hints as newline-delimited JSON
//! [`WireEvent`] messages. Polling remains authoritative for pane-output text
//! because mux output notifications do not carry raw PTY bytes.

#![cfg(all(
    unix,
    any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "dragonfly"
    )
))]

use frankenterm_core::native_events::{
    NativeEventError, WireEvent, WirePaneState, validate_native_event_peer,
    validate_native_event_socket_endpoint,
};
use frankenterm_gui::terminal_pane_id_to_u64;
use mux::pane::CachePolicy;
use mux::{Mux, MuxNotification};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Channel capacity for the bounded event queue.
/// Events are dropped when the channel is full (backpressure).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// How long the sender thread waits for events before checking shutdown.
const RECV_TIMEOUT: Duration = Duration::from_millis(250);

/// Reconnect backoff parameters.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const SOCKET_WRITE_RETRY_INTERVAL: Duration = Duration::from_millis(1);
const INITIAL_SERIALIZED_EVENT_CAPACITY: usize = 1024;
const MAX_BRIDGE_TEXT_FIELD_BYTES: usize = 16 * 1024;
static BRIDGE_QUEUE_FULL_DROPS: AtomicU64 = AtomicU64::new(0);
static BRIDGE_OVERSIZED_FIELD_DROPS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// A mux-to-wire event that's ready to serialize.
enum BridgeEvent {
    StateChange {
        pane_id: u64,
        state: WirePaneState,
        timestamp_ms: u64,
    },
    UserVar {
        pane_id: u64,
        name: String,
        value: String,
        timestamp_ms: u64,
    },
    PaneCreated {
        pane_id: u64,
        domain: String,
        cwd: Option<String>,
        timestamp_ms: u64,
    },
    PaneDestroyed {
        pane_id: u64,
        timestamp_ms: u64,
    },
}

impl BridgeEvent {
    fn metadata(&self) -> (&'static str, u64) {
        match self {
            Self::StateChange { pane_id, .. } => ("state_change", *pane_id),
            Self::UserVar { pane_id, .. } => ("user_var", *pane_id),
            Self::PaneCreated { pane_id, .. } => ("pane_created", *pane_id),
            Self::PaneDestroyed { pane_id, .. } => ("pane_destroyed", *pane_id),
        }
    }

    fn into_wire_event(self) -> WireEvent {
        match self {
            BridgeEvent::StateChange {
                pane_id,
                state,
                timestamp_ms,
            } => WireEvent::StateChange {
                pane_id,
                state,
                ts: timestamp_ms,
            },
            BridgeEvent::UserVar {
                pane_id,
                name,
                value,
                timestamp_ms,
            } => WireEvent::UserVar {
                pane_id,
                name,
                value,
                ts: timestamp_ms,
            },
            BridgeEvent::PaneCreated {
                pane_id,
                domain,
                cwd,
                timestamp_ms,
            } => WireEvent::PaneCreated {
                pane_id,
                domain,
                cwd,
                ts: timestamp_ms,
            },
            BridgeEvent::PaneDestroyed {
                pane_id,
                timestamp_ms,
            } => WireEvent::PaneDestroyed {
                pane_id,
                ts: timestamp_ms,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeQueueOutcome {
    Queued,
    DroppedFull,
    Closed,
}

impl BridgeQueueOutcome {
    fn keeps_subscription(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

fn enqueue_bridge_event(
    tx: &std_mpsc::SyncSender<BridgeEvent>,
    event: BridgeEvent,
) -> BridgeQueueOutcome {
    let (event_kind, pane_id) = event.metadata();
    match tx.try_send(event) {
        Ok(()) => BridgeQueueOutcome::Queued,
        Err(std_mpsc::TrySendError::Full(_)) => {
            let previous = match BRIDGE_QUEUE_FULL_DROPS.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |count| Some(count.saturating_add(1)),
            ) {
                Ok(previous) | Err(previous) => previous,
            };
            let drop_count = previous.saturating_add(1);
            // Pane-output storms must not turn a bounded data queue into an
            // unbounded warning stream. Powers-of-two sampling stays visible
            // while adding only logarithmic log volume.
            if drop_count.is_power_of_two() {
                log::warn!(
                    "Native event bridge: bounded queue full; dropped event_kind={} pane_id={} delivery_gap=known cumulative_queue_drops={}",
                    event_kind,
                    pane_id,
                    drop_count
                );
            }
            BridgeQueueOutcome::DroppedFull
        }
        Err(std_mpsc::TrySendError::Disconnected(_)) => {
            log::warn!(
                "Native event bridge: sender channel closed; dropped event_kind={} pane_id={} delivery_gap=known",
                event_kind,
                pane_id
            );
            BridgeQueueOutcome::Closed
        }
    }
}

fn bridge_text_field_allowed(
    event_kind: &'static str,
    pane_id: u64,
    field: &'static str,
    value: &str,
) -> bool {
    if value.len() <= MAX_BRIDGE_TEXT_FIELD_BYTES {
        return true;
    }

    let previous = match BRIDGE_OVERSIZED_FIELD_DROPS.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |count| Some(count.saturating_add(1)),
    ) {
        Ok(previous) | Err(previous) => previous,
    };
    let drop_count = previous.saturating_add(1);
    if drop_count.is_power_of_two() {
        log::warn!(
            "Native event bridge: oversized text field dropped; event_kind={} pane_id={} field={} field_bytes={} max_field_bytes={} delivery_gap=known cumulative_oversized_drops={}",
            event_kind,
            pane_id,
            field,
            value.len(),
            MAX_BRIDGE_TEXT_FIELD_BYTES,
            drop_count
        );
    }
    false
}

/// The native event bridge. Owns the sender thread and mux subscription.
pub struct NativeEventBridge {
    shutdown: Arc<AtomicBool>,
    sender_thread: Option<std::thread::JoinHandle<()>>,
    mux: Option<Arc<Mux>>,
    subscription_id: Option<usize>,
}

impl NativeEventBridge {
    /// Start the native event bridge.
    ///
    /// Subscribes to mux events immediately and starts a bounded reconnect loop
    /// for `socket_path`. The endpoint need not exist yet; each eventual connect
    /// revalidates endpoint metadata and peer credentials before sending.
    /// Returns `None` only if local thread/subscription setup fails.
    pub fn start(socket_path: &Path) -> Option<Self> {
        let socket_path = socket_path.to_path_buf();

        let (tx, rx) = std_mpsc::sync_channel::<BridgeEvent>(EVENT_CHANNEL_CAPACITY);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        // Sender thread: connects to socket, writes events
        let sender_thread = std::thread::Builder::new()
            .name("native-event-bridge".into())
            .spawn(move || {
                sender_loop(&socket_path, rx, &shutdown_clone);
            });
        let sender_thread = match sender_thread {
            Ok(thread) => thread,
            Err(err) => {
                log::warn!(
                    "Native event bridge: failed to spawn sender thread ({}), \
                     running without native events",
                    err
                );
                return None;
            }
        };

        // Subscribe to mux notifications
        let Some(mux) = Mux::try_get() else {
            log::warn!(
                "Native event bridge: mux singleton unavailable, running without native events"
            );
            shutdown.store(true, Ordering::Release);
            drop(tx);
            if sender_thread.join().is_err() {
                log::warn!("Native event bridge: sender thread panicked during shutdown");
            }
            return None;
        };
        let subscription_id = {
            let tx_clone = tx.clone();
            let shutdown_for_subscription = shutdown.clone();
            // `Mux` owns its subscriber callbacks, so capture a `Weak` identity
            // rather than creating Mux -> callback -> Mux reference cycle. The
            // bridge itself retains the strong originating mux until it first
            // unsubscribes during Drop.
            let originating_mux = Arc::downgrade(&mux);
            match mux.subscribe(move |notification| {
                if shutdown_for_subscription.load(Ordering::Acquire) {
                    return false;
                }
                let Some(originating_mux) = originating_mux_for_notification(&originating_mux)
                else {
                    return false;
                };
                handle_mux_notification(originating_mux.as_ref(), &notification, &tx_clone)
            }) {
                Ok(subscription_id) => subscription_id,
                Err(err) => {
                    log::warn!(
                        "Native event bridge: failed to allocate mux subscription ({err}), \
                         running without native events"
                    );
                    shutdown.store(true, Ordering::Release);
                    drop(tx);
                    if sender_thread.join().is_err() {
                        log::warn!("Native event bridge: sender thread panicked during shutdown");
                    }
                    return None;
                }
            }
        };

        Some(Self {
            shutdown,
            sender_thread: Some(sender_thread),
            mux: Some(mux),
            subscription_id: Some(subscription_id),
        })
    }
}

fn originating_mux_for_notification(originating_mux: &Weak<Mux>) -> Option<Arc<Mux>> {
    originating_mux.upgrade()
}

impl Drop for NativeEventBridge {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);

        if let (Some(mux), Some(subscription_id)) = (self.mux.take(), self.subscription_id.take()) {
            let _ = mux.unsubscribe(subscription_id);
        }

        if let Some(sender_thread) = self.sender_thread.take() {
            if sender_thread.join().is_err() {
                log::warn!("Native event bridge: sender thread panicked during shutdown");
            }
        }
    }
}

fn wait_for_retry_or_shutdown(delay: Duration, shutdown: &AtomicBool) -> bool {
    let Some(deadline) = Instant::now().checked_add(delay) else {
        log::warn!("native event bridge retry delay is too large for Instant");
        return false;
    };
    loop {
        if shutdown.load(Ordering::Acquire) {
            return false;
        }

        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return true;
        };

        if remaining.is_zero() {
            return true;
        }

        std::thread::sleep(remaining.min(SHUTDOWN_POLL_INTERVAL));
    }
}

fn connect_authenticated_native_event_socket(
    socket_path: &Path,
) -> Result<UnixStream, NativeEventError> {
    validate_native_event_socket_endpoint(socket_path)?;
    // A plain UnixStream::connect can block indefinitely when a local listener's
    // accept backlog wedges. The bridge's Drop implementation joins this sender
    // thread, so use socket2's safe nonblocking-connect + poll implementation and
    // retain a finite shutdown bound without detached connector threads.
    let address = socket2::SockAddr::unix(socket_path)?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&address, SOCKET_CONNECT_TIMEOUT)?;
    let stream: UnixStream = socket.into();
    validate_native_event_peer(&stream)?;
    // `SO_SNDTIMEO` bounds each write syscall rather than the complete
    // `write_all` operation. A peer making slow partial progress could
    // therefore hold the joined sender thread indefinitely. Keep the stream
    // nonblocking and enforce one end-to-end deadline in `write_event`.
    stream.set_nonblocking(true)?;
    Ok(stream)
}

/// Background thread that reads events from the channel and writes to the socket.
fn sender_loop(socket_path: &Path, rx: std_mpsc::Receiver<BridgeEvent>, shutdown: &AtomicBool) {
    let mut connect_backoff = INITIAL_BACKOFF;
    let mut write_failure_backoff = INITIAL_BACKOFF;
    let mut stream: Option<UnixStream> = None;
    let mut serialized_event = Vec::with_capacity(INITIAL_SERIALIZED_EVENT_CAPACITY);

    // Send Hello on first connect
    let mut sent_hello = false;

    while !shutdown.load(Ordering::Acquire) {
        // Ensure we have a connection
        if stream.is_none() {
            match connect_authenticated_native_event_socket(socket_path) {
                Ok(s) => {
                    log::info!(
                        "Native event bridge: authenticated socket connected at {}",
                        socket_path.display()
                    );
                    stream = Some(s);
                    sent_hello = false;
                }
                Err(e) => {
                    log::debug!(
                        "Native event bridge: connect failed ({}), backoff {:?}",
                        e,
                        connect_backoff
                    );
                    if !wait_for_retry_or_shutdown(connect_backoff, shutdown) {
                        break;
                    }
                    connect_backoff = (connect_backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            }
        }

        // Send Hello handshake if needed
        if !sent_hello {
            if let Some(ref mut s) = stream {
                let hello = WireEvent::Hello {
                    proto: Some(1),
                    wezterm_version: Some(
                        concat!("FrankenTerm ", env!("CARGO_PKG_VERSION")).into(),
                    ),
                    ts: Some(now_ms()),
                };
                if let Err(error) = write_event(s, &hello, &mut serialized_event, shutdown) {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    log::warn!(
                        "Native event bridge: failed to send Hello ({error}), reconnecting"
                    );
                    stream = None;
                    if !wait_for_retry_or_shutdown(connect_backoff, shutdown) {
                        break;
                    }
                    connect_backoff = (connect_backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
                sent_hello = true;
                // Do not call an accepted socket healthy until this side has
                // written one complete application-protocol frame.
                connect_backoff = INITIAL_BACKOFF;
            }
        }

        // Wait for an event from the channel
        match rx.recv_timeout(RECV_TIMEOUT) {
            Ok(event) => {
                let (event_kind, pane_id) = event.metadata();
                let wire = event.into_wire_event();
                if let Some(ref mut s) = stream {
                    if let Err(error) =
                        write_event(s, &wire, &mut serialized_event, shutdown)
                    {
                        if shutdown.load(Ordering::Acquire) {
                            break;
                        }
                        // A stream write may fail after some or all bytes reached
                        // the receiver. Retrying here could duplicate a mutation,
                        // while dropping it leaves a possible gap. Until the wire
                        // protocol has sequence/ack reconciliation, surface the
                        // outcome as indeterminate and reconnect without a blind
                        // replay.
                        log::warn!(
                            "Native event bridge: write outcome indeterminate; event_kind={} pane_id={} error={} reconnecting without blind retry",
                            event_kind,
                            pane_id,
                            error
                        );
                        stream = None;
                        if !wait_for_retry_or_shutdown(write_failure_backoff, shutdown) {
                            break;
                        }
                        write_failure_backoff =
                            (write_failure_backoff * 2).min(MAX_BACKOFF);
                    } else {
                        write_failure_backoff = INITIAL_BACKOFF;
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
fn write_event(
    stream: &mut UnixStream,
    event: &WireEvent,
    serialized_event: &mut Vec<u8>,
    shutdown: &AtomicBool,
) -> Result<(), std::io::Error> {
    if shutdown.load(Ordering::Acquire) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "native event bridge is shutting down",
        ));
    }

    let deadline = Instant::now()
        .checked_add(SOCKET_WRITE_TIMEOUT)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "native event write timeout exceeds Instant range",
            )
        })?;
    serialized_event.clear();
    serde_json::to_writer(&mut *serialized_event, event)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    serialized_event.push(b'\n');
    write_bytes_with_deadline(
        stream,
        serialized_event,
        shutdown,
        deadline,
        Instant::now,
        std::thread::sleep,
    )
}

fn write_bytes_with_deadline<W, Now, Sleep>(
    writer: &mut W,
    bytes: &[u8],
    shutdown: &AtomicBool,
    deadline: Instant,
    mut now: Now,
    mut sleep: Sleep,
) -> Result<(), std::io::Error>
where
    W: Write,
    Now: FnMut() -> Instant,
    Sleep: FnMut(Duration),
{
    let mut remaining = bytes;

    while !remaining.is_empty() {
        if shutdown.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "native event bridge is shutting down",
            ));
        }

        let Some(time_left) = deadline.checked_duration_since(now()) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "native event write exceeded its total deadline",
            ));
        };
        if time_left.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "native event write exceeded its total deadline",
            ));
        }

        match writer.write(remaining) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "native event socket accepted no bytes",
                ));
            }
            Ok(written) => remaining = &remaining[written..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) =>
            {
                sleep(time_left.min(SOCKET_WRITE_RETRY_INTERVAL));
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

/// Convert a MuxNotification into a BridgeEvent and send it.
fn handle_mux_notification(
    mux: &Mux,
    notification: &MuxNotification,
    tx: &std_mpsc::SyncSender<BridgeEvent>,
) -> bool {
    if matches!(notification, MuxNotification::PaneOutput(_)) {
        // PaneOutput carries no bytes or new state. Snapshotting title,
        // geometry, and cursor here cloned pane data on every PTY output
        // notification and amplified high-throughput sessions into GUI lag,
        // while polling still remained the only authoritative text source.
        // The future raw-byte/sequence bridge owns re-enabling this path.
        // The callback return value controls subscription liveness. Ignoring
        // this notification must keep the bridge subscribed for later
        // lifecycle and state notifications.
        return true;
    }
    if matches!(
        notification,
        MuxNotification::TabTitleChanged { .. }
            | MuxNotification::Alert {
                alert: wezterm_term::Alert::CurrentWorkingDirectoryChanged,
                ..
            }
    ) {
        // The receiver currently consumes only StateChange::is_alt_screen;
        // title, geometry, cursor, and cwd are ignored (cwd is not present in
        // WirePaneState at all). Polling and terminal parsing remain the
        // authoritative sources, so avoid cloning and serializing a misleading
        // full pane snapshot for these metadata notifications.
        return true;
    }

    let timestamp_ms = now_ms();
    let event = match notification {
        MuxNotification::PaneOutput(_) => None,

        MuxNotification::PaneAdded(pane_id) => {
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
            let wire_pane_id = terminal_pane_id_to_u64(*pane_id);
            if !bridge_text_field_allowed("pane_created", wire_pane_id, "domain", &domain)
                || cwd.as_deref().is_some_and(|cwd| {
                    !bridge_text_field_allowed("pane_created", wire_pane_id, "cwd", cwd)
                })
            {
                None
            } else {
                Some(BridgeEvent::PaneCreated {
                    pane_id: wire_pane_id,
                    domain,
                    cwd,
                    timestamp_ms,
                })
            }
        }

        MuxNotification::PaneRemoved(pane_id) => Some(BridgeEvent::PaneDestroyed {
            pane_id: terminal_pane_id_to_u64(*pane_id),
            timestamp_ms,
        }),

        MuxNotification::Alert {
            pane_id,
            alert: wezterm_term::Alert::SetUserVar { name, value },
        } => {
            let wire_pane_id = terminal_pane_id_to_u64(*pane_id);
            if !bridge_text_field_allowed("user_var", wire_pane_id, "name", name)
                || !bridge_text_field_allowed("user_var", wire_pane_id, "value", value)
            {
                None
            } else {
                Some(BridgeEvent::UserVar {
                    pane_id: wire_pane_id,
                    name: name.clone(),
                    value: value.clone(),
                    timestamp_ms,
                })
            }
        }

        // Ignore other notifications for now
        _ => None,
    };

    event.is_none_or(|event| enqueue_bridge_event(tx, event).keeps_subscription())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex as StdMutex, OnceLock};

    enum ScriptedWrite {
        Bytes(usize),
        Error(std::io::ErrorKind),
    }

    struct ScriptedWriter {
        steps: VecDeque<ScriptedWrite>,
        written: Vec<u8>,
        calls: usize,
    }

    impl std::io::Write for ScriptedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.calls = self.calls.saturating_add(1);
            match self.steps.pop_front().unwrap_or(ScriptedWrite::Error(
                std::io::ErrorKind::WouldBlock,
            )) {
                ScriptedWrite::Bytes(maximum) => {
                    let written = maximum.min(bytes.len());
                    self.written.extend_from_slice(&bytes[..written]);
                    Ok(written)
                }
                ScriptedWrite::Error(kind) => Err(std::io::Error::from(kind)),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn test_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    struct TestMuxGuard {
        _guard: std::sync::MutexGuard<'static, ()>,
        prior: Option<Arc<Mux>>,
    }

    impl TestMuxGuard {
        fn install(mux: Arc<Mux>) -> Self {
            let guard = test_lock().lock().expect("lock");
            let prior = Mux::try_get();
            Mux::set_mux(&mux);
            Self {
                _guard: guard,
                prior,
            }
        }
    }

    impl Drop for TestMuxGuard {
        fn drop(&mut self) {
            if let Some(prior) = self.prior.take() {
                Mux::set_mux(&prior);
            } else {
                Mux::shutdown();
            }
        }
    }

    fn test_state_event(pane_id: u64) -> BridgeEvent {
        BridgeEvent::StateChange {
            pane_id,
            state: WirePaneState {
                title: "test".to_string(),
                rows: 24,
                cols: 80,
                is_alt_screen: false,
                cursor_row: 0,
                cursor_col: 0,
            },
            timestamp_ms: 42,
        }
    }

    #[test]
    fn bounded_queue_reports_every_delivery_loss_class() {
        let (tx, rx) = std_mpsc::sync_channel(1);
        assert_eq!(
            enqueue_bridge_event(&tx, test_state_event(1)),
            BridgeQueueOutcome::Queued
        );
        assert_eq!(
            enqueue_bridge_event(&tx, test_state_event(2)),
            BridgeQueueOutcome::DroppedFull
        );
        drop(rx);
        assert_eq!(
            enqueue_bridge_event(&tx, test_state_event(3)),
            BridgeQueueOutcome::Closed
        );
    }

    #[test]
    fn text_field_bound_is_byte_exact_and_fail_closed() {
        let exact = "x".repeat(MAX_BRIDGE_TEXT_FIELD_BYTES);
        assert!(bridge_text_field_allowed("test", 1, "exact", &exact));

        let oversized = format!("{exact}é");
        assert_eq!(oversized.len(), MAX_BRIDGE_TEXT_FIELD_BYTES + 2);
        assert!(!bridge_text_field_allowed(
            "test",
            1,
            "oversized",
            &oversized
        ));
    }

    #[test]
    fn wire_event_preserves_notification_occurrence_timestamp() {
        match test_state_event(7).into_wire_event() {
            WireEvent::StateChange { pane_id, ts, .. } => {
                assert_eq!(pane_id, 7);
                assert_eq!(ts, 42);
            }
            other => panic!("unexpected bridge wire event: {other:?}"),
        }
    }

    #[test]
    fn explicitly_enabled_bridge_starts_before_endpoint_exists() {
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = TestMuxGuard::install(mux);
        let directory = tempfile::tempdir().expect("tempdir");
        let socket_path = directory.path().join("listener-not-started-yet.sock");
        assert!(!socket_path.exists());

        let bridge = NativeEventBridge::start(&socket_path)
            .expect("missing endpoint should enter bounded reconnect mode");
        drop(bridge);
    }

    #[test]
    fn retry_wait_returns_false_after_shutdown() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_trigger = shutdown.clone();
        let trigger = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            shutdown_trigger.store(true, Ordering::Release);
        });

        let start = Instant::now();
        let completed_delay = wait_for_retry_or_shutdown(Duration::from_secs(1), shutdown.as_ref());
        let elapsed = start.elapsed();
        trigger.join().expect("trigger thread should finish");

        assert!(!completed_delay);
        assert!(
            elapsed < Duration::from_millis(250),
            "shutdown-aware wait should stop quickly, elapsed={elapsed:?}"
        );
    }

    #[test]
    fn authenticated_connect_enforces_private_endpoint_metadata_and_peer_uid() {
        let root = tempfile::tempdir().expect("tempdir");
        let socket_path = root.path().join("native-bridge.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind native bridge test listener");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .expect("secure native bridge test socket");

        let stream = connect_authenticated_native_event_socket(&socket_path)
            .expect("same-user private endpoint should authenticate");
        assert!(
            socket2::SockRef::from(&stream)
                .nonblocking()
                .expect("read nonblocking socket mode"),
            "authenticated stream must use nonblocking writes so one total deadline is authoritative"
        );
        assert!(!SOCKET_CONNECT_TIMEOUT.is_zero());
        drop(stream);

        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o666))
            .expect("make native bridge test socket deliberately open");
        assert!(matches!(
            connect_authenticated_native_event_socket(&socket_path),
            Err(NativeEventError::Security(
                frankenterm_core::native_events::NativeEventSecurityError::EndpointModeMismatch {
                    actual_mode: 0o666
                }
            ))
        ));
        drop(listener);
    }

    #[test]
    fn write_event_refuses_io_after_shutdown_is_observed() {
        use std::io::Read as _;

        let (mut sender, mut receiver) = UnixStream::pair().expect("unix stream pair");
        sender
            .set_nonblocking(true)
            .expect("nonblocking test sender");
        receiver
            .set_nonblocking(true)
            .expect("nonblocking test receiver");
        let shutdown = AtomicBool::new(true);
        let event = WireEvent::Hello {
            proto: Some(1),
            wezterm_version: None,
            ts: Some(42),
        };

        let mut serialized_event = Vec::new();
        let error = write_event(&mut sender, &event, &mut serialized_event, &shutdown)
            .expect_err("shutdown must stop a write before socket side effects");
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);

        let mut byte = [0_u8; 1];
        let read_error = receiver
            .read(&mut byte)
            .expect_err("shutdown write must leave the peer empty");
        assert_eq!(read_error.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn pane_output_notification_does_no_state_snapshot_or_queue_work() {
        let mux = Mux::new(None);
        let (tx, rx) = std_mpsc::sync_channel(1);

        assert!(
            handle_mux_notification(&mux, &MuxNotification::PaneOutput(42), &tx),
            "byte-less PaneOutput must keep the native bridge subscribed"
        );
        assert!(matches!(
            rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn unconsumed_metadata_notifications_do_no_snapshot_or_queue_work() {
        let mux = Mux::new(None);
        let (tx, rx) = std_mpsc::sync_channel(2);

        assert!(handle_mux_notification(
            &mux,
            &MuxNotification::TabTitleChanged {
                tab_id: 7,
                title: "ignored-title".to_owned(),
            },
            &tx,
        ));
        assert!(handle_mux_notification(
            &mux,
            &MuxNotification::Alert {
                pane_id: 8,
                alert: wezterm_term::Alert::CurrentWorkingDirectoryChanged,
            },
            &tx,
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(std_mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn unrepresentable_retry_delay_stops_instead_of_spinning() {
        let shutdown = AtomicBool::new(false);
        assert!(
            !wait_for_retry_or_shutdown(Duration::MAX, &shutdown),
            "an unschedulable retry must fail closed instead of hot-spinning"
        );
    }

    #[test]
    fn write_event_emits_one_complete_json_line() {
        use std::io::BufRead as _;

        let (mut sender, receiver) = UnixStream::pair().expect("unix stream pair");
        sender
            .set_nonblocking(true)
            .expect("nonblocking test sender");
        receiver
            .set_read_timeout(Some(Duration::from_millis(250)))
            .expect("bounded test read");
        let shutdown = AtomicBool::new(false);
        let event = WireEvent::Hello {
            proto: Some(1),
            wezterm_version: Some("FrankenTerm test".to_string()),
            ts: Some(42),
        };
        let mut serialized_event = Vec::with_capacity(INITIAL_SERIALIZED_EVENT_CAPACITY);

        write_event(&mut sender, &event, &mut serialized_event, &shutdown)
            .expect("write one complete frame");

        let mut line = String::new();
        let mut reader = std::io::BufReader::new(receiver);
        let bytes_read = reader
            .read_line(&mut line)
            .expect("read one complete frame");
        assert_eq!(bytes_read, line.len());
        assert!(line.ends_with('\n'));
        assert!(matches!(
            serde_json::from_str::<WireEvent>(&line).expect("decode emitted frame"),
            WireEvent::Hello {
                proto: Some(1),
                wezterm_version: Some(version),
                ts: Some(42),
            } if version == "FrankenTerm test"
        ));
    }

    #[test]
    fn deadline_writer_handles_partial_progress_and_retryable_errors() {
        let mut writer = ScriptedWriter {
            steps: VecDeque::from([
                ScriptedWrite::Bytes(2),
                ScriptedWrite::Error(std::io::ErrorKind::Interrupted),
                ScriptedWrite::Error(std::io::ErrorKind::WouldBlock),
                ScriptedWrite::Bytes(usize::MAX),
            ]),
            written: Vec::new(),
            calls: 0,
        };
        let shutdown = AtomicBool::new(false);
        let start = Instant::now();
        let clock = Cell::new(start);
        let deadline = clock
            .get()
            .checked_add(Duration::from_millis(10))
            .expect("test deadline fits Instant");

        write_bytes_with_deadline(
            &mut writer,
            b"abcdef",
            &shutdown,
            deadline,
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
        )
        .expect("partial writes and retryable errors complete within one deadline");

        assert_eq!(writer.written, b"abcdef");
        assert_eq!(writer.calls, 4);
        assert_eq!(
            clock.get().duration_since(start),
            Duration::from_millis(2)
        );
    }

    #[test]
    fn deadline_writer_bounds_repeated_would_block_without_wall_time() {
        let mut writer = ScriptedWriter {
            steps: VecDeque::new(),
            written: Vec::new(),
            calls: 0,
        };
        let shutdown = AtomicBool::new(false);
        let start = Instant::now();
        let clock = Cell::new(start);
        let deadline = start
            .checked_add(Duration::from_millis(3))
            .expect("test deadline fits Instant");

        let error = write_bytes_with_deadline(
            &mut writer,
            b"blocked",
            &shutdown,
            deadline,
            || clock.get(),
            |duration| clock.set(clock.get() + duration),
        )
        .expect_err("repeated WouldBlock must terminate at the total deadline");

        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(writer.calls, 3);
        assert_eq!(clock.get().duration_since(start), Duration::from_millis(3));
        assert!(writer.written.is_empty());
    }

    #[test]
    fn drop_unsubscribes_mux_subscription_and_joins_sender_thread() {
        let mux = Arc::new(Mux::new(None));
        let _mux_guard = TestMuxGuard::install(mux.clone());

        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let thread_exited = Arc::new(AtomicBool::new(false));
        let exited_flag = thread_exited.clone();
        let sender_thread = std::thread::spawn(move || {
            let completed_delay =
                wait_for_retry_or_shutdown(Duration::from_secs(30), thread_shutdown.as_ref());
            assert!(!completed_delay);
            std::thread::sleep(Duration::from_millis(25));
            exited_flag.store(true, Ordering::Release);
        });

        let subscription_id = mux
            .subscribe(|_| true)
            .expect("test mux subscription should allocate an identifier");
        let bridge = NativeEventBridge {
            shutdown,
            sender_thread: Some(sender_thread),
            mux: Some(mux.clone()),
            subscription_id: Some(subscription_id),
        };

        drop(bridge);

        assert!(
            thread_exited.load(Ordering::Acquire),
            "drop should wait for the sender thread to exit"
        );
        assert!(
            !mux.unsubscribe(subscription_id),
            "drop should already have removed the mux subscription"
        );
    }

    #[test]
    fn drop_unsubscribes_original_mux_after_global_mux_swap() {
        let original_mux = Arc::new(Mux::new(None));
        let _mux_guard = TestMuxGuard::install(original_mux.clone());
        let replacement_mux = Arc::new(Mux::new(None));

        let subscription_id = original_mux
            .subscribe(|_| true)
            .expect("test mux subscription should allocate an identifier");
        let bridge = NativeEventBridge {
            shutdown: Arc::new(AtomicBool::new(false)),
            sender_thread: None,
            mux: Some(original_mux.clone()),
            subscription_id: Some(subscription_id),
        };

        Mux::set_mux(&replacement_mux);
        drop(bridge);

        assert!(
            !original_mux.unsubscribe(subscription_id),
            "drop should unsubscribe from the original mux instance, not the current global mux"
        );
    }

    #[test]
    fn notification_lookup_remains_bound_to_originating_mux_after_global_swap() {
        let original_mux = Arc::new(Mux::new(None));
        let _mux_guard = TestMuxGuard::install(original_mux.clone());
        let originating_mux = Arc::downgrade(&original_mux);
        let replacement_mux = Arc::new(Mux::new(None));

        Mux::set_mux(&replacement_mux);
        let callback_mux = originating_mux_for_notification(&originating_mux)
            .expect("bridge retains the originating mux while subscribed");

        assert!(Arc::ptr_eq(&callback_mux, &original_mux));
        assert!(!Arc::ptr_eq(&callback_mux, &replacement_mux));
    }
}
