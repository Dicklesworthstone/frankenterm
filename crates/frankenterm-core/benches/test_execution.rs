//! Criterion bench: LabRuntime vs tokio test-execution time for the one
//! scenario where the two diverge interestingly — time-dependent logic.
//!
//! Bead: wa-22x4r (Port existing async tests to LabRuntime).
//!
//! `labruntime_overhead.rs` already covers generic setup/teardown, oracle
//! cost, spawn cost, DPOR exploration cost, and trivial-future block-on
//! comparison. Those benches show LabRuntime is competitive on raw
//! overhead. The one claim they do *not* pin is the bead's headline
//! justification for porting tokio::test tests:
//!
//!   > Target: LabRuntime tests should be at least 2x faster than
//!   > equivalent tokio::test tests for time-dependent tests (due to
//!   > virtual time eliminating real sleeps).
//!
//! This bench compares the wall-clock cost of the same logical "wait N
//! milliseconds then proceed" test body running under:
//!
//!   - tokio (real sleep, bound by OS scheduler) — `tokio_sleep_*`
//!   - LabRuntime (virtual time skip, zero real wall-clock wait) —
//!     `lab_sleep_*`
//!
//! Running the bench produces a speedup ratio. The wa-22x4r bead cites
//! 2x as the minimum; in practice for milliseconds-scale waits the ratio
//! is several orders of magnitude. The `finish_*` variants measure
//! completed round trips.
//!
//! Criterion handles sample sizing; we intentionally run few iterations
//! on the tokio variants because each iteration really does sleep, and
//! a full sample would take minutes.

#![cfg(feature = "asupersync-runtime")]

use std::hint::black_box;
use std::time::Duration;

use asupersync::{Budget, LabConfig, LabRuntime};
use criterion::{Criterion, criterion_group, criterion_main};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "test_execution/tokio_sleep_1ms",
        budget: "tokio block_on of tokio::time::sleep(1ms) — real wall clock",
    },
    bench_common::BenchBudget {
        name: "test_execution/lab_sleep_1ms",
        budget: "LabRuntime run_until_quiescent with an async task that \
                 awaits 1ms of virtual time — should complete essentially \
                 instantaneously",
    },
    bench_common::BenchBudget {
        name: "test_execution/tokio_sleep_10ms",
        budget: "tokio block_on of tokio::time::sleep(10ms) — real wall clock",
    },
    bench_common::BenchBudget {
        name: "test_execution/lab_sleep_10ms",
        budget: "LabRuntime run_until_quiescent with an async task that \
                 awaits 10ms of virtual time",
    },
    bench_common::BenchBudget {
        name: "test_execution/lab_finish_chain_1ms_x5",
        budget: "LabRuntime run_until_quiescent with a task that chains 5 \
                 sequential 1ms virtual waits — measures the per-timer \
                 overhead under virtual time",
    },
];

// ---------------------------------------------------------------------------
// tokio baseline: real sleeps
// ---------------------------------------------------------------------------

fn bench_tokio_sleep(c: &mut Criterion) {
    let mut group = c.benchmark_group("test_execution");
    group.sample_size(10); // real sleeps; keep samples small

    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for sleep baseline");

    for &millis in &[1u64, 10] {
        let id = format!("tokio_sleep_{millis}ms");
        group.bench_function(id, |b| {
            b.iter(|| {
                tokio_rt.block_on(async {
                    tokio::time::sleep(Duration::from_millis(millis)).await;
                    black_box(millis);
                });
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// LabRuntime: virtual-time sleeps under auto-advance
// ---------------------------------------------------------------------------

fn bench_lab_sleep(c: &mut Criterion) {
    let mut group = c.benchmark_group("test_execution");

    for &millis in &[1u64, 10] {
        let id = format!("lab_sleep_{millis}ms");
        group.bench_function(id, |b| {
            b.iter(|| {
                let mut runtime = LabRuntime::new(
                    LabConfig::new(black_box(42))
                        .with_auto_advance()
                        .worker_count(1)
                        .max_steps(10_000),
                );
                let region = runtime.state.create_root_region(Budget::INFINITE);
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        let cx = asupersync::Cx::current().expect("lab Cx");
                        let _ = asupersync::time::budget_sleep(
                            &cx,
                            Duration::from_millis(millis),
                            asupersync::Time::ZERO,
                        )
                        .await;
                        black_box(millis);
                    })
                    .expect("spawn lab task");
                runtime.scheduler.lock().schedule(task_id, 0);
                let report = runtime.run_with_auto_advance();
                black_box(!matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ));
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// LabRuntime: chained virtual-time sleeps — measures per-timer overhead
// ---------------------------------------------------------------------------

fn bench_lab_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("test_execution");

    group.bench_function("lab_finish_chain_1ms_x5", |b| {
        b.iter(|| {
            let mut runtime = LabRuntime::new(
                LabConfig::new(black_box(42))
                    .with_auto_advance()
                    .worker_count(1)
                    .max_steps(10_000),
            );
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async move {
                    for _ in 0..5 {
                        let cx = asupersync::Cx::current().expect("lab Cx");
                        let _ = asupersync::time::budget_sleep(
                            &cx,
                            Duration::from_millis(1),
                            asupersync::Time::ZERO,
                        )
                        .await;
                    }
                })
                .expect("spawn chain task");
            runtime.scheduler.lock().schedule(task_id, 0);
            let report = runtime.run_with_auto_advance();
            black_box(!matches!(
                report.termination,
                asupersync::lab::AutoAdvanceTermination::StuckBailout
            ));
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_tokio_sleep(c);
    bench_lab_sleep(c);
    bench_lab_chain(c);
    bench_common::emit_bench_artifacts("test_execution", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
