//! Benchmarks for auto-tuning configuration parameters.
//!
//! Performance budgets:
//! - Parameter tick latency (5 params, 100-entry history): **< 100us**
//! - Hysteresis evaluation per parameter: **< 10us**
//! - clamp_to_ranges (all params): **< 1us**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::auto_tune::{
    AutoTuneConfig, AutoTuner, PinnedParams, TunableParams, TunerMetrics,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "parameter_tick_latency",
        budget: "p50 < 100us (5 params, 100-entry history)",
    },
    bench_common::BenchBudget {
        name: "hysteresis_evaluation",
        budget: "p50 < 10us per parameter",
    },
    bench_common::BenchBudget {
        name: "clamp_to_ranges",
        budget: "p50 < 1us (pure arithmetic)",
    },
];

fn default_config() -> AutoTuneConfig {
    AutoTuneConfig::default()
}

fn calm_metrics() -> TunerMetrics {
    TunerMetrics {
        rss_fraction: 0.5,
        mux_latency_ms: 10.0,
        cpu_fraction: 0.3,
    }
}

fn high_memory_metrics() -> TunerMetrics {
    TunerMetrics {
        rss_fraction: 0.8,
        mux_latency_ms: 5.0,
        cpu_fraction: 0.15,
    }
}

fn mixed_pressure_metrics() -> TunerMetrics {
    TunerMetrics {
        rss_fraction: 0.7,
        mux_latency_ms: 20.0,
        cpu_fraction: 0.5,
    }
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Benchmark a full tick() cycle with varying history depths.
fn bench_parameter_tick(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_tune/parameter_tick");

    for &history_depth in &[0, 10, 50, 100] {
        let label = format!("history_{history_depth}");

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("tick", &label),
            &history_depth,
            |b, &depth| {
                b.iter(|| {
                    let config = AutoTuneConfig {
                        hysteresis_ticks: 1,
                        history_limit: 100,
                        ..default_config()
                    };
                    let mut tuner = AutoTuner::new(config);

                    // Pre-fill history to the desired depth
                    for _ in 0..depth {
                        tuner.tick(&calm_metrics());
                    }

                    // Measure a single tick with pressure
                    tuner.tick(&high_memory_metrics())
                });
            },
        );
    }

    group.finish();
}

/// Benchmark hysteresis evaluation: how long it takes for the hysteresis
/// state machine to evaluate whether the sustained signal threshold is met.
fn bench_hysteresis_evaluation(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_tune/hysteresis");

    for &hysteresis_ticks in &[1, 3, 5, 10] {
        let label = format!("threshold_{hysteresis_ticks}");

        group.throughput(Throughput::Elements(hysteresis_ticks as u64));
        group.bench_with_input(
            BenchmarkId::new("evaluate", &label),
            &hysteresis_ticks,
            |b, &threshold| {
                b.iter(|| {
                    let config = AutoTuneConfig {
                        hysteresis_ticks: threshold,
                        history_limit: 10,
                        ..default_config()
                    };
                    let mut tuner = AutoTuner::new(config);

                    // Run exactly threshold ticks to trigger sustained signal
                    for _ in 0..threshold {
                        tuner.tick(&high_memory_metrics());
                    }

                    tuner.params().clone()
                });
            },
        );
    }

    group.finish();
}

/// Benchmark clamp_to_ranges: pure arithmetic bounding of all parameters.
fn bench_clamp_to_ranges(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_tune/clamp_to_ranges");

    // In-range values (no clamping needed)
    group.bench_function("in_range", |b| {
        b.iter(|| {
            let mut params = TunableParams::default();
            params.clamp_to_ranges();
            params
        });
    });

    // Out-of-range values (all need clamping)
    group.bench_function("out_of_range", |b| {
        b.iter(|| {
            let mut params = TunableParams {
                poll_interval_ms: 5.0,
                scrollback_lines: 50_000.0,
                snapshot_interval_secs: 0.0,
                pool_size: 100.0,
                backpressure_threshold: -1.0,
            };
            params.clamp_to_ranges();
            params
        });
    });

    group.finish();
}

/// Benchmark a sustained pressure scenario: multiple ticks with varying
/// conditions to simulate real-world adaptive tuning.
fn bench_sustained_tuning(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_tune/sustained_tuning");

    for &ticks in &[10, 50, 100] {
        group.throughput(Throughput::Elements(ticks as u64));
        group.bench_with_input(
            BenchmarkId::new("mixed_pressure", ticks),
            &ticks,
            |b, &tick_count| {
                b.iter(|| {
                    let config = AutoTuneConfig {
                        hysteresis_ticks: 3,
                        ..default_config()
                    };
                    let mut tuner = AutoTuner::new(config);

                    for _ in 0..tick_count {
                        tuner.tick(&mixed_pressure_metrics());
                    }

                    tuner.params().clone()
                });
            },
        );
    }

    group.finish();
}

/// Benchmark with pinned parameters (manual overrides).
fn bench_pinned_params(c: &mut Criterion) {
    let mut group = c.benchmark_group("auto_tune/pinned_params");

    // All pinned (should be fastest — nothing to compute)
    group.bench_function("all_pinned", |b| {
        b.iter(|| {
            let config = AutoTuneConfig {
                hysteresis_ticks: 1,
                ..default_config()
            };
            let mut tuner = AutoTuner::new(config);
            tuner.set_pinned(PinnedParams {
                poll_interval_ms: true,
                scrollback_lines: true,
                snapshot_interval_secs: true,
                pool_size: true,
                backpressure_threshold: true,
            });

            for _ in 0..10 {
                tuner.tick(&high_memory_metrics());
            }

            tuner.params().clone()
        });
    });

    // None pinned (maximum computation)
    group.bench_function("none_pinned", |b| {
        b.iter(|| {
            let config = AutoTuneConfig {
                hysteresis_ticks: 1,
                ..default_config()
            };
            let mut tuner = AutoTuner::new(config);

            for _ in 0..10 {
                tuner.tick(&high_memory_metrics());
            }

            tuner.params().clone()
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("auto_tuning", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_parameter_tick,
        bench_hysteresis_evaluation,
        bench_clamp_to_ranges,
        bench_sustained_tuning,
        bench_pinned_params
);
criterion_main!(benches);
