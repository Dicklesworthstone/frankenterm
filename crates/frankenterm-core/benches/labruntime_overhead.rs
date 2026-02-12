//! Criterion benchmarks for LabRuntime overhead vs real async.
//!
//! Measures the cost of deterministic testing infrastructure so users
//! can make informed decisions about test granularity.
//!
//! Benchmark groups:
//! - `lab_overhead/setup_teardown`: LabRuntime::new() + run_until_quiescent cost
//! - `lab_overhead/oracle_check`: Oracle report generation overhead
//! - `lab_overhead/virtual_time`: Virtual time advance cost
//! - `lab_overhead/task_spawn`: Task creation + scheduling overhead
//! - `lab_overhead/exploration`: DPOR exploration cost per seed

use std::hint::black_box;

use asupersync::lab::explorer::{ExplorerConfig, ScheduleExplorer};
use asupersync::{Budget, LabConfig, LabRuntime};
use criterion::{Criterion, criterion_group, criterion_main};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "lab_overhead/setup_teardown/empty",
        budget: "LabRuntime new + quiesce with no tasks",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/setup_teardown/with_report",
        budget: "LabRuntime new + quiesce + oracle report",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/oracle_check/no_tasks",
        budget: "Oracle report on empty runtime",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/oracle_check/after_tasks",
        budget: "Oracle report after running tasks",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/virtual_time/advance_1ms",
        budget: "Advance virtual time by 1ms",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/virtual_time/advance_100ms",
        budget: "Advance virtual time by 100ms",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/task_spawn/single",
        budget: "Create + schedule + run 1 task",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/task_spawn/ten",
        budget: "Create + schedule + run 10 tasks",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/exploration/3_seeds",
        budget: "DPOR exploration over 3 seeds (empty body)",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/exploration/10_seeds",
        budget: "DPOR exploration over 10 seeds (empty body)",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/comparison/tokio_block_on",
        budget: "tokio block_on with trivial future",
    },
    bench_common::BenchBudget {
        name: "lab_overhead/comparison/lab_block_on",
        budget: "LabRuntime run single task to completion",
    },
];

// ---------------------------------------------------------------------------
// Setup + teardown benchmarks
// ---------------------------------------------------------------------------

fn bench_setup_teardown(c: &mut Criterion) {
    let mut group = c.benchmark_group("lab_overhead/setup_teardown");

    group.bench_function("empty", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(1).max_steps(1_000));
            black_box(runtime.run_until_quiescent());
        });
    });

    group.bench_function("with_report", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(1).max_steps(1_000));
            let report = runtime.run_until_quiescent_with_report();
            black_box(report.oracle_report.all_passed());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Oracle check benchmarks
// ---------------------------------------------------------------------------

fn bench_oracle_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("lab_overhead/oracle_check");

    group.bench_function("no_tasks", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(7)).worker_count(1).max_steps(1_000));
            runtime.run_until_quiescent();
            let report = runtime.report();
            black_box(report.oracle_report.all_passed());
        });
    });

    group.bench_function("after_tasks", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(7)).worker_count(2).max_steps(10_000));
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..5_u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move {
                        black_box(i * 2);
                    })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            runtime.run_until_quiescent();
            let report = runtime.report();
            black_box(report.oracle_report.all_passed());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Virtual time benchmarks
// ---------------------------------------------------------------------------

fn bench_virtual_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("lab_overhead/virtual_time");

    group.bench_function("advance_1ms", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(1).max_steps(10_000));
            runtime.advance_time(black_box(1_000_000)); // 1ms in nanos
            black_box(runtime.now());
        });
    });

    group.bench_function("advance_100ms", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(1).max_steps(10_000));
            runtime.advance_time(black_box(100_000_000)); // 100ms in nanos
            black_box(runtime.now());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Task spawn benchmarks
// ---------------------------------------------------------------------------

fn bench_task_spawn(c: &mut Criterion) {
    let mut group = c.benchmark_group("lab_overhead/task_spawn");

    group.bench_function("single", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(1).max_steps(10_000));
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async { black_box(42_u32) })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .expect("lock scheduler")
                .schedule(task_id, 0);
            black_box(runtime.run_until_quiescent());
        });
    });

    group.bench_function("ten", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(2).max_steps(10_000));
            let region = runtime.state.create_root_region(Budget::INFINITE);
            for i in 0..10_u32 {
                let (task_id, _handle) = runtime
                    .state
                    .create_task(region, Budget::INFINITE, async move { black_box(i) })
                    .expect("create task");
                runtime
                    .scheduler
                    .lock()
                    .expect("lock scheduler")
                    .schedule(task_id, 0);
            }
            black_box(runtime.run_until_quiescent());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Exploration (DPOR) benchmarks
// ---------------------------------------------------------------------------

fn bench_exploration(c: &mut Criterion) {
    let mut group = c.benchmark_group("lab_overhead/exploration");

    group.bench_function("3_seeds", |b| {
        b.iter(|| {
            let config = ExplorerConfig {
                base_seed: black_box(0),
                max_runs: 3,
                max_steps_per_run: 1_000,
                worker_count: 1,
                record_traces: true,
            };
            let mut explorer = ScheduleExplorer::new(config);
            let report = explorer.explore(|runtime| {
                runtime.run_until_quiescent();
            });
            black_box(report.has_violations());
        });
    });

    group.bench_function("10_seeds", |b| {
        b.iter(|| {
            let config = ExplorerConfig {
                base_seed: black_box(0),
                max_runs: 10,
                max_steps_per_run: 1_000,
                worker_count: 1,
                record_traces: true,
            };
            let mut explorer = ScheduleExplorer::new(config);
            let report = explorer.explore(|runtime| {
                runtime.run_until_quiescent();
            });
            black_box(report.has_violations());
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Comparison: tokio vs LabRuntime
// ---------------------------------------------------------------------------

fn bench_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("lab_overhead/comparison");

    let tokio_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    group.bench_function("tokio_block_on", |b| {
        b.iter(|| {
            tokio_rt.block_on(async {
                black_box(42_u32);
            });
        });
    });

    group.bench_function("lab_block_on", |b| {
        b.iter(|| {
            let mut runtime =
                LabRuntime::new(LabConfig::new(black_box(42)).worker_count(1).max_steps(1_000));
            let region = runtime.state.create_root_region(Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, Budget::INFINITE, async { black_box(42_u32) })
                .expect("create task");
            runtime
                .scheduler
                .lock()
                .expect("lock scheduler")
                .schedule(task_id, 0);
            runtime.run_until_quiescent();
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("labruntime_overhead", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_setup_teardown,
        bench_oracle_check,
        bench_virtual_time,
        bench_task_spawn,
        bench_exploration,
        bench_comparison
);
criterion_main!(benches);
