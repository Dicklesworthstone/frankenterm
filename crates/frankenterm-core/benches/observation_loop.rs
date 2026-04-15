//! Criterion benchmarks for observation loop overhead (wa-1m7nk).
//!
//! Measures:
//! - Single event dispatch latency (channel send → recv)
//! - Multi-channel select resolution latency (N=2,4,8,16 channels)
//! - Adaptive sleep overhead (cx.sleep cost during idle periods)
//! - Event throughput (events/sec sustained through channels)
//! - RwLock acquisition overhead (simulates registry/cursor access)
//!
//! All benchmarks run under asupersync to measure the actual async primitive
//! costs used by the observation runtime.

#![cfg(feature = "asupersync-runtime")]

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::runtime_compat;
use std::hint::black_box;
use std::time::Duration;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "observation_loop/single_event_dispatch",
        budget: "< 10µs per event dispatch (send + recv on asupersync mpsc)",
    },
    bench_common::BenchBudget {
        name: "observation_loop/multi_channel_select",
        budget: "< 50µs for N-channel poll resolution",
    },
    bench_common::BenchBudget {
        name: "observation_loop/sleep_overhead",
        budget: "< 5µs per cx.sleep() call overhead (excluding virtual wait)",
    },
    bench_common::BenchBudget {
        name: "observation_loop/event_throughput",
        budget: "> 100K events/sec sustained throughput",
    },
];

/// Benchmark: single event dispatch latency through asupersync mpsc channel.
fn bench_single_event_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("observation_loop/single_event_dispatch");

    group.bench_function("send_recv", |b| {
        b.iter(|| {
            let runtime = runtime_compat::RuntimeBuilder::current_thread()
                .build()
                .expect("build runtime");
            runtime.block_on(async {
                let cx = frankenterm_core::cx::for_request();
                let (tx, mut rx) = runtime_compat::mpsc::channel::<u64>(64);
                for i in 0..100u64 {
                    tx.send(&cx, i).await.expect("send");
                }
                for _ in 0..100 {
                    let val = rx.recv(&cx).await.expect("recv");
                    black_box(val);
                }
            });
        });
    });

    group.finish();
}

/// Benchmark: multi-channel resolution latency. Simulates the pattern where
/// the observation loop checks multiple channels for events.
fn bench_multi_channel_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("observation_loop/multi_channel_select");

    for n_channels in [2, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::new("channels", n_channels),
            &n_channels,
            |b, &n| {
                b.iter(|| {
                    let runtime = runtime_compat::RuntimeBuilder::current_thread()
                        .build()
                        .expect("build runtime");
                    runtime.block_on(async {
                        let cx = frankenterm_core::cx::for_request();
                        let mut channels: Vec<(
                            runtime_compat::mpsc::Sender<u32>,
                            runtime_compat::mpsc::Receiver<u32>,
                        )> = (0..n).map(|_| runtime_compat::mpsc::channel(16)).collect();

                        // Send one event on each channel
                        for (i, (tx, _)) in channels.iter().enumerate() {
                            tx.send(&cx, i as u32).await.expect("send");
                        }

                        // Poll each channel (simulating select across N channels)
                        for (_, rx) in channels.iter_mut() {
                            let val = rx.recv(&cx).await.expect("recv");
                            black_box(val);
                        }
                    });
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: sleep overhead for the adaptive polling loop.
fn bench_sleep_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("observation_loop/sleep_overhead");

    group.bench_function("sleep_1ms", |b| {
        let runtime = runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        b.iter(|| {
            runtime.block_on(async {
                runtime_compat::sleep(Duration::from_millis(1)).await;
            });
        });
    });

    group.finish();
}

/// Benchmark: sustained event throughput through the channel pipeline.
fn bench_event_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("observation_loop/event_throughput");

    for batch_size in [100u64, 1000, 10_000] {
        group.throughput(Throughput::Elements(batch_size));
        group.bench_with_input(
            BenchmarkId::new("events", batch_size),
            &batch_size,
            |b, &n| {
                b.iter(|| {
                    let runtime = runtime_compat::RuntimeBuilder::current_thread()
                        .build()
                        .expect("build runtime");
                    runtime.block_on(async {
                        let cx = frankenterm_core::cx::for_request();
                        let (tx, mut rx) =
                            runtime_compat::mpsc::channel::<u64>((n as usize).min(8192));

                        // Producer sends all events
                        for i in 0..n {
                            tx.send(&cx, i).await.expect("send");
                        }
                        drop(tx);

                        // Consumer drains all events
                        let mut count = 0u64;
                        while rx.recv(&cx).await.is_ok() {
                            count += 1;
                        }
                        black_box(count);
                        assert_eq!(count, n);
                    });
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: RwLock acquisition overhead (simulates registry/cursor access).
fn bench_rwlock_acquisition(c: &mut Criterion) {
    let mut group = c.benchmark_group("observation_loop/rwlock");

    group.bench_function("read_uncontended", |b| {
        let runtime = runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let lock = std::sync::Arc::new(runtime_compat::RwLock::new(42u64));
        b.iter(|| {
            let lock = lock.clone();
            runtime.block_on(async {
                let guard = lock.read().await;
                black_box(*guard);
            });
        });
    });

    group.bench_function("write_uncontended", |b| {
        let runtime = runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .expect("build runtime");
        let lock = std::sync::Arc::new(runtime_compat::RwLock::new(42u64));
        b.iter(|| {
            let lock = lock.clone();
            runtime.block_on(async {
                let mut guard = lock.write().await;
                *guard += 1;
                black_box(*guard);
            });
        });
    });

    group.finish();
}

fn custom_criterion() -> Criterion {
    bench_common::emit_bench_artifacts("observation_loop", BUDGETS);
    Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3))
}

criterion_group! {
    name = benches;
    config = custom_criterion();
    targets = bench_single_event_dispatch, bench_multi_channel_select, bench_sleep_overhead, bench_event_throughput, bench_rwlock_acquisition
}

criterion_main!(benches);
