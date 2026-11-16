//! Criterion benchmarks for token bucket rate limiter.
//!
//! Bead: (proactive — covers token_bucket.rs module)
//!
//! Performance budgets:
//! - Single try_acquire:   **< 15ns** (refill + compare + subtract)
//! - Wait time query:      **< 15ns** (refill + division)
//! - Hierarchical acquire: **< 30ns** (two-bucket atomic check)
//! - 10K acquires:         **< 150us** total throughput
//! - Config build:         **< 20ns**
//! - Stats + serde:        **< 200ns** roundtrip

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::token_bucket::{
    BucketConfig, BucketStats, HierarchicalBucket, TokenBucket, TokenBucketTelemetrySnapshot,
};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "token_bucket_try_acquire",
        budget: "p50 < 15ns (single acquire with refill)",
    },
    bench_common::BenchBudget {
        name: "token_bucket_wait_time",
        budget: "p50 < 15ns (wait time computation)",
    },
    bench_common::BenchBudget {
        name: "token_bucket_hierarchical",
        budget: "p50 < 30ns (two-bucket atomic acquire)",
    },
    bench_common::BenchBudget {
        name: "token_bucket_throughput",
        budget: "10K acquires < 150us",
    },
    bench_common::BenchBudget {
        name: "token_bucket_config_build",
        budget: "p50 < 20ns (config → bucket construction)",
    },
    bench_common::BenchBudget {
        name: "token_bucket_stats_serde",
        budget: "p50 < 200ns (stats JSON roundtrip)",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Generate a deterministic sequence of (cost, timestamp_ms) pairs.
/// Simulates bursty request traffic with variable inter-arrival times.
fn bursty_traffic(n: usize) -> Vec<(u32, u64)> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xCAFE_BABE_DEAD_BEEF;
    let mut t = 0u64;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Cost: 1-5 tokens
        let cost = 1 + ((state >> 60) as u32 % 5);
        // Inter-arrival: 1-200ms (bursty)
        let interval = 1 + (state >> 50) % 200;
        t += interval;
        out.push((cost, t));
    }
    out
}

/// Generate timestamps with fixed interval (uniform traffic).
fn uniform_traffic(n: usize, interval_ms: u64) -> Vec<u64> {
    (0..n as u64).map(|i| i * interval_ms).collect()
}

/// Pre-build a bucket that has been exercised with `n` operations.
fn build_exercised_bucket(n: usize) -> TokenBucket {
    let mut bucket = TokenBucket::with_time(100.0, 50.0, 0);
    for (cost, ts) in bursty_traffic(n) {
        bucket.try_acquire(cost, ts);
    }
    bucket
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: single try_acquire latency.
fn bench_try_acquire(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/try_acquire");

    // Cold acquire (first operation on fresh bucket).
    group.bench_function("cold", |b| {
        b.iter_batched(
            || TokenBucket::with_time(100.0, 50.0, 0),
            |mut bucket| {
                black_box(bucket.try_acquire(black_box(1), black_box(0)));
                bucket
            },
            BatchSize::SmallInput,
        );
    });

    // Warm acquire (after 1K operations, time advances).
    group.bench_function("after_1K", |b| {
        b.iter_batched(
            || build_exercised_bucket(1000),
            |mut bucket| {
                black_box(bucket.try_acquire(black_box(1), black_box(999_999)));
                bucket
            },
            BatchSize::SmallInput,
        );
    });

    // Same-timestamp acquire (dt=0 path, no refill computation).
    group.bench_function("dt_zero", |b| {
        b.iter_batched(
            || TokenBucket::with_time(100.0, 50.0, 1000),
            |mut bucket| {
                black_box(bucket.try_acquire(black_box(1), black_box(1000)));
                bucket
            },
            BatchSize::SmallInput,
        );
    });

    // Acquire when empty (denial path).
    group.bench_function("denied", |b| {
        b.iter_batched(
            || TokenBucket::new_empty(10.0, 1.0),
            |mut bucket| {
                black_box(bucket.try_acquire(black_box(1), black_box(0)));
                bucket
            },
            BatchSize::SmallInput,
        );
    });

    // Multi-token acquire.
    group.bench_function("multi_token_5", |b| {
        b.iter_batched(
            || TokenBucket::with_time(100.0, 50.0, 0),
            |mut bucket| {
                black_box(bucket.try_acquire(black_box(5), black_box(100)));
                bucket
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: acquire throughput (many operations).
fn bench_acquire_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/throughput");

    for count in [100, 1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(count));
        let traffic = bursty_traffic(count as usize);

        group.bench_with_input(BenchmarkId::new("bursty", count), &traffic, |b, traffic| {
            b.iter(|| {
                let mut bucket = TokenBucket::with_time(100.0, 50.0, 0);
                for &(cost, ts) in traffic {
                    bucket.try_acquire(black_box(cost), black_box(ts));
                }
                black_box(bucket.total_consumed())
            });
        });
    }

    // Uniform traffic (all cost=1, fixed interval).
    let timestamps = uniform_traffic(10_000, 10);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("uniform_10K", |b| {
        b.iter(|| {
            let mut bucket = TokenBucket::with_time(100.0, 50.0, 0);
            for &ts in &timestamps {
                bucket.try_acquire_one(black_box(ts));
            }
            black_box(bucket.total_consumed())
        });
    });

    group.finish();
}

/// Benchmark: wait_time_ms computation.
fn bench_wait_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/wait_time");

    // Wait time when tokens available (fast path: returns 0).
    group.bench_function("available", |b| {
        b.iter_batched(
            || TokenBucket::with_time(100.0, 50.0, 0),
            |mut bucket| black_box(bucket.wait_time_ms(black_box(1), black_box(0))),
            BatchSize::SmallInput,
        );
    });

    // Wait time when empty (slow path: division + ceil).
    group.bench_function("empty", |b| {
        b.iter_batched(
            || {
                let mut bucket = TokenBucket::with_time(10.0, 2.0, 0);
                bucket.try_acquire(10, 0); // drain it
                bucket
            },
            |mut bucket| black_box(bucket.wait_time_ms(black_box(5), black_box(0))),
            BatchSize::SmallInput,
        );
    });

    // Wait time after partial refill.
    group.bench_function("partial_refill", |b| {
        b.iter_batched(
            || {
                let mut bucket = TokenBucket::with_time(10.0, 2.0, 0);
                bucket.try_acquire(10, 0); // drain it
                bucket
            },
            |mut bucket| black_box(bucket.wait_time_ms(black_box(5), black_box(1000))),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: HierarchicalBucket (two-level rate limiting).
fn bench_hierarchical(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/hierarchical");

    // Both allowed path.
    group.bench_function("both_allowed", |b| {
        b.iter_batched(
            || {
                HierarchicalBucket::new(
                    TokenBucket::with_time(10.0, 5.0, 0),
                    TokenBucket::with_time(100.0, 50.0, 0),
                )
            },
            |mut hb| {
                black_box(hb.try_acquire(black_box(1), black_box(100)));
                hb
            },
            BatchSize::SmallInput,
        );
    });

    // Local denied path.
    group.bench_function("local_denied", |b| {
        b.iter_batched(
            || {
                HierarchicalBucket::new(
                    TokenBucket::new_empty(5.0, 1.0),
                    TokenBucket::with_time(100.0, 50.0, 0),
                )
            },
            |mut hb| {
                black_box(hb.try_acquire(black_box(1), black_box(0)));
                hb
            },
            BatchSize::SmallInput,
        );
    });

    // Global denied path.
    group.bench_function("global_denied", |b| {
        b.iter_batched(
            || {
                HierarchicalBucket::new(
                    TokenBucket::with_time(10.0, 5.0, 0),
                    TokenBucket::new_empty(100.0, 10.0),
                )
            },
            |mut hb| {
                black_box(hb.try_acquire(black_box(1), black_box(0)));
                hb
            },
            BatchSize::SmallInput,
        );
    });

    // Throughput: 10K hierarchical acquires.
    let traffic = bursty_traffic(10_000);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("throughput_10K", |b| {
        b.iter(|| {
            let mut hb = HierarchicalBucket::new(
                TokenBucket::with_time(20.0, 10.0, 0),
                TokenBucket::with_time(200.0, 100.0, 0),
            );
            for &(cost, ts) in &traffic {
                hb.try_acquire(black_box(cost), black_box(ts));
            }
            black_box((hb.local().total_consumed(), hb.global().total_consumed()))
        });
    });

    group.finish();
}

/// Benchmark: reset and refill rate changes.
fn bench_reset_and_reconfig(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/reset_reconfig");

    // Reset after heavy usage.
    group.bench_function("reset_after_1K", |b| {
        b.iter_batched(
            || build_exercised_bucket(1000),
            |mut bucket| {
                bucket.reset(black_box(999_999));
                black_box(bucket)
            },
            BatchSize::SmallInput,
        );
    });

    // Set refill rate.
    group.bench_function("set_refill_rate", |b| {
        b.iter_batched(
            || build_exercised_bucket(100),
            |mut bucket| {
                bucket.set_refill_rate(black_box(100.0));
                black_box(bucket)
            },
            BatchSize::SmallInput,
        );
    });

    // Reset + re-acquire cycle (streaming reuse pattern).
    group.bench_function("reset_reacquire_cycle", |b| {
        b.iter_batched(
            || build_exercised_bucket(1000),
            |mut bucket| {
                bucket.reset(black_box(100_000));
                for i in 0..10u64 {
                    bucket.try_acquire(black_box(1), black_box(100_000 + i * 10));
                }
                black_box(bucket.total_consumed())
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: BucketConfig build.
fn bench_config(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/config");

    let config_full = BucketConfig {
        capacity: 100.0,
        refill_rate: 50.0,
        start_empty: false,
    };

    let config_empty = BucketConfig {
        capacity: 100.0,
        refill_rate: 50.0,
        start_empty: true,
    };

    group.bench_function("build_full", |b| {
        b.iter(|| black_box(config_full.build(black_box(1000))));
    });

    group.bench_function("build_empty", |b| {
        b.iter(|| black_box(config_empty.build(black_box(1000))));
    });

    // Config serde roundtrip.
    group.bench_function("config_serde_roundtrip", |b| {
        b.iter(|| {
            let json = serde_json::to_string(black_box(&config_full)).unwrap();
            black_box(serde_json::from_str::<BucketConfig>(&json).unwrap())
        });
    });

    group.finish();
}

/// Benchmark: stats and telemetry queries.
fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/stats");

    let bucket = build_exercised_bucket(10_000);

    // Stats query.
    group.bench_function("stats_query", |b| {
        b.iter(|| black_box(bucket.stats()));
    });

    // Stats serde roundtrip.
    group.bench_function("stats_serde_roundtrip", |b| {
        let stats = bucket.stats();
        b.iter(|| {
            let json = serde_json::to_string(black_box(&stats)).unwrap();
            black_box(serde_json::from_str::<BucketStats>(&json).unwrap())
        });
    });

    // Telemetry query.
    group.bench_function("telemetry_query", |b| {
        b.iter(|| black_box(bucket.telemetry()));
    });

    // Telemetry serde roundtrip.
    group.bench_function("telemetry_serde_roundtrip", |b| {
        let telem = bucket.telemetry();
        b.iter(|| {
            let json = serde_json::to_string(black_box(&telem)).unwrap();
            black_box(serde_json::from_str::<TokenBucketTelemetrySnapshot>(&json).unwrap())
        });
    });

    group.finish();
}

/// Benchmark: refill rate sensitivity (different rates, same traffic).
fn bench_refill_rate_sensitivity(c: &mut Criterion) {
    let mut group = c.benchmark_group("token_bucket/refill_rate_sensitivity");
    let traffic = bursty_traffic(10_000);
    group.throughput(Throughput::Elements(10_000));

    for rate in [1.0, 10.0, 100.0, 1000.0] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{rate}/s")),
            &rate,
            |b, &rate| {
                b.iter(|| {
                    let mut bucket = TokenBucket::with_time(100.0, rate, 0);
                    for &(cost, ts) in &traffic {
                        bucket.try_acquire(black_box(cost), black_box(ts));
                    }
                    black_box((bucket.total_consumed(), bucket.total_denied()))
                });
            },
        );
    }

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_try_acquire,
    bench_acquire_throughput,
    bench_wait_time,
    bench_hierarchical,
    bench_reset_and_reconfig,
    bench_config,
    bench_stats,
    bench_refill_rate_sensitivity,
);

criterion_main!(benches);
