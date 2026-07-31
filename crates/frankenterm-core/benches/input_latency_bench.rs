//! Benchmarks for the legacy proxy-only latency framework (ft-1memj.25).
//!
//! Performance budgets:
//! - Measurement recording: **< 100ns** per stage timestamp
//! - Percentile computation (1000 samples): **< 500µs**
//! - Budget evaluation (1000 samples): **< 1ms**
//! - Report generation (1000 samples): **< 2ms**
//!
//! These benchmarks measure synthetic DTO/framework overhead only. They neither
//! exercise production instrumentation nor establish production input-to-present
//! latency or observer effect.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use frankenterm_core::input_latency::{
    InputLatencyBudget, InputLatencyClockDomainId, InputLatencyCollector,
    InputLatencyMeasurement, InputLatencyProducerId, InputLatencyStage, InputLatencyTimestamp,
    Percentile, evaluate_budget, generate_report,
};
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

fn timestamp(timestamp_us: u64) -> InputLatencyTimestamp {
    InputLatencyTimestamp::new(
        timestamp_us,
        InputLatencyProducerId::new(1).expect("benchmark producer ID is non-zero"),
        InputLatencyClockDomainId::new(1).expect("benchmark clock ID is non-zero"),
    )
}

fn make_full_measurement(id: u64, base: u64) -> InputLatencyMeasurement {
    let mut m = InputLatencyMeasurement::new(id);
    for (i, &stage) in InputLatencyStage::ALL.iter().enumerate() {
        m.record_stage(stage, timestamp(base + (i as u64) * 400))
            .expect("benchmark stages are unique");
    }
    m
}

fn make_populated_collector(n: usize) -> InputLatencyCollector {
    let mut collector = InputLatencyCollector::new(n + 100);
    for i in 0..n {
        collector.record(make_full_measurement(
            i as u64 + 1,
            1000 + i as u64 * 10,
        ));
    }
    collector
}

fn bench_record_stage(c: &mut Criterion) {
    let mut group = c.benchmark_group("input_latency");

    group.bench_function("record_stage", |b| {
        let mut ts = 1000u64;
        b.iter_batched(
            || {
                ts = ts.saturating_add(1);
                (InputLatencyMeasurement::new(1), ts)
            },
            |(mut measurement, timestamp_us)| {
                black_box(measurement.record_stage(
                    InputLatencyStage::KeyEvent,
                    timestamp(black_box(timestamp_us)),
                ))
                .expect("fresh benchmark measurement accepts its first stage");
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("total_latency", |b| {
        let m = make_full_measurement(1, 1000);
        b.iter(|| {
            black_box(
                m.total_latency_us()
                    .expect("complete benchmark measurement has a total latency"),
            );
        });
    });

    group.bench_function("stage_latency", |b| {
        let m = make_full_measurement(1, 1000);
        b.iter(|| {
            black_box(
                m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::GpuPresent)
                    .expect("complete benchmark measurement has the requested stages"),
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
                black_box(
                    collector
                        .total_latency_percentile(Percentile::P50)
                        .expect("populated benchmark collector has a p50"),
                );
            });
        });
        group.bench_function(format!("percentile_p99_{size}"), |b| {
            b.iter(|| {
                black_box(
                    collector
                        .total_latency_percentile(Percentile::P99)
                        .expect("populated benchmark collector has a p99"),
                );
            });
        });
        group.bench_function(format!("summary_{size}"), |b| {
            b.iter(|| {
                black_box(
                    collector
                        .total_latency_summary()
                        .expect("populated benchmark collector has a summary"),
                );
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
            id = id.saturating_add(1);
            let m = make_full_measurement(id, 1000);
            collector.record(black_box(m));
        });
    });

    group.bench_function("collector_record_eviction", |b| {
        let mut collector = InputLatencyCollector::new(100);
        // Pre-fill to capacity
        for i in 0..100 {
            collector.record(make_full_measurement(i + 1, 1000));
        }
        let mut id = 100u64;
        b.iter(|| {
            id = id.saturating_add(1);
            let m = make_full_measurement(id, 1000);
            collector.record(black_box(m));
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("input_latency", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_record_stage,
        bench_percentile_computation,
        bench_budget_evaluation,
        bench_report_generation,
        bench_collector_recording
);

criterion_main!(benches);
