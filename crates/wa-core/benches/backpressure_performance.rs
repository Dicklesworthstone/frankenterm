//! Benchmarks for backpressure handling and capture scheduler overhead.
//!
//! Performance budgets:
//! - Tier classification (idle, green): **< 100ns** per call
//! - Tier evaluation with hysteresis: **< 200ns** per call
//! - Scheduler select_panes (10 panes): **< 500ns** per call
//! - Scheduler select_panes (100 panes): **< 5µs** per call
//! - Scheduler snapshot generation: **< 200ns** per call

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use wa_core::backpressure::{BackpressureConfig, BackpressureManager, QueueDepths};
use wa_core::config::CaptureBudgetConfig;
use wa_core::tailer::CaptureScheduler;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "tier_classify_idle",
        budget: "p50 < 100ns (green tier, no pressure)",
    },
    bench_common::BenchBudget {
        name: "tier_evaluate_hysteresis",
        budget: "p50 < 200ns (evaluate with hysteresis check)",
    },
    bench_common::BenchBudget {
        name: "select_panes_10",
        budget: "p50 < 500ns (10 ready panes, budget enforcement)",
    },
    bench_common::BenchBudget {
        name: "select_panes_100",
        budget: "p50 < 5µs (100 ready panes, budget enforcement)",
    },
    bench_common::BenchBudget {
        name: "scheduler_snapshot",
        budget: "p50 < 200ns (generate scheduler snapshot)",
    },
];

// --- Helpers ---

fn idle_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 0,
        capture_capacity: 1024,
        write_depth: 0,
        write_capacity: 10_000,
    }
}

fn yellow_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 600,
        capture_capacity: 1024,
        write_depth: 7000,
        write_capacity: 10_000,
    }
}

fn red_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 800,
        capture_capacity: 1024,
        write_depth: 8500,
        write_capacity: 10_000,
    }
}

fn near_black_depths() -> QueueDepths {
    QueueDepths {
        capture_depth: 1020,
        capture_capacity: 1024,
        write_depth: 9950,
        write_capacity: 10_000,
    }
}

fn make_panes(n: usize) -> Vec<(u64, u32)> {
    (0..n)
        .map(|i| {
            let priority = match i % 3 {
                0 => 10,
                1 => 5,
                _ => 1,
            };
            (i as u64, priority)
        })
        .collect()
}

// --- Backpressure Manager Benchmarks ---

fn bench_tier_classification(c: &mut Criterion) {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let mut group = c.benchmark_group("backpressure_classify");

    // Budget: < 100ns for idle (green) classification
    group.bench_function("idle_green", |b| {
        let depths = idle_depths();
        b.iter(|| manager.classify(&depths));
    });

    group.bench_function("yellow_pressure", |b| {
        let depths = yellow_depths();
        b.iter(|| manager.classify(&depths));
    });

    group.bench_function("red_pressure", |b| {
        let depths = red_depths();
        b.iter(|| manager.classify(&depths));
    });

    group.bench_function("near_black", |b| {
        let depths = near_black_depths();
        b.iter(|| manager.classify(&depths));
    });

    group.finish();
}

fn bench_tier_evaluation(c: &mut Criterion) {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let mut group = c.benchmark_group("backpressure_evaluate");

    // Budget: < 200ns for evaluation with hysteresis
    group.bench_function("idle_no_transition", |b| {
        let depths = idle_depths();
        b.iter(|| manager.evaluate(&depths));
    });

    group.bench_function("escalation", |b| {
        let depths = red_depths();
        b.iter(|| manager.evaluate(&depths));
    });

    group.finish();
}

fn bench_queue_depth_sampling(c: &mut Criterion) {
    let mut group = c.benchmark_group("backpressure_queue_sampling");

    group.bench_function("capture_ratio", |b| {
        let depths = yellow_depths();
        b.iter(|| depths.capture_ratio());
    });

    group.bench_function("write_ratio", |b| {
        let depths = yellow_depths();
        b.iter(|| depths.write_ratio());
    });

    group.finish();
}

fn bench_pane_pause_management(c: &mut Criterion) {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let mut group = c.benchmark_group("backpressure_pane_pause");

    group.bench_function("pause_check_empty", |b| {
        b.iter(|| manager.is_pane_paused(42));
    });

    // Pause some panes then check
    for i in 0..20 {
        manager.pause_pane(i);
    }

    group.bench_function("pause_check_with_20_paused", |b| {
        b.iter(|| manager.is_pane_paused(10));
    });

    group.bench_function("paused_ids_20", |b| {
        b.iter(|| manager.paused_pane_ids());
    });

    group.finish();
}

fn bench_backpressure_snapshot(c: &mut Criterion) {
    let config = BackpressureConfig::default();
    let manager = BackpressureManager::new(config);

    let mut group = c.benchmark_group("backpressure_snapshot");

    group.bench_function("snapshot_idle", |b| {
        let depths = idle_depths();
        b.iter(|| manager.snapshot(&depths));
    });

    // Add paused panes for a more realistic snapshot
    for i in 0..5 {
        manager.pause_pane(i);
    }

    group.bench_function("snapshot_with_paused", |b| {
        let depths = yellow_depths();
        b.iter(|| manager.snapshot(&depths));
    });

    group.finish();
}

// --- Capture Scheduler Benchmarks ---

fn bench_scheduler_select_panes(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_select_panes");

    // Budget: < 500ns for 10 panes
    for n in [5, 10, 50, 100] {
        let panes = make_panes(n);

        // Unlimited budget (common case)
        let mut scheduler = CaptureScheduler::new(CaptureBudgetConfig {
            max_captures_per_sec: 0,
            max_bytes_per_sec: 0,
        });

        group.bench_with_input(BenchmarkId::new("unlimited", n), &panes, |b, panes| {
            b.iter(|| scheduler.select_panes(panes, 8));
        });

        // With rate limit active
        let mut scheduler_limited = CaptureScheduler::new(CaptureBudgetConfig {
            max_captures_per_sec: 50,
            max_bytes_per_sec: 1_000_000,
        });

        group.bench_with_input(BenchmarkId::new("rate_limited", n), &panes, |b, panes| {
            b.iter(|| scheduler_limited.select_panes(panes, 8));
        });
    }

    group.finish();
}

fn bench_scheduler_budget_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_budget_check");

    // Unlimited budget (no-op fast path)
    let mut unlimited = CaptureScheduler::new(CaptureBudgetConfig {
        max_captures_per_sec: 0,
        max_bytes_per_sec: 0,
    });

    group.bench_function("global_check_unlimited", |b| {
        b.iter(|| unlimited.check_global_budget());
    });

    // Active budget
    let mut limited = CaptureScheduler::new(CaptureBudgetConfig {
        max_captures_per_sec: 100,
        max_bytes_per_sec: 10_000_000,
    });

    group.bench_function("global_check_active", |b| {
        b.iter(|| limited.check_global_budget());
    });

    group.bench_function("byte_budget_check_unlimited", |b| {
        b.iter(|| unlimited.is_byte_budget_exhausted());
    });

    group.bench_function("byte_budget_check_active", |b| {
        b.iter(|| limited.is_byte_budget_exhausted());
    });

    group.finish();
}

fn bench_scheduler_record_capture(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_record_capture");

    let mut scheduler = CaptureScheduler::new(CaptureBudgetConfig {
        max_captures_per_sec: 100,
        max_bytes_per_sec: 10_000_000,
    });

    group.bench_function("record_small_capture", |b| {
        b.iter(|| scheduler.record_capture(1, 256));
    });

    group.bench_function("record_large_capture", |b| {
        b.iter(|| scheduler.record_capture(2, 65536));
    });

    group.finish();
}

fn bench_scheduler_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_snapshot");

    // Budget: < 200ns
    let mut scheduler = CaptureScheduler::new(CaptureBudgetConfig {
        max_captures_per_sec: 50,
        max_bytes_per_sec: 1_000_000,
    });

    // Record some activity to make snapshot non-trivial
    for i in 0..10 {
        scheduler.record_capture(i, 1024);
    }

    group.bench_function("snapshot_with_activity", |b| {
        b.iter(|| scheduler.snapshot());
    });

    // Unlimited scheduler (budget_active = false)
    let scheduler_unlimited = CaptureScheduler::new(CaptureBudgetConfig {
        max_captures_per_sec: 0,
        max_bytes_per_sec: 0,
    });

    group.bench_function("snapshot_unlimited", |b| {
        b.iter(|| scheduler_unlimited.snapshot());
    });

    group.finish();
}

fn bench_scheduler_update_budget(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_update_budget");

    let mut scheduler = CaptureScheduler::new(CaptureBudgetConfig {
        max_captures_per_sec: 50,
        max_bytes_per_sec: 1_000_000,
    });

    group.bench_function("hot_reload_budget", |b| {
        b.iter(|| {
            scheduler.update_budget(CaptureBudgetConfig {
                max_captures_per_sec: 100,
                max_bytes_per_sec: 2_000_000,
            });
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("backpressure_performance", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_tier_classification,
        bench_tier_evaluation,
        bench_queue_depth_sampling,
        bench_pane_pause_management,
        bench_backpressure_snapshot,
        bench_scheduler_select_panes,
        bench_scheduler_budget_check,
        bench_scheduler_record_capture,
        bench_scheduler_snapshot,
        bench_scheduler_update_budget
);
criterion_main!(benches);
