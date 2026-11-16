//! Criterion bench for LabRuntime test-execution time on time-dependent logic.
//!
//! Bead: wa-22x4r (Port existing async tests to LabRuntime).
//!
//! `labruntime_overhead.rs` already covers generic setup/teardown, oracle
//! cost, spawn cost, and DPOR exploration cost. This bench stays focused
//! on the virtual-time task shapes that motivated the migration.

#![cfg(feature = "asupersync-runtime")]

use std::hint::black_box;
use std::time::Duration;

use asupersync::{Budget, LabConfig, LabRuntime};
use criterion::{Criterion, criterion_group, criterion_main};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "test_execution/lab_sleep_1ms",
        budget: "LabRuntime run_until_quiescent with an async task that \
                 awaits 1ms of virtual time — should complete essentially \
                 instantaneously",
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
    bench_lab_sleep(c);
    bench_lab_chain(c);
    bench_common::emit_bench_artifacts("test_execution", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
