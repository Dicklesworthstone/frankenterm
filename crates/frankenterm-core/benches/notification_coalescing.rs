//! Benchmarks for notification coalescing (wa-x4rq).
//!
//! WezTerm can emit extremely high-frequency “pane dirty” notifications during
//! bursty output. The watcher should coalesce those notifications into fewer
//! capture operations.

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{
    BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "coalescing_throughput",
        budget: "Target >1M notifications/sec ingestion rate",
    },
    bench_common::BenchBudget {
        name: "drain_ready_latency",
        budget: "Target <100µs to scan 50 panes",
    },
    bench_common::BenchBudget {
        name: "coalescing_ratio",
        budget: "Target >20:1 notifications→captures under burst",
    },
];

#[derive(Debug, Clone)]
struct NotificationCoalescer {
    window_ms: u64,
    max_delay_ms: u64,
    // pane_id -> first_seen_ms
    pending: HashMap<u64, u64>,
}

impl NotificationCoalescer {
    fn new(window_ms: u64, max_delay_ms: u64) -> Self {
        Self {
            window_ms,
            max_delay_ms,
            pending: HashMap::new(),
        }
    }

    fn on_notification(&mut self, pane_id: u64, now_ms: u64) {
        self.pending.entry(pane_id).or_insert(now_ms);
    }

    fn drain_ready(&mut self, now_ms: u64) -> Vec<u64> {
        let mut ready = Vec::new();
        let mut due_ids = Vec::new();

        for (&pane_id, &first_seen_ms) in &self.pending {
            let age_ms = now_ms.saturating_sub(first_seen_ms);
            if age_ms >= self.window_ms || age_ms >= self.max_delay_ms {
                due_ids.push(pane_id);
            }
        }

        for pane_id in due_ids {
            self.pending.remove(&pane_id);
            ready.push(pane_id);
        }

        ready
    }
}

fn bench_coalescing_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("notification_coalescing");

    let pane_count = 50_u64;
    let notifications = 10_000_u64;
    group.throughput(Throughput::Elements(notifications));

    group.bench_with_input(
        BenchmarkId::new("coalescing_throughput", "10k_notifs_50_panes"),
        &(pane_count, notifications),
        |b, &(pane_count, notifications)| {
            b.iter_batched(
                || NotificationCoalescer::new(50, 200),
                |mut coalescer| {
                    for i in 0..notifications {
                        let pane_id = i % pane_count;
                        coalescer.on_notification(pane_id, 0);
                    }
                    black_box(coalescer.pending.len())
                },
                BatchSize::SmallInput,
            );
        },
    );

    group.finish();
}

fn bench_drain_ready_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("notification_coalescing");

    for &pane_count in &[10_u64, 50_u64, 200_u64] {
        group.throughput(Throughput::Elements(pane_count));
        group.bench_with_input(
            BenchmarkId::new("drain_ready_latency", pane_count),
            &pane_count,
            |b, &pane_count| {
                b.iter_batched(
                    || {
                        let mut coalescer = NotificationCoalescer::new(50, 200);
                        for pane_id in 0..pane_count {
                            coalescer.on_notification(pane_id, 0);
                        }
                        coalescer
                    },
                    |mut coalescer| {
                        let ready = coalescer.drain_ready(50);
                        black_box(ready.len())
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_coalescing_ratio(c: &mut Criterion) {
    let mut group = c.benchmark_group("notification_coalescing");

    let pane_count = 50_u64;
    let notifications_per_pane = 50_u64;
    let notifications = pane_count * notifications_per_pane;
    group.throughput(Throughput::Elements(notifications));

    group.bench_function("coalescing_ratio_burst", |b| {
        b.iter_batched(
            || NotificationCoalescer::new(50, 200),
            |mut coalescer| {
                // Burst: lots of notifications within the same 50ms window.
                for pane_id in 0..pane_count {
                    for _ in 0..notifications_per_pane {
                        coalescer.on_notification(pane_id, 0);
                    }
                }
                let ready = coalescer.drain_ready(50);
                let ratio = notifications as f64 / ready.len().max(1) as f64;
                black_box(ratio)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("notification_coalescing", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_coalescing_throughput,
        bench_drain_ready_latency,
        bench_coalescing_ratio
);
criterion_main!(benches);
