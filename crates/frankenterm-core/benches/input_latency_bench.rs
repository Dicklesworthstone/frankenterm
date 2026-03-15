//! Benchmarks for input-to-display latency measurement framework (ft-1memj.25).
//!
//! Performance budgets:
//! - Measurement recording: **< 100ns** per stage timestamp
//! - Percentile computation (1000 samples): **< 500µs**
//! - Budget evaluation (1000 samples): **< 1ms**
//! - Report generation (1000 samples): **< 2ms**
//!
//! These benchmarks validate the measurement infrastructure itself is fast enough
//! to not introduce observable latency when instrumenting the input pipeline.

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::input_latency::*;
use std::hint::black_box;

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "input_latency/record_stage",
        budget: "< 100ns per stage timestamp",
    },
    bench_common::BenchBudget {
        name: "input_latency/total_latency",
        budget: "< 50ns per total_latency_us() call",
    },
    bench_common::BenchBudget {
        name: "input_latency/percentile_1000",
        budget: "< 500us for p50/p95/p99 over 1000 measurements",
    },
    bench_common::BenchBudget {
        name: "input_latency/budget_eval_1000",
        budget: "< 1ms for budget evaluation over 1000 measurements",
    },
    bench_common::BenchBudget {
        name: "input_latency/report_1000",
        budget: "< 2ms for full report generation over 1000 measurements",
    },
];

fn make_full_measurement(id: u64, base: u64) -> InputLatencyMeasurement {
    let mut m = InputLatencyMeasurement::new(id);
    for (i, &stage) in InputLatencyStage::ALL.iter().enumerate() {
        m.record_stage(stage, base + (i as u64) * 400);
    }
    m
}

fn make_populated_collector(n: usize) -> InputLatencyCollector {
    let mut collector = InputLatencyCollector::new(n + 100);
    for i in 0..n {
        collector.record(make_full_measurement(i as u64, 1000 + i as u64 * 10));
    }
    collector
}

fn bench_record_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("input_latency");

    group.bench_function("record_stage", |b| {
        let mut m = InputLatencyMeasurement::new(0);
        let mut ts = 1000u64;
        b.iter(|| {
            ts += 1;
            m.record_stage(InputLatencyStage::KeyEvent, black_box(ts));
        });
    });

    group.bench_function("total_latency", |b| {
        let m = make_full_measurement(0, 1000);
        b.iter(|| {
            black_box(m.total_latency_us());
        });
    });

    group.bench_function("stage_latency", |b| {
        let m = make_full_measurement(0, 1000);
        b.iter(|| {
            black_box(
                m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::GpuPresent),
            );
        });
    });

    group.finish();
}

fn bench_percentile_computation(c: &mut Criterion) {
    let mut group = c.benchmark_group("input_latency");

    for &size in &[100, 1000, 10000] {
        let collector = make_populated_collector(size);
        group.bench_function(format!("percentile_p50_{size}"), |b| {
            b.iter(|| {
                black_box(collector.total_latency_percentile(Percentile::P50));
            });
        });
        group.bench_function(format!("percentile_p99_{size}"), |b| {
            b.iter(|| {
                black_box(collector.total_latency_percentile(Percentile::P99));
            });
        });
        group.bench_function(format!("summary_{size}"), |b| {
            b.iter(|| {
                black_box(collector.total_latency_summary());
            });
        });
    }

    group.finish();
}

fn bench_budget_evaluation(c: &mut Criterion) {
    let mut group = c.benchmark_group("input_latency");

    for &size in &[100, 1000] {
        let collector = make_populated_collector(size);
        let budget = InputLatencyBudget::default();

        group.bench_function(format!("budget_eval_{size}"), |b| {
            b.iter(|| {
                black_box(evaluate_budget(&collector, &budget));
            });
        });
    }

    group.finish();
}

fn bench_report_generation(c: &mut Criterion) {
    let mut group = c.benchmark_group("input_latency");

    for &size in &[100, 1000] {
        let collector = make_populated_collector(size);
        let budget = InputLatencyBudget::default();

        group.bench_function(format!("report_{size}"), |b| {
            b.iter(|| {
                black_box(generate_report(&collector, Some(&budget)));
            });
        });

        group.bench_function(format!("report_no_budget_{size}"), |b| {
            b.iter(|| {
                black_box(generate_report(&collector, None));
            });
        });
    }

    group.finish();
}

fn bench_collector_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("input_latency");

    group.bench_function("collector_record", |b| {
        let mut collector = InputLatencyCollector::new(10000);
        let mut id = 0u64;
        b.iter(|| {
            id += 1;
            let m = make_full_measurement(id, 1000);
            collector.record(black_box(m));
        });
    });

    group.bench_function("collector_record_eviction", |b| {
        let mut collector = InputLatencyCollector::new(100);
        // Pre-fill to capacity
        for i in 0..100 {
            collector.record(make_full_measurement(i, 1000));
        }
        let mut id = 100u64;
        b.iter(|| {
            id += 1;
            let m = make_full_measurement(id, 1000);
            collector.record(black_box(m));
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_record_stage,
    bench_percentile_computation,
    bench_budget_evaluation,
    bench_report_generation,
    bench_collector_recording,
);

criterion_main!(benches);
