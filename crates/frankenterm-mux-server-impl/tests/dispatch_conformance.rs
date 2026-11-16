//! Cross-backend conformance harness for the mux dispatch loop.
//!
//! wa-283h4.18: drives `process_with_config` through every
//! `DispatchIoPreference` against an in-memory scripted stream that replays a
//! canned request stream and captures every byte written by the server. The
//! wire output must be identical across backends — the I/O reactor may change
//! but the PDU protocol surface must not.
//!
//! The harness exercises the real UnixStream fast path and the fallback
//! readiness path symmetrically via the `DispatchStream` trait, so it remains
//! valid after wa-283h4.17 wires io_uring read/write onto UnixStream.

use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use async_channel::TryRecvError;
use codec::{CompressionMode, DecodedPdu, Pdu, Ping, StreamingPduBuffer};
use frankenterm_mux_server_impl::dispatch::{
    self, DispatchIoPreference, DispatchReadySide, DispatchRuntimeConfig, DispatchStream,
    DispatchStreamKind,
};
use mux::Mux;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

static GLOBAL_STATE_TEST_LOCK: StdMutex<()> = StdMutex::new(());

struct ScopedMux(Option<Arc<Mux>>);

impl ScopedMux {
    fn install(mux: &Arc<Mux>) -> Self {
        let prior = Mux::try_get();
        Mux::set_mux(mux);
        Self(prior)
    }
}

impl Drop for ScopedMux {
    fn drop(&mut self) {
        if let Some(prior) = self.0.take() {
            Mux::set_mux(&prior);
        } else {
            Mux::shutdown();
        }
    }
}

#[derive(Debug)]
struct ScriptState {
    script: Vec<u8>,
    cursor: AtomicUsize,
    chunk: usize,
    writes: StdMutex<Vec<u8>>,
    flush_count: AtomicUsize,
    writable_waits: AtomicUsize,
    readable_waits: AtomicUsize,
    backpressure: bool,
    write_pending: AtomicBool,
    flush_pending: AtomicBool,
    pending_polls: AtomicUsize,
    stream_kind: DispatchStreamKind,
}

#[derive(Debug, Clone)]
struct ScriptHandle(Arc<ScriptState>);

impl ScriptHandle {
    fn writes(&self) -> Vec<u8> {
        self.0.writes.lock().expect("writes lock").clone()
    }

    fn flush_count(&self) -> usize {
        self.0.flush_count.load(Ordering::Relaxed)
    }

    fn writable_waits(&self) -> usize {
        self.0.writable_waits.load(Ordering::Relaxed)
    }

    fn readable_waits(&self) -> usize {
        self.0.readable_waits.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
struct ScriptedDispatchStream {
    state: Arc<ScriptState>,
}

impl ScriptedDispatchStream {
    fn new(
        script: Vec<u8>,
        stream_kind: DispatchStreamKind,
        chunk: usize,
        backpressure: bool,
    ) -> (Self, ScriptHandle) {
        let state = Arc::new(ScriptState {
            script,
            cursor: AtomicUsize::new(0),
            chunk,
            writes: StdMutex::new(Vec::new()),
            flush_count: AtomicUsize::new(0),
            writable_waits: AtomicUsize::new(0),
            readable_waits: AtomicUsize::new(0),
            backpressure,
            write_pending: AtomicBool::new(true),
            flush_pending: AtomicBool::new(true),
            pending_polls: AtomicUsize::new(0),
            stream_kind,
        });
        let stream = Self {
            state: Arc::clone(&state),
        };
        (stream, ScriptHandle(state))
    }
}

impl DispatchStream for ScriptedDispatchStream {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        self.state.stream_kind
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        self.state.readable_waits.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state.writable_waits.fetch_add(1, Ordering::Relaxed);
            Ok(())
        })
    }

    fn wait_for_readable_or_writable(
        &self,
    ) -> Pin<Box<dyn Future<Output = io::Result<DispatchReadySide>> + Send + '_>> {
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            state.writable_waits.fetch_add(1, Ordering::Relaxed);
            Ok(DispatchReadySide::Writable)
        })
    }
}

impl AsyncRead for ScriptedDispatchStream {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let state = &self.state;
        let cursor = state.cursor.load(Ordering::Relaxed);
        let available = state.script.len().saturating_sub(cursor);
        if available == 0 {
            return Poll::Ready(Ok(()));
        }
        let chunk_cap = if state.chunk == 0 {
            usize::MAX
        } else {
            state.chunk
        };
        let want = buf.remaining().min(available).min(chunk_cap);
        if want == 0 {
            return Poll::Ready(Ok(()));
        }
        buf.put_slice(&state.script[cursor..cursor + want]);
        state.cursor.store(cursor + want, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for ScriptedDispatchStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let buf = if self.state.backpressure {
            if self.state.write_pending.swap(false, Ordering::Relaxed) {
                self.state.pending_polls.fetch_add(1, Ordering::Relaxed);
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            assert_eq!(
                self.state.writable_waits.load(Ordering::Relaxed),
                self.state.pending_polls.load(Ordering::Relaxed),
                "pending write must await readiness before retry"
            );
            self.state.write_pending.store(true, Ordering::Relaxed);
            &buf[..buf.len().min(2)]
        } else {
            buf
        };
        self.state
            .writes
            .lock()
            .expect("writes lock")
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.state.backpressure {
            if self.state.flush_pending.swap(false, Ordering::Relaxed) {
                self.state.pending_polls.fetch_add(1, Ordering::Relaxed);
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            assert_eq!(
                self.state.writable_waits.load(Ordering::Relaxed),
                self.state.pending_polls.load(Ordering::Relaxed),
                "pending flush must await readiness before retry"
            );
            self.state.flush_pending.store(true, Ordering::Relaxed);
        }
        self.state.flush_count.fetch_add(1, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn encoded_ping_series(n: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    for serial in 1..=n {
        Pdu::Ping(Ping {})
            .encode(&mut buf, serial)
            .expect("encode ping");
    }
    buf
}

const ALL_COMPRESSION_MODES: &[CompressionMode] = &[
    CompressionMode::Auto,
    CompressionMode::Never,
    CompressionMode::Always,
];

fn encoded_mixed_compression_ping_series(n: u64) -> (Vec<u8>, Vec<u64>) {
    let mut buf = Vec::new();
    let mut serials = Vec::new();
    for idx in 0..n {
        let serial = 10_000 + idx * 17;
        let mode = ALL_COMPRESSION_MODES[idx as usize % ALL_COMPRESSION_MODES.len()];
        Pdu::Ping(Ping {})
            .encode_with_mode(&mut buf, serial, mode)
            .expect("encode mixed-compression ping");
        serials.push(serial);
    }
    (buf, serials)
}

fn try_decode_all(bytes: &[u8]) -> Result<Vec<DecodedPdu>, String> {
    let mut out = Vec::new();
    let mut buffer = StreamingPduBuffer::from(bytes.to_vec());
    while let Some(decoded) =
        Pdu::stream_decode(&mut buffer).map_err(|err| format!("stream_decode frame: {err:#}"))?
    {
        out.push(decoded);
    }
    if !buffer.is_empty() {
        return Err(format!(
            "dispatch conformance output ended with {} trailing undecoded byte(s): {:02x?}",
            buffer.len(),
            buffer.as_slice()
        ));
    }
    Ok(out)
}

fn decode_all(bytes: &[u8]) -> Vec<DecodedPdu> {
    try_decode_all(bytes).unwrap_or_else(|message| panic!("{message}"))
}

#[test]
fn decoder_rejects_trailing_partial_frame() {
    let mut wire = encoded_ping_series(1);
    let mut partial = encoded_ping_series(1);
    partial.pop().expect("encoded ping frame must not be empty");
    wire.extend_from_slice(&partial);

    let message =
        try_decode_all(&wire).expect_err("trailing partial frame must fail conformance decoding");
    assert!(
        message.contains("trailing undecoded"),
        "unexpected decoder error: {message}"
    );
}

fn run_once(
    preference: DispatchIoPreference,
    stream_kind: DispatchStreamKind,
    script: Vec<u8>,
    read_chunk: usize,
) -> ScriptHandle {
    run_with_backpressure(preference, stream_kind, script, read_chunk, false)
}

fn run_with_backpressure(
    preference: DispatchIoPreference,
    stream_kind: DispatchStreamKind,
    script: Vec<u8>,
    read_chunk: usize,
    backpressure: bool,
) -> ScriptHandle {
    let _lock = GLOBAL_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // Session admission reserves its render coordinator on the real bounded
    // scheduler, including for empty streams. Retire queued work only after
    // this session and its scoped mux have been dropped.
    let _executor = promise::spawn::SimpleExecutor::new();
    let mux = Arc::new(Mux::new(None));
    let _scoped = ScopedMux::install(&mux);

    let (stream, handle) =
        ScriptedDispatchStream::new(script, stream_kind, read_chunk, backpressure);

    let result = promise::spawn::block_on(dispatch::process_with_config(
        stream,
        DispatchRuntimeConfig::new(preference),
    ));
    assert!(
        result.is_ok(),
        "process_with_config returned error for {preference:?}/{stream_kind:?}: {:?}",
        result.err()
    );

    handle
}

const ALL_PREFERENCES: &[DispatchIoPreference] = &[
    DispatchIoPreference::Auto,
    DispatchIoPreference::IoUring,
    DispatchIoPreference::Epoll,
    DispatchIoPreference::Kqueue,
    DispatchIoPreference::Poll,
];

const ALL_STREAM_KINDS: &[DispatchStreamKind] = &[
    DispatchStreamKind::Unix,
    DispatchStreamKind::Tls,
    DispatchStreamKind::Generic,
];

#[test]
fn empty_stream_yields_clean_disconnect_for_every_backend() {
    for pref in ALL_PREFERENCES {
        for stream_kind in ALL_STREAM_KINDS {
            let handle = run_once(*pref, *stream_kind, Vec::new(), 0);
            assert!(
                handle.writes().is_empty(),
                "{pref:?}/{stream_kind:?}: empty script must produce no output"
            );
            assert_eq!(
                handle.flush_count(),
                0,
                "{pref:?}/{stream_kind:?}: no writes should mean no flushes"
            );
        }
    }
}

#[test]
fn ping_stream_produces_identical_wire_output_across_backends() {
    let script = encoded_ping_series(16);

    let baseline = run_once(
        DispatchIoPreference::Poll,
        DispatchStreamKind::Unix,
        script.clone(),
        0,
    );
    let baseline_bytes = baseline.writes();
    let baseline_flushes = baseline.flush_count();

    let decoded = decode_all(&baseline_bytes);
    assert_eq!(
        decoded.len(),
        16,
        "expected 16 Pongs in baseline output, got {}",
        decoded.len()
    );
    for (idx, pdu) in decoded.iter().enumerate() {
        let expected_serial = (idx as u64) + 1;
        assert_eq!(
            pdu.serial, expected_serial,
            "serial mismatch at idx {idx}: got {}",
            pdu.serial
        );
        assert!(
            matches!(pdu.pdu, Pdu::Pong(_)),
            "non-Pong response at idx {idx}: {:?}",
            pdu.pdu
        );
    }
    assert!(
        baseline_flushes > 0,
        "baseline should flush at least once for 16 pongs"
    );

    for pref in ALL_PREFERENCES {
        for stream_kind in ALL_STREAM_KINDS {
            let handle = run_once(*pref, *stream_kind, script.clone(), 0);
            assert_eq!(
                handle.writes(),
                baseline_bytes,
                "wire output diverged for pref={pref:?} stream={stream_kind:?}"
            );
            assert_eq!(
                handle.flush_count(),
                baseline_flushes,
                "flush count diverged for pref={pref:?} stream={stream_kind:?}"
            );
            assert_eq!(
                handle.writable_waits(),
                0,
                "ready writes must not await writability for pref={pref:?} stream={stream_kind:?}"
            );
        }
    }
}

#[test]
fn chunked_reads_preserve_wire_output_across_backends() {
    let script = encoded_ping_series(12);
    let baseline = run_once(
        DispatchIoPreference::Poll,
        DispatchStreamKind::Unix,
        script.clone(),
        0,
    );
    let baseline_bytes = baseline.writes();
    let baseline_flushes = baseline.flush_count();

    for chunk in [1usize, 2, 3, 5, 8, 13] {
        for pref in ALL_PREFERENCES {
            for stream_kind in ALL_STREAM_KINDS {
                let handle = run_once(*pref, *stream_kind, script.clone(), chunk);
                assert_eq!(
                    handle.writes(),
                    baseline_bytes,
                    "wire output diverged for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
                assert_eq!(
                    handle.flush_count(),
                    baseline_flushes,
                    "flush count diverged for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
                assert_eq!(
                    handle.writable_waits(),
                    0,
                    "ready writes must not await writability for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
                assert!(
                    handle.readable_waits() >= 1,
                    "chunked read path should observe readiness for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
            }
        }
    }
}

#[test]
fn mixed_compression_ping_stream_preserves_dispatch_order_across_backends() {
    let (script, expected_serials) = encoded_mixed_compression_ping_series(18);

    for chunk in [0usize, 1, 2, 7, 19] {
        for pref in ALL_PREFERENCES {
            for stream_kind in ALL_STREAM_KINDS {
                let handle = run_once(*pref, *stream_kind, script.clone(), chunk);
                let decoded = decode_all(&handle.writes());
                let observed_serials: Vec<u64> = decoded.iter().map(|pdu| pdu.serial).collect();
                assert_eq!(
                    observed_serials, expected_serials,
                    "Pong serial order diverged for pref={pref:?} stream={stream_kind:?} chunk={chunk}",
                );
                for (idx, pdu) in decoded.iter().enumerate() {
                    assert!(
                        matches!(pdu.pdu, Pdu::Pong(_)),
                        "non-Pong response at idx {idx} for pref={pref:?} stream={stream_kind:?} chunk={chunk}: {:?}",
                        pdu.pdu
                    );
                }
                assert!(
                    handle.readable_waits() >= 1,
                    "dispatch should wait for readability for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
                assert_eq!(
                    handle.writable_waits(),
                    0,
                    "ready writes must not await writability for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
            }
        }
    }
}

#[test]
fn pending_short_writes_and_flushes_preserve_exact_wire_output() {
    let script = encoded_ping_series(3);
    let baseline = run_once(
        DispatchIoPreference::Poll,
        DispatchStreamKind::Unix,
        script.clone(),
        0,
    );
    for preference in ALL_PREFERENCES {
        for stream_kind in ALL_STREAM_KINDS {
            let blocked = run_with_backpressure(*preference, *stream_kind, script.clone(), 2, true);
            assert_eq!(blocked.writes(), baseline.writes());
            assert_eq!(blocked.flush_count(), baseline.flush_count());
            let pending = blocked.0.pending_polls.load(Ordering::Relaxed);
            assert!(
                pending > blocked.flush_count(),
                "write and flush must both block"
            );
            assert_eq!(blocked.writable_waits(), pending);
        }
    }
}

#[test]
fn readable_wait_is_observed_before_first_decode() {
    // With an empty script, the select() races item_rx.recv() against
    // wait_for_readable(). wait_for_readable resolves immediately, so the
    // loop reaches decode_async, hits EOF, and returns cleanly. The stream
    // should observe at least one readable wait before the clean exit.
    let handle = run_once(
        DispatchIoPreference::Auto,
        DispatchStreamKind::Unix,
        Vec::new(),
        0,
    );
    assert!(
        handle.readable_waits() >= 1,
        "dispatch loop must consult wait_for_readable before EOF, got {}",
        handle.readable_waits()
    );
}

#[test]
fn async_channel_closed_surface_unchanged() {
    let (tx, rx) = async_channel::unbounded::<u8>();
    drop(tx);
    let err = rx.try_recv().expect_err("closed channel must error");
    assert!(matches!(err, TryRecvError::Closed));
}

mod pane_user_vars {
    use super::*;
    use base64::Engine;
    use codec::{
        GetCodecVersion, GetPaneUserVarsV1, ListPanes, ListPanesCoherent, ListPanesOrderedV1,
        PaneUserVarsOutcomeV1, SetClientId, TopologyCapabilities,
    };
    use mux::client::ClientId;
    use mux::pane::{
        CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, PaneUserVarsSnapshot,
        WithPaneLines,
    };
    use mux::renderable::{RenderableDimensions, StableCursorPosition};
    use parking_lot::{MappedMutexGuard, Mutex as ParkingMutex, MutexGuard};
    use promise::spawn::SimpleExecutor;
    use rangeset::RangeSet;
    use std::collections::{HashMap, VecDeque};
    use std::ops::Range;
    use std::sync::Weak;
    use std::task::Waker;
    use termwiz::surface::{Line, SequenceNo};
    use wezterm_term::color::ColorPalette;
    use wezterm_term::{
        Alert, AlertHandler, KeyCode, KeyModifiers, MouseEvent, StableRowIndex, Terminal,
        TerminalConfiguration, TerminalSize,
    };

    const MAX_WIRE_BUFFER_BYTES: usize = 16 * 1024 * 1024;
    const MAX_PROGRESS_POLLS: usize = 4096;

    #[derive(Debug, Default)]
    struct DuplexState {
        requests: VecDeque<u8>,
        replies: Vec<u8>,
        closed: bool,
        write_budget: Option<usize>,
        readable: Option<Waker>,
        writable: Option<Waker>,
    }

    /// An interactive, bounded byte transport. Unlike a canned request stream,
    /// this keeps the connection open while the main-thread RPC executor runs
    /// and lets the client stop a write after an exact byte boundary.
    #[derive(Clone, Debug, Default)]
    struct DuplexStream(Arc<ParkingMutex<DuplexState>>);

    impl DuplexStream {
        fn send(&self, serial: u64, pdu: Pdu) {
            let mut bytes = Vec::new();
            pdu.encode_with_mode(&mut bytes, serial, CompressionMode::Never)
                .expect("encode user-variable request");
            let mut state = self.0.lock();
            assert!(!state.closed, "request after client disconnect");
            assert!(state.requests.len() + bytes.len() <= MAX_WIRE_BUFFER_BYTES);
            state.requests.extend(bytes);
            if let Some(waker) = state.readable.take() {
                waker.wake();
            }
        }

        fn set_write_budget(&self, budget: Option<usize>) {
            let mut state = self.0.lock();
            state.write_budget = budget;
            if let Some(waker) = state.writable.take() {
                waker.wake();
            }
        }

        fn close(&self) {
            let mut state = self.0.lock();
            state.closed = true;
            if let Some(waker) = state.readable.take() {
                waker.wake();
            }
        }
    }

    impl DispatchStream for DuplexStream {
        fn dispatch_stream_kind(&self) -> DispatchStreamKind {
            DispatchStreamKind::Generic
        }

        fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::poll_fn(|cx| {
                let mut state = self.0.lock();
                if !state.requests.is_empty() || state.closed {
                    Poll::Ready(Ok(()))
                } else {
                    state.readable = Some(cx.waker().clone());
                    Poll::Pending
                }
            }))
        }

        fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
            Box::pin(std::future::poll_fn(|cx| {
                let mut state = self.0.lock();
                if state.write_budget != Some(0) {
                    Poll::Ready(Ok(()))
                } else {
                    state.writable = Some(cx.waker().clone());
                    Poll::Pending
                }
            }))
        }

        fn wait_for_readable_or_writable(
            &self,
        ) -> Pin<Box<dyn Future<Output = io::Result<DispatchReadySide>> + Send + '_>> {
            Box::pin(std::future::poll_fn(|cx| {
                let mut state = self.0.lock();
                if !state.requests.is_empty() || state.closed {
                    Poll::Ready(Ok(DispatchReadySide::Readable))
                } else if state.write_budget != Some(0) {
                    Poll::Ready(Ok(DispatchReadySide::Writable))
                } else {
                    state.readable = Some(cx.waker().clone());
                    state.writable = Some(cx.waker().clone());
                    Poll::Pending
                }
            }))
        }
    }

    impl AsyncRead for DuplexStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let mut state = self.0.lock();
            if state.requests.is_empty() && !state.closed {
                state.readable = Some(cx.waker().clone());
                return Poll::Pending;
            }
            // Fragment every request, including its header, across reads.
            let count = buf.remaining().min(state.requests.len()).min(3);
            for byte in state.requests.drain(..count) {
                buf.put_slice(&[byte]);
            }
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for DuplexStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut state = self.0.lock();
            if state.write_budget == Some(0) {
                state.writable = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let count = bytes.len().min(state.write_budget.unwrap_or(7)).min(7);
            assert!(state.replies.len() + count <= MAX_WIRE_BUFFER_BYTES);
            state.replies.extend_from_slice(&bytes[..count]);
            if let Some(budget) = state.write_budget.as_mut() {
                *budget -= count;
            }
            Poll::Ready(Ok(count))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    struct WireSession {
        wire: DuplexStream,
        server: Pin<Box<dyn Future<Output = anyhow::Result<()>>>>,
        received: StreamingPduBuffer,
    }

    impl WireSession {
        fn connect(executor: &SimpleExecutor) -> Self {
            let mut quiescent = false;
            for _ in 0..MAX_PROGRESS_POLLS {
                if !executor.try_tick().expect("settle prior fixture work") {
                    quiescent = true;
                    break;
                }
            }
            assert!(
                quiescent,
                "setup work exceeded its bounded scheduler budget"
            );
            let wire = DuplexStream::default();
            let server = Box::pin(dispatch::process_with_config(
                wire.clone(),
                DispatchRuntimeConfig::new(DispatchIoPreference::Poll),
            ));
            let mut session = Self {
                wire,
                server,
                received: StreamingPduBuffer::from(Vec::new()),
            };
            session
                .wire
                .send(1, Pdu::GetCodecVersion(GetCodecVersion {}));
            let codec = session.receive(executor);
            assert_eq!(codec.serial, 1);
            assert!(matches!(codec.pdu, Pdu::GetCodecVersionResponse(_)));
            session.wire.send(
                2,
                Pdu::SetClientId(SetClientId {
                    client_id: ClientId {
                        hostname: "user-var-conformance".into(),
                        username: "isolated-fixture".into(),
                        pid: 42,
                        epoch: 1,
                        id: 0,
                        ssh_auth_sock: None,
                    },
                    is_proxy: false,
                }),
            );
            let registration = session.receive(executor);
            assert_eq!(registration.serial, 2);
            assert!(matches!(registration.pdu, Pdu::UnitResponse(_)));
            session
        }

        fn poll(&mut self, executor: &SimpleExecutor) -> Poll<anyhow::Result<()>> {
            executor.try_tick().expect("pump the real RPC scheduler");
            self.server
                .as_mut()
                .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
        }

        fn decode_ready(&mut self) -> Option<DecodedPdu> {
            let bytes = std::mem::take(&mut self.wire.0.lock().replies);
            self.received.extend_from_slice(&bytes);
            assert!(self.received.len() <= MAX_WIRE_BUFFER_BYTES);
            Pdu::stream_decode(&mut self.received).expect("decode server wire response")
        }

        fn receive(&mut self, executor: &SimpleExecutor) -> DecodedPdu {
            for _ in 0..MAX_PROGRESS_POLLS {
                if let Some(frame) = self.decode_ready() {
                    return frame;
                }
                if let Poll::Ready(result) = self.poll(executor) {
                    panic!("dispatch retired before delivering the expected frame: {result:?}");
                }
            }
            panic!("dispatch made no response within the bounded scheduler budget");
        }

        fn snapshot(
            &mut self,
            executor: &SimpleExecutor,
            serial: u64,
            pane_id: PaneId,
        ) -> PaneUserVarsOutcomeV1 {
            self.wire.send(
                serial,
                Pdu::GetPaneUserVarsV1(GetPaneUserVarsV1 { pane_id }),
            );
            let frame = self.receive(executor);
            assert_eq!(frame.serial, serial, "snapshot must be a correlated reply");
            let Pdu::GetPaneUserVarsV1Response(response) = frame.pdu else {
                panic!(
                    "expected complete user-variable snapshot, got {:?}",
                    frame.pdu
                );
            };
            assert_eq!(response.pane_id, pane_id);
            response.outcome
        }

        fn assert_quiet(&mut self, executor: &SimpleExecutor) {
            for _ in 0..64 {
                assert!(self.poll(executor).is_pending());
                assert!(
                    self.decode_ready().is_none(),
                    "metadata-only attachment must not manufacture extra notifications"
                );
            }
            assert!(self.received.is_empty(), "unexpected partial extra reply");
        }

        fn disconnect(mut self, executor: &SimpleExecutor) {
            self.wire.close();
            for _ in 0..MAX_PROGRESS_POLLS {
                if let Poll::Ready(result) = self.poll(executor) {
                    result.expect("clean client disconnect");
                    return;
                }
            }
            panic!("dispatch did not settle client disconnect");
        }
    }

    #[derive(Debug)]
    struct UserVarTermConfig;

    impl TerminalConfiguration for UserVarTermConfig {
        fn color_palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn max_user_vars(&self) -> usize {
            2
        }
    }

    struct MuxAlerts {
        mux: Weak<Mux>,
        pane_id: PaneId,
    }

    impl AlertHandler for MuxAlerts {
        fn alert(&mut self, alert: Alert) {
            if let Some(mux) = self.mux.upgrade() {
                mux.notify(mux::MuxNotification::Alert {
                    pane_id: self.pane_id,
                    alert,
                });
            }
        }
    }

    /// The terminal parser and variable eviction are production code. Only the
    /// PTY/render surface is replaced: every render read is counted so that a
    /// passing metadata test cannot accidentally activate the background pane.
    struct UserVarPane {
        id: PaneId,
        terminal: ParkingMutex<Terminal>,
        registration: Arc<mux::PaneRegistrationSlot>,
        alerts: mux::PaneAlertDispatchQueue,
        render_reads: AtomicUsize,
        writer: ParkingMutex<io::Sink>,
    }

    impl UserVarPane {
        fn new(mux: &Arc<Mux>, id: PaneId) -> Arc<Self> {
            let mut terminal = Terminal::new(
                Self::size(),
                Arc::new(UserVarTermConfig),
                "frankenterm",
                "user-var-conformance",
                Box::new(io::sink()),
            );
            terminal.set_notification_handler(Box::new(MuxAlerts {
                mux: Arc::downgrade(mux),
                pane_id: id,
            }));
            Arc::new(Self {
                id,
                terminal: ParkingMutex::new(terminal),
                registration: Arc::new(mux::PaneRegistrationSlot::default()),
                alerts: mux::PaneAlertDispatchQueue::default(),
                render_reads: AtomicUsize::new(0),
                writer: ParkingMutex::new(io::sink()),
            })
        }

        fn size() -> TerminalSize {
            TerminalSize {
                rows: 2,
                cols: 8,
                pixel_width: 64,
                pixel_height: 32,
                dpi: 96,
            }
        }

        fn set(&self, name: &str, value: &str) {
            let encoded = base64::engine::general_purpose::STANDARD.encode(value);
            let osc = format!("\x1b]1337;SetUserVar={name}={encoded}\x07");
            self.terminal.lock().advance_bytes(osc.as_bytes());
        }
    }

    impl Pane for UserVarPane {
        fn pane_id(&self) -> PaneId {
            self.id
        }

        fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
            &self.registration
        }

        fn get_cursor_position(&self) -> StableCursorPosition {
            StableCursorPosition::default()
        }

        fn get_current_seqno(&self) -> SequenceNo {
            self.terminal.lock().current_seqno()
        }

        fn get_changed_since(
            &self,
            _lines: Range<StableRowIndex>,
            _seqno: SequenceNo,
        ) -> RangeSet<StableRowIndex> {
            self.render_reads.fetch_add(1, Ordering::Relaxed);
            RangeSet::new()
        }

        fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
            self.render_reads.fetch_add(1, Ordering::Relaxed);
            (lines.start, Vec::new())
        }

        fn with_lines_mut(&self, lines: Range<StableRowIndex>, callback: &mut dyn WithPaneLines) {
            mux::pane::impl_with_lines_via_get_lines(self, lines, callback);
        }

        fn for_each_logical_line_in_stable_range_mut(
            &self,
            lines: Range<StableRowIndex>,
            callback: &mut dyn ForEachPaneLogicalLine,
        ) {
            mux::pane::impl_for_each_logical_line_via_get_logical_lines(self, lines, callback);
        }

        fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
            mux::pane::impl_get_logical_lines_via_get_lines(self, lines)
        }

        fn get_dimensions(&self) -> RenderableDimensions {
            RenderableDimensions {
                cols: 8,
                viewport_rows: 2,
                scrollback_rows: 2,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 64,
                pixel_height: 32,
                reverse_video: false,
            }
        }

        fn get_title(&self) -> String {
            format!("pane-{}", self.id)
        }

        fn copy_user_vars(&self) -> HashMap<String, String> {
            self.terminal.lock().user_vars().clone()
        }

        fn capture_user_vars_snapshot(
            &self,
            max_entries: usize,
            max_text_bytes: usize,
        ) -> anyhow::Result<PaneUserVarsSnapshot> {
            let terminal = self.terminal.lock();
            PaneUserVarsSnapshot::capture(
                terminal.user_vars(),
                max_entries,
                max_text_bytes,
                &self.alerts,
            )
        }

        fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
            anyhow::bail!("metadata test must not send input")
        }

        fn reader(&self) -> anyhow::Result<Option<Box<dyn io::Read + Send>>> {
            Ok(None)
        }

        fn writer(&self) -> MappedMutexGuard<'_, dyn io::Write> {
            MutexGuard::map(self.writer.lock(), |sink| sink as &mut dyn io::Write)
        }

        fn resize(&self, _size: TerminalSize) -> anyhow::Result<()> {
            Ok(())
        }

        fn key_down(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            anyhow::bail!("metadata test must not send keys")
        }

        fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
            anyhow::bail!("metadata test must not send keys")
        }

        fn mouse_event(&self, _event: MouseEvent) -> anyhow::Result<()> {
            anyhow::bail!("metadata test must not interact with a pane")
        }

        fn is_dead(&self) -> bool {
            false
        }

        fn palette(&self) -> ColorPalette {
            ColorPalette::default()
        }

        fn domain_id(&self) -> mux::domain::DomainId {
            0
        }

        fn is_mouse_grabbed(&self) -> bool {
            false
        }

        fn is_alt_screen_active(&self) -> bool {
            false
        }

        fn get_current_working_dir(&self, _policy: CachePolicy) -> Option<url::Url> {
            None
        }
    }

    fn add_tab(mux: &Arc<Mux>, window_id: mux::window::WindowId, pane: &Arc<UserVarPane>) {
        let tab = Arc::new(mux::tab::Tab::new(&UserVarPane::size()));
        let erased: Arc<dyn Pane> = pane.clone();
        tab.assign_pane(&erased);
        mux.add_tab_and_active_pane(&tab)
            .expect("register fixture tab and pane");
        mux.add_tab_to_window(&tab, window_id)
            .expect("attach fixture tab");
    }

    fn expect_snapshot(outcome: PaneUserVarsOutcomeV1, expected: &[(&str, &str)]) {
        let PaneUserVarsOutcomeV1::Snapshot { user_vars } = outcome else {
            panic!("live pane must return a snapshot, got {outcome:?}");
        };
        let expected: HashMap<_, _> = expected
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        assert_eq!(user_vars, expected, "snapshot must be complete and current");
    }

    fn expect_user_var(frame: DecodedPdu, pane_id: PaneId, expected: &str) {
        assert_eq!(frame.serial, 0, "live change must remain unilateral");
        let Pdu::NotifyAlert(alert) = frame.pdu else {
            panic!(
                "expected live user-variable notification, got {:?}",
                frame.pdu
            );
        };
        assert_eq!(alert.pane_id, pane_id);
        assert!(
            matches!(alert.alert, Alert::SetUserVar { ref name, ref value }
                if name == "status" && value == expected),
            "unexpected alert: {:?}",
            alert.alert
        );
    }

    #[test]
    fn unrendered_user_vars_follow_legacy_coherent_and_ordered_fallback_discovery() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _scoped = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let foreground = UserVarPane::new(&mux, 8_001);
        let background = UserVarPane::new(&mux, 8_002);
        add_tab(&mux, *window, &foreground);
        add_tab(&mux, *window, &background);
        drop(window);
        foreground.set("status", "foreground-before-connect");
        background.set("status", "background-before-connect");
        background.set("branch", "feature/unrendered");

        let ordered_caps = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        );
        let listings = [
            Pdu::ListPanes(ListPanes {}),
            Pdu::ListPanesCoherent(ListPanesCoherent {
                supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            }),
            Pdu::ListPanesOrderedV1(ListPanesOrderedV1 {
                protocol_version: codec::ORDERED_WINDOW_PROTOCOL_VERSION,
                domain_binding_id: codec::DomainBindingId::from_bytes([0x51; 16]),
                supported: ordered_caps,
                required: ordered_caps,
            }),
        ];
        for (index, listing) in listings.into_iter().enumerate() {
            let mut client = WireSession::connect(&executor);
            client.wire.send(3, listing);
            let response = client.receive(&executor);
            assert_eq!(response.serial, 3, "discovery must precede metadata reads");
            assert!(match (index, response.pdu) {
                (0, Pdu::ListPanesResponse(response)) => response.tabs.len() == 2,
                (1, Pdu::ListPanesCoherentResponse(response)) => matches!(
                    response.outcome,
                    codec::ListPanesCoherentOutcome::Snapshot(ref snapshot)
                        if snapshot.panes.tabs.len() == 2
                ),
                (2, Pdu::ListPanesOrderedV1Response(response)) => matches!(
                    response.outcome,
                    codec::ListPanesOrderedV1Outcome::Unsupported {
                        supported: TopologyCapabilities::SERVER_SUPPORTED
                    }
                ),
                (_, response) => panic!("discovery returned an unexpected PDU: {response:?}"),
            });
            if index == 2 {
                // The ordered protocol is intentionally dormant on the live
                // server. Exercise the real client's coherent fallback without
                // advertising capability bits solely to make this test pass.
                client.wire.send(
                    4,
                    Pdu::ListPanesCoherent(ListPanesCoherent {
                        supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                        required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                    }),
                );
                let fallback = client.receive(&executor);
                assert_eq!(fallback.serial, 4);
                assert!(matches!(
                    fallback.pdu,
                    Pdu::ListPanesCoherentResponse(ref response)
                        if matches!(&response.outcome,
                            codec::ListPanesCoherentOutcome::Snapshot(snapshot)
                                if snapshot.panes.tabs.len() == 2)
                ));
            }
            expect_snapshot(
                client.snapshot(&executor, 5, background.id),
                &[
                    ("status", "background-before-connect"),
                    ("branch", "feature/unrendered"),
                ],
            );
            expect_snapshot(
                client.snapshot(&executor, 6, foreground.id),
                &[("status", "foreground-before-connect")],
            );
            client.assert_quiet(&executor);
            client.disconnect(&executor);
        }
        assert_eq!(foreground.render_reads.load(Ordering::Relaxed), 0);
        assert_eq!(background.render_reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn background_changes_resync_reconnect_and_removal_use_current_state() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _scoped = ScopedMux::install(&mux);
        let pane = UserVarPane::new(&mux, 8_010);
        let erased: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&erased).expect("register background pane");
        pane.set("a-evicted", "stale");
        pane.set("status", "initial");

        let mut client = WireSession::connect(&executor);
        expect_snapshot(
            client.snapshot(&executor, 3, pane.id),
            &[("a-evicted", "stale"), ("status", "initial")],
        );
        // Equal-valued live occurrences are still distinct Lua-visible events.
        for value in ["changed", "changed", "latest"] {
            pane.set("status", value);
        }
        for expected in ["changed", "changed", "latest"] {
            expect_user_var(client.receive(&executor), pane.id, expected);
        }
        expect_snapshot(
            client.snapshot(&executor, 4, pane.id),
            &[("a-evicted", "stale"), ("status", "latest")],
        );
        client.disconnect(&executor);

        // The real OSC parser evicts the lexicographically first old key when
        // its configured map is full. Reconnect must replace, not merge, this
        // map even though no connected client observed the change.
        pane.set("z-retained", "new-key");
        pane.set("status", "while-disconnected");
        let mut reconnected = WireSession::connect(&executor);
        expect_snapshot(
            reconnected.snapshot(&executor, 3, pane.id),
            &[("status", "while-disconnected"), ("z-retained", "new-key")],
        );
        reconnected.assert_quiet(&executor);
        mux.remove_pane(pane.id);
        let removed = reconnected.receive(&executor);
        assert_eq!(removed.serial, 0);
        assert!(matches!(removed.pdu, Pdu::PaneRemoved(ref removed) if removed.pane_id == pane.id));
        assert!(matches!(
            reconnected.snapshot(&executor, 4, pane.id),
            PaneUserVarsOutcomeV1::PaneGone
        ));
        reconnected.assert_quiet(&executor);
        reconnected.disconnect(&executor);
        assert_eq!(pane.render_reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn a_live_update_cannot_overtake_a_partially_written_snapshot() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _scoped = ScopedMux::install(&mux);
        let pane = UserVarPane::new(&mux, 8_020);
        let erased: Arc<dyn Pane> = pane.clone();
        mux.add_pane(&erased).expect("register background pane");
        pane.set("status", "snapshot-value");
        let mut client = WireSession::connect(&executor);
        client.wire.set_write_budget(Some(1));
        client.wire.send(
            3,
            Pdu::GetPaneUserVarsV1(GetPaneUserVarsV1 { pane_id: pane.id }),
        );
        for _ in 0..MAX_PROGRESS_POLLS {
            assert!(client.poll(&executor).is_pending());
            if !client.wire.0.lock().replies.is_empty() {
                break;
            }
        }
        assert_eq!(client.wire.0.lock().replies.len(), 1);
        pane.set("status", "after-snapshot");
        client.wire.set_write_budget(None);
        let snapshot = client.receive(&executor);
        assert_eq!(snapshot.serial, 3);
        let Pdu::GetPaneUserVarsV1Response(snapshot) = snapshot.pdu else {
            panic!("snapshot reply must finish before the new alert");
        };
        expect_snapshot(snapshot.outcome, &[("status", "snapshot-value")]);
        expect_user_var(client.receive(&executor), pane.id, "after-snapshot");
        expect_snapshot(
            client.snapshot(&executor, 4, pane.id),
            &[("status", "after-snapshot")],
        );
        client.assert_quiet(&executor);
        client.disconnect(&executor);
        assert_eq!(pane.render_reads.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn metadata_bootstrap_exceeds_512_panes_without_render_registration_or_pushes() {
        let _lock = GLOBAL_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let executor = SimpleExecutor::new();
        let mux = Arc::new(Mux::new(None));
        let _scoped = ScopedMux::install(&mux);
        let window = mux.new_empty_window(None, None);
        let panes: Vec<_> = (0..600)
            .map(|index| {
                let pane = UserVarPane::new(&mux, 10_000 + index);
                add_tab(&mux, *window, &pane);
                pane.set("status", &format!("before-attach-{index}"));
                pane
            })
            .collect();
        drop(window);

        let mut client = WireSession::connect(&executor);
        client.wire.send(3, Pdu::ListPanes(ListPanes {}));
        let listing = client.receive(&executor);
        assert_eq!(listing.serial, 3);
        assert!(
            matches!(listing.pdu, Pdu::ListPanesResponse(ref response) if response.tabs.len() == 600)
        );
        let mut serial = 4;
        for _ in 0..2 {
            for (index, pane) in panes.iter().enumerate() {
                let expected = format!("before-attach-{index}");
                expect_snapshot(
                    client.snapshot(&executor, serial, pane.id),
                    &[("status", &expected)],
                );
                serial += 1;
            }
        }
        client.assert_quiet(&executor);
        client.disconnect(&executor);
        assert!(
            panes
                .iter()
                .all(|pane| pane.render_reads.load(Ordering::Relaxed) == 0),
            "reading metadata for 600 panes must not start any render stream"
        );
    }
}
