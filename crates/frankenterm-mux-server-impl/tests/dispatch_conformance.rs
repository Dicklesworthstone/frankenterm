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
use codec::{CompressionMode, DecodedPdu, Pdu, Ping};
use frankenterm_mux_server_impl::dispatch::{
    self, DispatchIoPreference, DispatchRuntimeConfig, DispatchStream, DispatchStreamKind,
};
use mux::Mux;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    fn new(script: Vec<u8>, stream_kind: DispatchStreamKind, chunk: usize) -> (Self, ScriptHandle) {
        let state = Arc::new(ScriptState {
            script,
            cursor: AtomicUsize::new(0),
            chunk,
            writes: StdMutex::new(Vec::new()),
            flush_count: AtomicUsize::new(0),
            writable_waits: AtomicUsize::new(0),
            readable_waits: AtomicUsize::new(0),
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
        self.state.writable_waits.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
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
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.state
            .writes
            .lock()
            .expect("writes lock")
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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
    let mut buffer = bytes.to_vec();
    while let Some(decoded) =
        Pdu::stream_decode(&mut buffer).map_err(|err| format!("stream_decode frame: {err:#}"))?
    {
        out.push(decoded);
    }
    if !buffer.is_empty() {
        return Err(format!(
            "dispatch conformance output ended with {} trailing undecoded byte(s): {:02x?}",
            buffer.len(),
            buffer
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
    let _lock = GLOBAL_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mux = Arc::new(Mux::new(None));
    let _scoped = ScopedMux::install(&mux);

    let (stream, handle) = ScriptedDispatchStream::new(script, stream_kind, read_chunk);

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
                baseline_flushes,
                "writable_waits should equal flush count for pref={pref:?} stream={stream_kind:?}"
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
                    baseline_flushes,
                    "writable waits diverged for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
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
                    handle.flush_count(),
                    "writable waits should track flushes for pref={pref:?} stream={stream_kind:?} chunk={chunk}"
                );
            }
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
