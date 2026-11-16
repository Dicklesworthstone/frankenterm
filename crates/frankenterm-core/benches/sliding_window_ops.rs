//! Benchmarks for `SlidingWindow` rate-monitoring hot-path.
//!
//! Performance budgets:
//! - record (single event): **< 20ns**
//! - count (full window query): **< 50ns** (60 buckets)
//! - record under time advance (bucket rotation): **< 30ns**
//! - snapshot: **< 200ns** (60 buckets)

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::sliding_window::SlidingWindow;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "sliding_window/record_same_bucket",
        budget: "< 20ns per record (no bucket rotation)",
    },
    bench_common::BenchBudget {
        name: "sliding_window/record_advancing",
        budget: "< 30ns per record (steady time advance)",
    },
    bench_common::BenchBudget {
        name: "sliding_window/count_query",
        budget: "< 50ns per count query (60 buckets)",
    },
    bench_common::BenchBudget {
        name: "sliding_window/snapshot",
        budget: "< 200ns per snapshot (60 buckets)",
    },
];

/// Bench: record() with all events hitting the same bucket (no rotation).
fn bench_record_same_bucket(c: &mut Criterion) {
    let mut group = c.benchmark_group("sliding_window");

    for &n_buckets in &[10_usize, 60, 300] {
        let events = 10_000_u64;
        group.throughput(Throughput::Elements(events));
        group.bench_with_input(
            BenchmarkId::new("record_same_bucket", n_buckets),
            &n_buckets,
            |b, &n_buckets| {
                b.iter(|| {
                    let mut sw = SlidingWindow::new(60_000, n_buckets);
                    let ts = 100_000_u64;
                    for _ in 0..events {
                        sw.record(ts);
                    }
                    black_box(sw.count(ts));
                });
            },
        );
    }

    group.finish();
}

/// Bench: record() with time advancing by 1 bucket per event (rotation path).
fn bench_record_advancing(c: &mut Criterion) {
    let mut group = c.benchmark_group("sliding_window");

    for &n_buckets in &[10_usize, 60, 300] {
        let events = 5_000_u64;
        let window_ms = 60_000_u64;
        let bucket_ms = window_ms / n_buckets as u64;
        group.throughput(Throughput::Elements(events));

        group.bench_with_input(
            BenchmarkId::new("record_advancing", n_buckets),
            &(n_buckets, bucket_ms),
            |b, &(n_buckets, bucket_ms)| {
                b.iter(|| {
                    let mut sw = SlidingWindow::new(window_ms, n_buckets);
                    let base = 100_000_u64;
                    for i in 0..events {
                        // Each event advances time by one bucket width
                        sw.record(base + i * bucket_ms);
                    }
                    black_box(sw.count(base + events * bucket_ms));
                });
            },
        );
    }

    group.finish();
}

/// Bench: count() query on a populated window at varying bucket counts.
fn bench_count_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("sliding_window");

    for &n_buckets in &[10_usize, 60, 300] {
        let window_ms = 60_000_u64;
        let bucket_ms = window_ms / n_buckets as u64;
        let queries = 10_000_u64;
        group.throughput(Throughput::Elements(queries));

        group.bench_with_input(
            BenchmarkId::new("count_query", n_buckets),
            &n_buckets,
            |b, &n_buckets| {
                // Pre-populate the window: one event per bucket
                let mut sw = SlidingWindow::new(window_ms, n_buckets);
                let base = 100_000_u64;
                for i in 0..n_buckets as u64 {
                    sw.record(base + i * bucket_ms);
                }
                let now = base + (n_buckets as u64 - 1) * bucket_ms;

                b.iter(|| {
                    let mut total = 0_u64;
                    for _ in 0..queries {
                        total += sw.count(now);
                    }
                    black_box(total);
                });
            },
        );
    }

    group.finish();
}

/// Bench: snapshot() on a populated window at varying bucket counts.
fn bench_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("sliding_window");

    for &n_buckets in &[10_usize, 60, 300] {
        let window_ms = 60_000_u64;
        let bucket_ms = window_ms / n_buckets as u64;

        group.bench_with_input(
            BenchmarkId::new("snapshot", n_buckets),
            &n_buckets,
            |b, &n_buckets| {
                let mut sw = SlidingWindow::new(window_ms, n_buckets);
                let base = 100_000_u64;
                for i in 0..n_buckets as u64 {
                    sw.record(base + i * bucket_ms);
                }
                let now = base + (n_buckets as u64 - 1) * bucket_ms;

                b.iter(|| {
                    black_box(sw.snapshot(now));
                });
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("sliding_window", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_record_same_bucket,
        bench_record_advancing,
        bench_count_query,
        bench_snapshot
);
criterion_main!(benches);
