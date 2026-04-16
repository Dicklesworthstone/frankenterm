//! Criterion bench — full asupersync-stack integration throughput
//! (wa-2ojrv).
//!
//! Measures the four workloads the bead's bench requirements call out,
//! using the raw asupersync primitives the integration tests exercise
//! through their mock fixtures. Benches run under the asupersync-runtime
//! feature; they compose a `LabRuntime` per iteration, spawn a root
//! task, and drive the primitive under test to completion.
//!
//! - `integration_throughput/lifecycle/*`: full pool lifecycle cycles/sec
//!   (Semaphore acquire + release + task finish) — mirrors integration
//!   scenario 1 at bench scale.
//! - `integration_throughput/concurrent_pool/*`: aggregate ops/sec with
//!   N=5,10,20 concurrent tasks contending on a bounded semaphore —
//!   mirrors integration scenario 2.
//! - `integration_throughput/mpsc_roundtrip/*`: mpsc reserve/commit +
//!   recv round-trip, parameterised over message count.
//! - `integration_throughput/event_stream/*`: sustained events/sec for a
//!   single-producer/single-consumer mpsc with capacity bounds — mirrors
//!   integration scenario 4.
//!
//! Criterion records per-iteration timings from which p50/p95/p99
//! percentiles fall out of its statistical model; no bespoke percentile
//! machinery is needed here.

#![cfg(feature = "asupersync-runtime")]

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use asupersync::sync::{OwnedSemaphorePermit, Semaphore};
use asupersync::{Budget, Cx, LabConfig, LabRuntime};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "integration_throughput/lifecycle/single",
        budget: "Full acquire/release cycle on a max_size=4 semaphore, \
                 one task per iteration — integration scenario 1 at bench scale",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/concurrent_pool/5",
        budget: "5 concurrent tasks contending on a max_size=5 semaphore",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/concurrent_pool/10",
        budget: "10 concurrent tasks contending on a max_size=5 semaphore",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/concurrent_pool/20",
        budget: "20 concurrent tasks contending on a max_size=5 semaphore",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/mpsc_roundtrip/1",
        budget: "mpsc reserve/commit + recv round-trip, 1 message",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/mpsc_roundtrip/10",
        budget: "mpsc reserve/commit + recv round-trip, 10 messages",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/mpsc_roundtrip/100",
        budget: "mpsc reserve/commit + recv round-trip, 100 messages",
    },
    bench_common::BenchBudget {
        name: "integration_throughput/event_stream/256",
        budget: "Single producer / single consumer mpsc, 256 events",
    },
];

/// Build and run a LabRuntime task with an Infinite budget. Assumes the
/// task body completes in finite steps; panics if the runtime bails.
fn run_lab_bench<F>(seed: u64, body: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut runtime = LabRuntime::new(
        LabConfig::new(seed)
            .with_auto_advance()
            .worker_count(2)
            .max_steps(1_000_000),
    );
    let region = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, body)
        .expect("spawn bench task");
    runtime.scheduler.lock().schedule(task_id, 0);
    let report = runtime.run_with_auto_advance();
    assert!(
        !matches!(
            report.termination,
            asupersync::lab::AutoAdvanceTermination::StuckBailout
        ),
        "bench LabRuntime got stuck; termination: {:?}",
        report.termination,
    );
}

// ---------------------------------------------------------------------------
// lifecycle — acquire/release cycle on a semaphore
// ---------------------------------------------------------------------------

fn bench_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("integration_throughput/lifecycle");
    group.throughput(Throughput::Elements(1));

    group.bench_function("single", |b| {
        b.iter(|| {
            run_lab_bench(black_box(100), async move {
                let cx = asupersync::Cx::current().expect("lab Cx");
                let sem = Arc::new(Semaphore::new(4));
                let permit = OwnedSemaphorePermit::acquire(Arc::clone(&sem), &cx, 1)
                    .await
                    .expect("acquire");
                drop(permit);
                black_box(sem.available_permits());
            });
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// concurrent_pool — N tasks contending on a max_size=5 semaphore
// ---------------------------------------------------------------------------

async fn contend(sem: Arc<Semaphore>, cx: &Cx, counter: Arc<AtomicU64>) {
    let permit = OwnedSemaphorePermit::acquire(sem, cx, 1)
        .await
        .expect("acquire");
    asupersync::runtime::yield_now().await;
    counter.fetch_add(1, Ordering::Relaxed);
    drop(permit);
}

fn bench_concurrent_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("integration_throughput/concurrent_pool");

    for &n in &[5usize, 10, 20] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut runtime = LabRuntime::new(
                    LabConfig::new(black_box(200))
                        .with_auto_advance()
                        .worker_count(2)
                        .max_steps(2_000_000),
                );
                let region = runtime.state.create_root_region(Budget::INFINITE);
                let sem = Arc::new(Semaphore::new(5));
                let counter = Arc::new(AtomicU64::new(0));

                for _ in 0..n {
                    let sem_c = Arc::clone(&sem);
                    let counter_c = Arc::clone(&counter);
                    let (task_id, _h) = runtime
                        .state
                        .create_task(region, Budget::INFINITE, async move {
                            let cx = asupersync::Cx::current().expect("lab Cx");
                            contend(sem_c, &cx, counter_c).await;
                        })
                        .expect("spawn contender");
                    runtime.scheduler.lock().schedule(task_id, 0);
                }

                let report = runtime.run_with_auto_advance();
                assert!(!matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ));
                assert_eq!(counter.load(Ordering::Relaxed), n as u64);
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// mpsc_roundtrip — send+recv latency for N messages
// ---------------------------------------------------------------------------

fn bench_mpsc_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("integration_throughput/mpsc_roundtrip");

    for &n in &[1usize, 10, 100] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                run_lab_bench(black_box(300), async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    let (tx, mut rx) = asupersync::channel::mpsc::channel::<u64>(16);
                    for i in 0..n as u64 {
                        tx.reserve(&cx).await.expect("reserve").send(i);
                        black_box(rx.recv(&cx).await.expect("recv"));
                    }
                });
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// event_stream — single producer, single consumer
// ---------------------------------------------------------------------------

fn bench_event_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("integration_throughput/event_stream");
    const N: u64 = 256;
    group.throughput(Throughput::Elements(N));

    group.bench_function("256", |b| {
        b.iter(|| {
            let mut runtime = LabRuntime::new(
                LabConfig::new(black_box(400))
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(2_000_000),
            );
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (tx, rx) = asupersync::channel::mpsc::channel::<u64>(64);
            let received = Arc::new(AtomicU64::new(0));

            let (prod_id, _h) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    for i in 0..N {
                        tx.reserve(&cx).await.expect("reserve").send(i);
                    }
                    drop(tx);
                })
                .expect("spawn producer");
            runtime.scheduler.lock().schedule(prod_id, 0);

            let received_c = Arc::clone(&received);
            let (cons_id, _h) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    let cx = asupersync::Cx::current().expect("lab Cx");
                    let mut rx = rx;
                    while rx.recv(&cx).await.is_ok() {
                        received_c.fetch_add(1, Ordering::Relaxed);
                    }
                })
                .expect("spawn consumer");
            runtime.scheduler.lock().schedule(cons_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(!matches!(
                report.termination,
                asupersync::lab::AutoAdvanceTermination::StuckBailout
            ));
            assert_eq!(received.load(Ordering::Relaxed), N);
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_lifecycle(c);
    bench_concurrent_pool(c);
    bench_mpsc_roundtrip(c);
    bench_event_stream(c);
    bench_common::emit_bench_artifacts("integration_throughput", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
