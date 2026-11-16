//! Criterion benchmarks for EWMA (Exponentially Weighted Moving Average).
//!
//! Bead: (proactive — covers ewma.rs module)
//!
//! Performance budgets:
//! - Single observe:   **< 20ns**
//! - Z-score query:    **< 10ns**
//! - Rate estimation:  **< 50ns** per tick
//! - 10K observations: **< 200µs** total

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::ewma::{Ewma, EwmaWithVariance, RateEstimator};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "ewma_single_observe",
        budget: "p50 < 20ns (single observation with time-based decay)",
    },
    bench_common::BenchBudget {
        name: "ewma_z_score",
        budget: "p50 < 10ns (z-score query)",
    },
    bench_common::BenchBudget {
        name: "ewma_rate_tick",
        budget: "p50 < 50ns per tick (rate estimator)",
    },
    bench_common::BenchBudget {
        name: "ewma_throughput",
        budget: "10K observations < 200us",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Generate a deterministic sequence of (value, timestamp_ms) pairs.
/// Simulates a noisy metric with slight upward trend.
fn noisy_metric(n: usize) -> Vec<(f64, u64)> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xCAFE_BABE_DEAD_BEEF;
    for i in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let noise = ((state >> 33) as f64 / u32::MAX as f64).mul_add(20.0, -10.0);
        let value = (i as f64).mul_add(0.01, 100.0) + noise;
        let time_ms = (i as u64) * 100; // 100ms intervals
        out.push((value, time_ms));
    }
    out
}

/// Generate irregular timestamps (variable intervals).
fn irregular_timestamps(n: usize) -> Vec<u64> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xBEEF_CAFE;
    let mut t = 0u64;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let interval = 10 + (state >> 58) * 10; // 10-650ms jitter
        t += interval;
        out.push(t);
    }
    out
}

/// Pre-build a warmed-up EWMA with n observations.
fn build_ewma(n: usize) -> Ewma {
    let mut ewma = Ewma::with_half_life_ms(1000.0);
    for (val, ts) in noisy_metric(n) {
        ewma.observe(val, ts);
    }
    ewma
}

/// Pre-build a warmed-up EwmaWithVariance.
fn build_ewma_var(n: usize) -> EwmaWithVariance {
    let mut ewma = EwmaWithVariance::with_half_life_ms(1000.0);
    for (val, ts) in noisy_metric(n) {
        ewma.observe(val, ts);
    }
    ewma
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: single observation latency for Ewma.
fn bench_ewma_observe(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/observe");

    // Cold observe (first observation, no decay computation).
    group.bench_function("cold", |b| {
        b.iter_batched(
            || Ewma::with_half_life_ms(1000.0),
            |mut ewma| {
                ewma.observe(black_box(42.0), black_box(0));
                ewma
            },
            BatchSize::SmallInput,
        );
    });

    // Warm observe (after 1K observations).
    group.bench_function("after_1K", |b| {
        b.iter_batched(
            || build_ewma(1000),
            |mut ewma| {
                ewma.observe(black_box(99.0), black_box(100_100));
                ewma
            },
            BatchSize::SmallInput,
        );
    });

    // Simultaneous timestamps (dt=0 path).
    group.bench_function("dt_zero", |b| {
        b.iter_batched(
            || {
                let mut e = Ewma::with_half_life_ms(1000.0);
                e.observe(50.0, 1000);
                e
            },
            |mut ewma| {
                ewma.observe(black_box(60.0), black_box(1000));
                ewma
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: observation throughput (many observations).
fn bench_ewma_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/throughput");

    for count in [100, 1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(count));
        let data = noisy_metric(count as usize);

        group.bench_with_input(BenchmarkId::from_parameter(count), &data, |b, data| {
            b.iter(|| {
                let mut ewma = Ewma::with_half_life_ms(1000.0);
                for &(val, ts) in data {
                    ewma.observe(black_box(val), black_box(ts));
                }
                black_box(ewma.value())
            });
        });
    }

    group.finish();
}

/// Benchmark: EwmaWithVariance (observe + z-score).
fn bench_ewma_variance(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/variance");

    // Single observe on warmed EwmaWithVariance.
    group.bench_function("observe_after_1K", |b| {
        b.iter_batched(
            || build_ewma_var(1000),
            |mut ewma| {
                ewma.observe(black_box(110.0), black_box(100_100));
                ewma
            },
            BatchSize::SmallInput,
        );
    });

    // Z-score query (no mutation, pure computation).
    group.bench_function("z_score", |b| {
        let ewma = build_ewma_var(1000);
        b.iter(|| black_box(ewma.z_score(black_box(200.0))));
    });

    // is_anomaly query.
    group.bench_function("is_anomaly", |b| {
        let ewma = build_ewma_var(1000);
        b.iter(|| black_box(ewma.is_anomaly(black_box(200.0), black_box(3.0))));
    });

    // stddev query.
    group.bench_function("stddev", |b| {
        let ewma = build_ewma_var(1000);
        b.iter(|| black_box(ewma.stddev()));
    });

    // Throughput with variance tracking.
    let data = noisy_metric(10_000);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("throughput_10K", |b| {
        b.iter(|| {
            let mut ewma = EwmaWithVariance::with_half_life_ms(1000.0);
            for &(val, ts) in &data {
                ewma.observe(black_box(val), black_box(ts));
            }
            black_box((ewma.mean(), ewma.stddev()))
        });
    });

    group.finish();
}

/// Benchmark: RateEstimator.
fn bench_rate_estimator(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/rate_estimator");

    // Single tick on warmed estimator.
    group.bench_function("tick_after_1K", |b| {
        b.iter_batched(
            || {
                let mut rate = RateEstimator::with_half_life_ms(5000.0);
                for ts in irregular_timestamps(1000) {
                    rate.tick(ts);
                }
                rate
            },
            |mut rate| {
                rate.tick(black_box(999_999));
                rate
            },
            BatchSize::SmallInput,
        );
    });

    // Rate query (no mutation).
    group.bench_function("rate_query", |b| {
        let mut rate = RateEstimator::with_half_life_ms(5000.0);
        for ts in irregular_timestamps(1000) {
            rate.tick(ts);
        }
        b.iter(|| black_box(rate.rate_per_sec()));
    });

    // Throughput: 10K ticks.
    let timestamps = irregular_timestamps(10_000);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("throughput_10K", |b| {
        b.iter(|| {
            let mut rate = RateEstimator::with_half_life_ms(5000.0);
            for &ts in &timestamps {
                rate.tick(black_box(ts));
            }
            black_box(rate.rate_per_sec())
        });
    });

    group.finish();
}

/// Benchmark: half-life sensitivity (different half-lives, same data).
fn bench_half_life_sensitivity(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/half_life_sensitivity");
    let data = noisy_metric(10_000);
    group.throughput(Throughput::Elements(10_000));

    for hl_ms in [100.0, 1000.0, 10_000.0, 100_000.0] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{hl_ms}ms")),
            &hl_ms,
            |b, &hl_ms| {
                b.iter(|| {
                    let mut ewma = Ewma::with_half_life_ms(hl_ms);
                    for &(val, ts) in &data {
                        ewma.observe(black_box(val), black_box(ts));
                    }
                    black_box(ewma.value())
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: reset and reuse pattern.
fn bench_reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/reset");

    group.bench_function("ewma_reset", |b| {
        b.iter_batched(
            || build_ewma(1000),
            |mut ewma| {
                ewma.reset();
                black_box(ewma)
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("variance_reset", |b| {
        b.iter_batched(
            || build_ewma_var(1000),
            |mut ewma| {
                ewma.reset();
                black_box(ewma)
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("rate_reset", |b| {
        b.iter_batched(
            || {
                let mut rate = RateEstimator::with_half_life_ms(5000.0);
                for ts in irregular_timestamps(1000) {
                    rate.tick(ts);
                }
                rate
            },
            |mut rate| {
                rate.reset();
                black_box(rate)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: stats serialization.
fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("ewma/stats");

    let ewma = build_ewma(10_000);

    group.bench_function("stats_query", |b| {
        b.iter(|| black_box(ewma.stats()));
    });

    group.bench_function("stats_serde_roundtrip", |b| {
        let stats = ewma.stats();
        b.iter(|| {
            let json = serde_json::to_string(black_box(&stats)).unwrap();
            black_box(serde_json::from_str::<frankenterm_core::ewma::EwmaStats>(&json).unwrap())
        });
    });

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_ewma_observe,
    bench_ewma_throughput,
    bench_ewma_variance,
    bench_rate_estimator,
    bench_half_life_sensitivity,
    bench_reset,
    bench_stats,
);

criterion_main!(benches);
