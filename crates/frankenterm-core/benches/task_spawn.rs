#![cfg(feature = "asupersync-runtime")]

//! Benchmarks for structured task spawning patterns (`wa-1bznu`).
//!
//! Tracks:
//! - single-task spawn latency
//! - bounded region fanout overhead
//! - flat spawn fanout overhead
//! - comparison against equivalent tokio spawn/join workloads

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::cx::{
    Cx, CxRuntimeBuilder, RuntimeTuning, for_testing, spawn_bounded_with_cx, spawn_with_cx,
};

mod bench_common;

const FANOUT_SIZES: [usize; 4] = [1, 10, 100, 1000];
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "task_spawn/scope_spawn_noop",
        budget: "single asupersync spawn+join no-op task latency",
    },
    bench_common::BenchBudget {
        name: "task_spawn/tokio_spawn_noop",
        budget: "single tokio spawn+join no-op task latency",
    },
    bench_common::BenchBudget {
        name: "task_spawn/region_batch",
        budget: "region-style bounded fanout spawn/join overhead",
    },
    bench_common::BenchBudget {
        name: "task_spawn/structured_overhead",
        budget: "region-bounded fanout overhead vs flat spawn fanout",
    },
];

fn build_asupersync_runtime() -> asupersync::runtime::Runtime {
    CxRuntimeBuilder::current_thread()
        .with_tuning(RuntimeTuning {
            worker_threads: 1,
            poll_budget: 128,
            blocking_min_threads: 0,
            blocking_max_threads: 0,
        })
        .build()
        .expect("build asupersync benchmark runtime")
}

fn build_tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio benchmark runtime")
}

fn bench_spawn_noop(c: &mut Criterion) {
    let as_runtime = build_asupersync_runtime();
    let as_handle = as_runtime.handle();
    let as_cx = for_testing();
    c.bench_function("task_spawn/scope_spawn_noop", |b| {
        b.iter(|| {
            let join = spawn_with_cx(&as_handle, &as_cx, |_child_cx| async move { 1_u8 });
            let value = as_runtime.block_on(join);
            black_box(value);
        });
    });

    let tokio_runtime = build_tokio_runtime();
    c.bench_function("task_spawn/tokio_spawn_noop", |b| {
        b.iter(|| {
            let value = tokio_runtime.block_on(async {
                tokio::spawn(async move { 1_u8 })
                    .await
                    .expect("tokio join should succeed")
            });
            black_box(value);
        });
    });
}

fn bench_region_batch(c: &mut Criterion) {
    let as_runtime = build_asupersync_runtime();
    let as_handle = as_runtime.handle();
    let as_cx = for_testing();
    let tokio_runtime = build_tokio_runtime();

    let mut group = c.benchmark_group("task_spawn/region_batch");
    for fanout in FANOUT_SIZES {
        group.bench_with_input(
            BenchmarkId::new("asupersync_bounded", fanout),
            &fanout,
            |b, &n| {
                b.iter(|| {
                    let tasks: Vec<_> = (0..n)
                        .map(|_| |_child_cx: Cx| async move { 1_u8 })
                        .collect();
                    let results = as_runtime
                        .block_on(spawn_bounded_with_cx(&as_handle, &as_cx, n.max(1), tasks))
                        .expect("bounded spawn benchmark should succeed");
                    black_box(results.len());
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("tokio_spawn_join_all", fanout),
            &fanout,
            |b, &n| {
                b.iter(|| {
                    let completed = tokio_runtime.block_on(async {
                        let mut joins = Vec::with_capacity(n);
                        for _ in 0..n {
                            joins.push(tokio::spawn(async move { 1_u8 }));
                        }

                        let mut completed = 0_usize;
                        for join in joins {
                            let _ = join.await.expect("tokio join should succeed");
                            completed += 1;
                        }
                        completed
                    });
                    black_box(completed);
                });
            },
        );
    }
    group.finish();
}

fn bench_structured_overhead(c: &mut Criterion) {
    let runtime = build_asupersync_runtime();
    let handle = runtime.handle();
    let cx = for_testing();

    let mut group = c.benchmark_group("task_spawn/structured_overhead");
    for fanout in FANOUT_SIZES {
        group.bench_with_input(
            BenchmarkId::new("region_bounded", fanout),
            &fanout,
            |b, &n| {
                b.iter(|| {
                    let tasks: Vec<_> = (0..n)
                        .map(|_| |_child_cx: Cx| async move { 1_u8 })
                        .collect();
                    let completed = runtime
                        .block_on(spawn_bounded_with_cx(&handle, &cx, n.max(1), tasks))
                        .expect("bounded fanout should succeed")
                        .len();
                    black_box(completed);
                });
            },
        );

        group.bench_with_input(BenchmarkId::new("flat_spawn", fanout), &fanout, |b, &n| {
            b.iter(|| {
                let completed = runtime.block_on(async {
                    let mut joins = Vec::with_capacity(n);
                    for _ in 0..n {
                        joins.push(spawn_with_cx(&handle, &cx, |_child_cx| async move { 1_u8 }));
                    }

                    let mut completed = 0_usize;
                    for join in joins {
                        let _ = join.await;
                        completed += 1;
                    }
                    completed
                });
                black_box(completed);
            });
        });
    }
    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("task_spawn", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_spawn_noop, bench_region_batch, bench_structured_overhead
);
criterion_main!(benches);
