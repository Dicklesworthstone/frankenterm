//! Criterion benchmarks for mux dispatch and bounded delivery hot paths.
//!
//! wa-283h4.18: measures Ping→Pong roundtrip throughput through
//! `process_with_config` across every `DispatchIoPreference`, using a
//! scripted in-memory stream that mimics UnixStream behavior (no TLS, no
//! handshake cost). A write-batching counter exposes the syscall-reduction
//! signal that the io_uring fast path in wa-283h4.17 is expected to improve.
//!
//! ft-interactive-systems-performance-4tenz.5.5.11 measures the delivery
//! ledger and keyed scheduler at 1, 16, 256, 4,096, and 16,384 tracked keys.
//! Setup, snapshot construction, and destruction are outside the timed region,
//! isolating publish, close, retry, reclaim, resync selection, and next-ready
//! bookkeeping. These microbenchmarks do **not** prove end-to-end keypress,
//! renderer, wire, snapshot-build, or application-ACK latency; target-class
//! M4/M5 and Threadripper artifacts remain separate campaign gates.
//!
//! The dispatch bench intentionally does **not** exercise AsyncSslStream — TLS
//! amortizes through a different read/write path and would conflate the
//! fast-path signal.

use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use codec::{Pdu, Ping};
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_mux_server_impl::delivery_ledger::{
    ClosePaneOutcome, DeliveryClaim, DeliveryGeneration, DeliveryLedger, DirtyOutcome,
    PaneCloseAckToken, SettleOutcome,
};
use frankenterm_mux_server_impl::delivery_scheduler::{
    AdmissionOutcome, DeliveryScheduler, ScheduledItem, SchedulerLimits,
};
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

const DELIVERY_SCALE_COUNTS: &[usize] = &[1, 16, 256, 4096, 16_384];

type BenchDeliveryScheduler = DeliveryScheduler<usize, usize, usize, usize, usize, usize>;

fn dirty_delivery_ledger(panes: usize) -> DeliveryLedger {
    let mut ledger = DeliveryLedger::new(DeliveryGeneration::new(1), panes)
        .expect("delivery benchmark ledger identity must be available");
    for pane_id in 0..panes {
        assert_eq!(ledger.mark_dirty(pane_id), DirtyOutcome::BecameDirty);
    }
    ledger
}

fn settled_delivery_ledger(panes: usize) -> DeliveryLedger {
    let mut ledger = dirty_delivery_ledger(panes);
    for _ in 0..panes {
        let claim = ledger
            .claim_next()
            .expect("delivery benchmark token must be available")
            .expect("dirty benchmark pane must be claimable");
        assert_eq!(ledger.commit(claim), SettleOutcome::CommittedClean);
    }
    ledger
}

fn close_dirty_middle(panes: usize) -> (DeliveryLedger, usize, PaneCloseAckToken) {
    let mut ledger = dirty_delivery_ledger(panes);
    let pane_id = panes / 2;
    let close_ack = match ledger.close_pane(pane_id) {
        ClosePaneOutcome::ClosedDirty { close_ack } => close_ack,
        outcome => panic!("expected dirty benchmark close, got {outcome:?}"),
    };
    (ledger, pane_id, close_ack)
}

fn claimed_dirty_head(panes: usize) -> (DeliveryLedger, DeliveryClaim) {
    let mut ledger = dirty_delivery_ledger(panes);
    let claim = ledger
        .claim_next()
        .expect("delivery benchmark token must be available")
        .expect("dirty benchmark head must be claimable");
    (ledger, claim)
}

fn populated_render_scheduler(keys: usize) -> BenchDeliveryScheduler {
    let mut scheduler = DeliveryScheduler::new(SchedulerLimits::new(0, 0, keys, 0))
        .expect("delivery benchmark capacities must be representable");
    for key in 0..keys {
        assert_eq!(
            scheduler.admit_render(key, key).outcome(),
            AdmissionOutcome::Admitted
        );
    }
    scheduler
}

fn render_resync_scheduler(keys: usize) -> BenchDeliveryScheduler {
    let mut scheduler = populated_render_scheduler(keys);
    assert_eq!(
        scheduler.admit_render(keys, keys).outcome(),
        AdmissionOutcome::Escalated
    );
    scheduler
}

fn retired_render_scheduler(keys: usize) -> BenchDeliveryScheduler {
    let mut scheduler = render_resync_scheduler(keys);
    assert!(matches!(
        scheduler.pop_next(),
        Some(ScheduledItem::RenderResync)
    ));
    scheduler
}

fn bench_delivery_ledger_hot_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_delivery_ledger_hot_paths");
    group.throughput(Throughput::Elements(1));

    for &panes in DELIVERY_SCALE_COUNTS {
        group.bench_with_input(
            BenchmarkId::new("dirty_claim_commit", panes),
            &panes,
            |b, &panes| {
                let mut ledger = settled_delivery_ledger(panes);
                let pane_id = panes / 2;
                b.iter(|| {
                    let dirty = ledger.mark_dirty(pane_id);
                    let claim = ledger
                        .claim_next()
                        .expect("benchmark token must be available")
                        .expect("published benchmark pane must be claimable");
                    let settled = ledger.commit(claim);
                    assert_eq!(dirty, DirtyOutcome::BecameDirty);
                    assert_eq!(settled, SettleOutcome::CommittedClean);
                    black_box(());
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("close_middle", panes),
            &panes,
            |b, &panes| {
                b.iter_batched_ref(
                    || dirty_delivery_ledger(panes),
                    |ledger| ledger.close_pane(panes / 2),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("ack_reclaim_middle", panes),
            &panes,
            |b, &panes| {
                b.iter_batched_ref(
                    || close_dirty_middle(panes),
                    |(ledger, pane_id, close_ack)| {
                        ledger.acknowledge_pane_close(*pane_id, *close_ack)
                    },
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("retry_requeue", panes),
            &panes,
            |b, &panes| {
                b.iter_batched_ref(
                    || claimed_dirty_head(panes),
                    |(ledger, claim)| ledger.retry(*claim),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("next_ready", panes),
            &panes,
            |b, &panes| {
                b.iter_batched_ref(
                    || dirty_delivery_ledger(panes),
                    |ledger| ledger.claim_next(),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("resync_select", panes),
            &panes,
            |b, &panes| {
                b.iter_batched_ref(
                    || dirty_delivery_ledger(panes),
                    |ledger| {
                        let request = ledger.request_resync_all();
                        let claim = ledger.claim_next();
                        black_box((request, claim))
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

fn bench_delivery_scheduler_hot_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("mux_delivery_scheduler_hot_paths");
    group.throughput(Throughput::Elements(1));

    for &keys in DELIVERY_SCALE_COUNTS {
        group.bench_with_input(
            BenchmarkId::new("stable_key_replace", keys),
            &keys,
            |b, &keys| {
                let mut scheduler = populated_render_scheduler(keys);
                let key = keys / 2;
                b.iter(|| black_box(scheduler.admit_render(key, key)));
            },
        );
        group.bench_with_input(BenchmarkId::new("next_ready", keys), &keys, |b, &keys| {
            b.iter_batched_ref(
                || populated_render_scheduler(keys),
                |scheduler| scheduler.pop_next(),
                BatchSize::LargeInput,
            );
        });
        group.bench_with_input(
            BenchmarkId::new("resync_select", keys),
            &keys,
            |b, &keys| {
                b.iter_batched_ref(
                    || render_resync_scheduler(keys),
                    |scheduler| scheduler.pop_next(),
                    BatchSize::LargeInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("reactivate_retired_key", keys),
            &keys,
            |b, &keys| {
                b.iter_batched_ref(
                    || retired_render_scheduler(keys),
                    |scheduler| scheduler.admit_render(keys / 2, keys),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_throughput,
    bench_flush_amortization,
    bench_delivery_ledger_hot_paths,
    bench_delivery_scheduler_hot_paths
);
criterion_main!(benches);
