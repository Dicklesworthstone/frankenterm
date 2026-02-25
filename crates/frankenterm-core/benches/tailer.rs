#![cfg(feature = "asupersync-runtime")]

//! Benchmarks for tailer capture throughput and timeout/channel overhead.
//!
//! These benches support `ft-124z4` by providing a focused perf harness for:
//! - single-pane capture rounds at varying payload sizes
//! - concurrent capture rounds with bounded `max_concurrent`
//! - two-phase capture-event channel reserve/send/recv path
//! - timeout wrapper overhead on a ready future

use std::collections::HashMap;
use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::cx;
use frankenterm_core::ingest::{PaneCursor, PaneRegistry};
use frankenterm_core::runtime_compat::{self, CompatRuntime, RwLock, mpsc};
use frankenterm_core::tailer::{TailerConfig, TailerPollTaskSet, TailerSupervisor};
use frankenterm_core::wezterm::{PaneInfo, PaneTextSource};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "tailer/single_capture_round",
        budget: "single-pane capture round throughput across payload sizes",
    },
    bench_common::BenchBudget {
        name: "tailer/concurrent_capture_round",
        budget: "bounded-concurrency capture round throughput (max_concurrent=1,2,4,8)",
    },
    bench_common::BenchBudget {
        name: "tailer/capture_event_channel",
        budget: "two-phase reserve/send/recv overhead for capture event channel",
    },
    bench_common::BenchBudget {
        name: "tailer/timeout_overhead",
        budget: "runtime_compat timeout wrapper overhead on already-ready futures",
    },
];

type PaneFuture<'a> = Pin<Box<dyn Future<Output = frankenterm_core::Result<String>> + Send + 'a>>;

#[derive(Clone)]
struct FixedPayloadSource {
    payload: Arc<String>,
}

impl FixedPayloadSource {
    fn new(payload: Arc<String>) -> Self {
        Self { payload }
    }
}

impl PaneTextSource for FixedPayloadSource {
    type Fut<'a> = PaneFuture<'a>;

    fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
        let payload = Arc::clone(&self.payload);
        Box::pin(async move { Ok((*payload).clone()) })
    }
}

fn make_pane(pane_id: u64) -> PaneInfo {
    PaneInfo {
        pane_id,
        tab_id: 0,
        window_id: 0,
        domain_id: None,
        domain_name: None,
        workspace: None,
        size: None,
        rows: Some(24),
        cols: Some(80),
        title: None,
        cwd: None,
        tty_name: None,
        cursor_x: Some(0),
        cursor_y: Some(0),
        cursor_visibility: None,
        left_col: None,
        top_row: None,
        is_active: pane_id == 1,
        is_zoomed: false,
        extra: std::collections::HashMap::new(),
    }
}

fn make_panes(count: u64) -> HashMap<u64, PaneInfo> {
    (1..=count)
        .map(|pane_id| (pane_id, make_pane(pane_id)))
        .collect()
}

fn build_runtime() -> runtime_compat::Runtime {
    runtime_compat::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .expect("build runtime_compat benchmark runtime")
}

async fn run_capture_round(payload: Arc<String>, pane_count: u64, max_concurrent: usize) -> usize {
    let source = Arc::new(FixedPayloadSource::new(payload));
    let (tx, mut rx) = mpsc::channel((pane_count as usize).saturating_mul(2).max(8));
    let cursors = Arc::new(RwLock::new(HashMap::<u64, PaneCursor>::new()));
    let registry = Arc::new(RwLock::new(PaneRegistry::new()));
    let shutdown = Arc::new(AtomicBool::new(false));

    {
        let mut guard = cursors.write().await;
        for pane_id in 1..=pane_count {
            guard.insert(pane_id, PaneCursor::new(pane_id));
        }
    }

    let mut supervisor = TailerSupervisor::new(
        TailerConfig {
            min_interval: Duration::ZERO,
            max_interval: Duration::from_millis(1),
            max_concurrent,
            send_timeout: Duration::from_millis(200),
            capture_timeout: Duration::from_secs(1),
            ..Default::default()
        },
        tx,
        cursors,
        registry,
        shutdown,
        source,
    );
    supervisor.sync_tailers(&make_panes(pane_count));

    let mut poll_tasks = TailerPollTaskSet::new();
    supervisor.spawn_ready(&mut poll_tasks);
    while let Some((pane_id, outcome)) = poll_tasks.join_next().await {
        supervisor.handle_poll_result(pane_id, outcome);
    }

    let expected_events = supervisor.metrics().events_sent as usize;
    let mut delivered = 0_usize;
    while delivered < expected_events {
        let _ = runtime_compat::mpsc_recv_option(&mut rx)
            .await
            .expect("expected captured event");
        delivered += 1;
    }

    delivered
}

fn bench_single_capture_round(c: &mut Criterion) {
    let runtime = build_runtime();
    let mut group = c.benchmark_group("tailer/single_capture_round");

    for size in [128_usize, 1024, 16384, 65536] {
        let payload = Arc::new("x".repeat(size));
        group.bench_with_input(
            BenchmarkId::new("payload_bytes", size),
            &payload,
            |b, payload| {
                b.iter(|| {
                    let delivered = runtime.block_on(run_capture_round(Arc::clone(payload), 1, 1));
                    black_box(delivered);
                });
            },
        );
    }

    group.finish();
}

fn bench_concurrent_capture_round(c: &mut Criterion) {
    let runtime = build_runtime();
    let payload = Arc::new("y".repeat(1024));
    let mut group = c.benchmark_group("tailer/concurrent_capture_round");

    for max_concurrent in [1_usize, 2, 4, 8] {
        group.bench_with_input(
            BenchmarkId::new("max_concurrent", max_concurrent),
            &max_concurrent,
            |b, &max_concurrent| {
                b.iter(|| {
                    let delivered = runtime.block_on(run_capture_round(
                        Arc::clone(&payload),
                        max_concurrent as u64,
                        max_concurrent,
                    ));
                    black_box(delivered);
                });
            },
        );
    }

    group.finish();
}

fn bench_capture_event_channel(c: &mut Criterion) {
    let runtime = build_runtime();
    let mut group = c.benchmark_group("tailer/capture_event_channel");

    group.bench_function("reserve_send_recv", |b| {
        b.iter(|| {
            let value = runtime.block_on(async {
                let (tx, mut rx) = mpsc::channel(1);

                #[cfg(feature = "asupersync-runtime")]
                {
                    let reserve_cx = cx::for_testing();
                    let permit = tx.reserve(&reserve_cx).await.expect("reserve");
                    permit.send(7_u64);
                }

                #[cfg(not(feature = "asupersync-runtime"))]
                {
                    let permit = tx.reserve().await.expect("reserve");
                    permit.send(7_u64);
                }

                runtime_compat::mpsc_recv_option(&mut rx)
                    .await
                    .expect("recv")
            });
            black_box(value);
        });
    });

    group.finish();
}

fn bench_timeout_overhead(c: &mut Criterion) {
    let runtime = build_runtime();
    let mut group = c.benchmark_group("tailer/timeout_overhead");

    group.bench_function("ready_future", |b| {
        b.iter(|| {
            let value = runtime.block_on(async {
                runtime_compat::timeout(Duration::from_millis(10), async { 1_u64 })
                    .await
                    .expect("timeout should succeed for ready future")
            });
            black_box(value);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("tailer", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_single_capture_round,
        bench_concurrent_capture_round,
        bench_capture_event_channel,
        bench_timeout_overhead
);
criterion_main!(benches);
