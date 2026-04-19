#![cfg(feature = "asupersync-runtime")]

//! Benchmarks for wa-1yz79 timer precision and timeout semantics.
//!
//! Uses virtual-time runtimes where possible so long deadlines remain cheap to
//! measure and deterministic in CI/local runs.

use std::hint::black_box;
use std::time::Duration;

use asupersync::{Budget, LabConfig, LabRuntime, Time};
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::runtime_compat;

mod bench_common;

const DEADLINES_MS: &[(u64, &str)] = &[(1, "1ms"), (10, "10ms"), (100, "100ms"), (1_000, "1s")];
const LOAD_TASKS: usize = 32;
const TIMER_BATCH: usize = 1_024;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "timer_precision/sleep_latency/asupersync",
        budget: "virtual-time sleep deadline delta for runtime_compat::sleep under LabRuntime",
    },
    bench_common::BenchBudget {
        name: "timer_precision/deadline_accuracy/asupersync",
        budget: "budget deadline accuracy under synthetic timer load in LabRuntime",
    },
    bench_common::BenchBudget {
        name: "timer_precision/create_cancel/asupersync",
        budget: "timer future creation and cancellation throughput for asupersync",
    },
];

fn duration_from_ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

fn run_asupersync_sleep_latency(ms: u64) -> u64 {
    let duration = duration_from_ms(ms);
    let expected_nanos = u64::try_from(duration.as_nanos()).expect("duration should fit in u64");
    let mut runtime = LabRuntime::new(
        LabConfig::new(41)
            .with_auto_advance()
            .worker_count(2)
            .max_steps(20_000),
    );
    let region = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, _handle) = runtime
        .state
        .create_task(region, Budget::INFINITE, async move {
            runtime_compat::sleep(duration).await;
        })
        .expect("spawn sleep task");
    runtime.scheduler.lock().schedule(task_id, 0);

    runtime.step_for_test();
    runtime.run_with_auto_advance();
    runtime.run_until_quiescent();

    let actual_nanos = runtime.now().as_nanos();
    actual_nanos.abs_diff(expected_nanos)
}

fn run_asupersync_deadline_accuracy(ms: u64) -> u64 {
    let expected = Time::from_millis(ms);
    let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
    let observed_task = std::sync::Arc::clone(&observed);
    let mut runtime = LabRuntime::new(
        LabConfig::new(43)
            .with_auto_advance()
            .worker_count(2)
            .max_steps(50_000),
    );
    let region = runtime.state.create_root_region(Budget::INFINITE);

    for task_ix in 0..LOAD_TASKS {
        let load_duration = Duration::from_millis((task_ix % 3) as u64);
        let (task_id, _handle) = runtime
            .state
            .create_task(region, Budget::INFINITE, async move {
                runtime_compat::sleep(load_duration).await;
            })
            .expect("spawn load task");
        runtime.scheduler.lock().schedule(task_id, 0);
    }

    let timeout_budget = Budget::new().with_deadline(expected);
    let (task_id, _handle) = runtime
        .state
        .create_task(region, timeout_budget, async move {
            let result =
                runtime_compat::timeout(Duration::from_secs(30), std::future::pending::<()>())
                    .await;
            black_box(result.is_err());
            let current = asupersync::Cx::current().expect("timeout task current cx");
            let now = current
                .timer_driver()
                .map_or_else(asupersync::time::wall_now, |driver| driver.now());
            *observed_task.lock().expect("observed deadline lock") = Some(now);
        })
        .expect("spawn timeout task");
    runtime.scheduler.lock().schedule(task_id, 0);

    runtime.step_for_test();
    runtime.run_with_auto_advance();
    runtime.run_until_quiescent();

    let actual = observed
        .lock()
        .expect("observed deadline lock")
        .expect("deadline observation");
    actual.as_nanos().abs_diff(expected.as_nanos())
}

fn bench_sleep_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_precision/sleep_latency");

    for &(deadline_ms, label) in DEADLINES_MS {
        group.bench_with_input(
            BenchmarkId::new("asupersync", label),
            &deadline_ms,
            |b, &ms| {
                b.iter(|| black_box(run_asupersync_sleep_latency(ms)));
            },
        );
    }

    group.finish();
}

fn bench_deadline_accuracy(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_precision/deadline_accuracy");

    for &(deadline_ms, label) in DEADLINES_MS {
        group.bench_with_input(
            BenchmarkId::new("asupersync", label),
            &deadline_ms,
            |b, &ms| {
                b.iter(|| black_box(run_asupersync_deadline_accuracy(ms)));
            },
        );
    }

    group.finish();
}

fn bench_create_cancel_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer_precision/create_cancel");

    group.bench_function("asupersync", |b| {
        b.iter(|| {
            let mut timers = Vec::with_capacity(TIMER_BATCH);
            for _ in 0..TIMER_BATCH {
                timers.push(asupersync::time::sleep(Time::ZERO, Duration::from_secs(60)));
            }
            black_box(timers.len());
            drop(timers);
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("timer_precision", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_sleep_latency, bench_deadline_accuracy, bench_create_cancel_throughput
);
criterion_main!(benches);
