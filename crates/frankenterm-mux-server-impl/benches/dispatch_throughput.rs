//! Criterion benchmark: mux dispatch UnixStream fast-path throughput.
//!
//! wa-283h4.18: measures Ping→Pong roundtrip throughput through
//! `process_with_config` across every `DispatchIoPreference`, using a
//! scripted in-memory stream that mimics UnixStream behavior (no TLS, no
//! handshake cost). A write-batching counter exposes the syscall-reduction
//! signal that the io_uring fast path in wa-283h4.17 is expected to improve.
//!
//! This bench intentionally does **not** exercise AsyncSslStream — TLS
//! amortizes through a different read/write path and would conflate the
//! fast-path signal.

use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use codec::{Pdu, Ping};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_mux_server_impl::dispatch::{
    self, DispatchIoPreference, DispatchRuntimeConfig, DispatchStream, DispatchStreamKind,
};
use mux::Mux;
use std::future::Future;
use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll};

static GLOBAL_STATE_BENCH_LOCK: StdMutex<()> = StdMutex::new(());

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
struct BenchStreamState {
    script: Vec<u8>,
    cursor: AtomicUsize,
    bytes_written: AtomicUsize,
    flush_count: AtomicUsize,
}

#[derive(Debug)]
struct BenchStream {
    state: Arc<BenchStreamState>,
}

impl BenchStream {
    fn new(script: Vec<u8>) -> (Self, Arc<BenchStreamState>) {
        let state = Arc::new(BenchStreamState {
            script,
            cursor: AtomicUsize::new(0),
            bytes_written: AtomicUsize::new(0),
            flush_count: AtomicUsize::new(0),
        });
        (
            Self {
                state: Arc::clone(&state),
            },
            state,
        )
    }
}

impl DispatchStream for BenchStream {
    fn dispatch_stream_kind(&self) -> DispatchStreamKind {
        DispatchStreamKind::Unix
    }

    fn wait_for_readable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn wait_for_writable(&self) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }
}

impl AsyncRead for BenchStream {
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
        let want = buf.remaining().min(available);
        if want == 0 {
            return Poll::Ready(Ok(()));
        }
        buf.put_slice(&state.script[cursor..cursor + want]);
        state.cursor.store(cursor + want, Ordering::Relaxed);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for BenchStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.state
            .bytes_written
            .fetch_add(buf.len(), Ordering::Relaxed);
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

fn build_ping_script(n: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(n as usize * 3);
    for serial in 1..=n {
        Pdu::Ping(Ping {})
            .encode(&mut buf, serial)
            .expect("encode ping");
    }
    buf
}

fn run(preference: DispatchIoPreference, script: &[u8]) -> (usize, usize) {
    let _lock = GLOBAL_STATE_BENCH_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let mux = Arc::new(Mux::new(None));
    let _scoped = ScopedMux::install(&mux);

    let (stream, state) = BenchStream::new(script.to_vec());
    let result = promise::spawn::block_on(dispatch::process_with_config(
        stream,
        DispatchRuntimeConfig::new(preference),
    ));
    result.expect("dispatch loop clean exit");
    (
        state.bytes_written.load(Ordering::Relaxed),
        state.flush_count.load(Ordering::Relaxed),
    )
}

fn bench_throughput(c: &mut Criterion) {
    const BATCH_SIZES: &[u64] = &[1, 16, 128, 1024];
    const PREFERENCES: &[(&str, DispatchIoPreference)] = &[
        ("auto", DispatchIoPreference::Auto),
        ("io_uring", DispatchIoPreference::IoUring),
        ("epoll", DispatchIoPreference::Epoll),
        ("kqueue", DispatchIoPreference::Kqueue),
        ("poll", DispatchIoPreference::Poll),
    ];

    let mut group = c.benchmark_group("mux_dispatch_ping_pong");
    for &batch in BATCH_SIZES {
        let script = build_ping_script(batch);
        // Each Ping encodes to 3 bytes; throughput counts script in + pong out.
        let bytes_per_iter = (script.len() as u64) * 2;
        group.throughput(Throughput::Bytes(bytes_per_iter));
        for (name, pref) in PREFERENCES {
            let id = BenchmarkId::new(*name, batch);
            group.bench_with_input(id, &(*pref, script.clone()), |b, (pref, script)| {
                b.iter(|| {
                    let (written, flushes) = run(*pref, script);
                    black_box((written, flushes));
                });
            });
        }
    }
    group.finish();
}

fn bench_flush_amortization(c: &mut Criterion) {
    // Records flush_count per N-Ping iteration. This isolates the
    // "syscall reduction" signal — io_uring should allow write batching
    // to reduce flushes per PDU once wa-283h4.17 wires the fast path.
    let script = build_ping_script(128);
    c.bench_function("mux_dispatch_flush_amortization_128", |b| {
        b.iter(|| {
            let (_, flushes) = run(DispatchIoPreference::Auto, &script);
            black_box(flushes);
        });
    });
}

criterion_group!(benches, bench_throughput, bench_flush_amortization);
criterion_main!(benches);
