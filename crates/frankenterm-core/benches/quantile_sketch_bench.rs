//! Criterion benchmarks for TDigest streaming quantile sketch.
//!
//! Bead: ft-283h4.20
//!
//! Performance budgets:
//! - Single insert:       **< 50ns** (amortised, buffer + occasional compress)
//! - Quantile query:      **< 1µs** (binary-ish centroid walk)
//! - CDF query:           **< 1µs** (centroid walk)
//! - Mean query:          **< 500ns** (sum over centroids)
//! - Merge two digests:   **< 50µs** (sort + compress)
//! - 10K inserts:         **< 500µs** total throughput

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::quantile_sketch::{TDigest, TDigestConfig, TDigestStats};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "tdigest_single_insert",
        budget: "p50 < 50ns amortised (buffer + occasional compress)",
    },
    bench_common::BenchBudget {
        name: "tdigest_quantile_query",
        budget: "p50 < 1us (centroid walk)",
    },
    bench_common::BenchBudget {
        name: "tdigest_cdf_query",
        budget: "p50 < 1us (centroid walk)",
    },
    bench_common::BenchBudget {
        name: "tdigest_merge",
        budget: "p50 < 50us (sort + compress)",
    },
    bench_common::BenchBudget {
        name: "tdigest_insert_throughput",
        budget: "10K inserts < 500us",
    },
    bench_common::BenchBudget {
        name: "tdigest_stats_serde",
        budget: "p50 < 200ns (stats JSON roundtrip)",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Deterministic pseudo-random f64 values via LCG.
fn random_values(n: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // Map to roughly [0, 1000)
        let v = (state >> 33) as f64 / u32::MAX as f64 * 1000.0;
        out.push(v);
    }
    out
}

/// Generate values from a bimodal distribution (two peaks).
fn bimodal_values(n: usize) -> Vec<f64> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xCAFE_1234;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let r = (state >> 33) as f64 / u32::MAX as f64;
        // 50% around 100, 50% around 900
        let v = if r < 0.5 {
            100.0 + r * 40.0
        } else {
            (r - 0.5).mul_add(40.0, 900.0)
        };
        out.push(v);
    }
    out
}

/// Pre-build a warmed-up TDigest with `n` values.
fn build_digest(n: usize) -> TDigest {
    let mut td = TDigest::new();
    for v in random_values(n) {
        td.insert(v);
    }
    // Force flush buffer
    let _ = td.quantile(0.5);
    td
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: single insert latency.
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/insert");

    // Cold insert (first value on empty digest).
    group.bench_function("cold", |b| {
        b.iter_batched(
            TDigest::new,
            |mut td| {
                td.insert(black_box(42.0));
                td
            },
            BatchSize::SmallInput,
        );
    });

    // Warm insert (after 1K values, buffer partially full).
    group.bench_function("after_1K", |b| {
        b.iter_batched(
            || build_digest(1000),
            |mut td| {
                td.insert(black_box(500.0));
                td
            },
            BatchSize::SmallInput,
        );
    });

    // Insert that triggers compression (buffer at capacity).
    group.bench_function("triggers_compress", |b| {
        b.iter_batched(
            || {
                let mut td = TDigest::new();
                // Fill buffer to capacity - 1
                for v in random_values(499) {
                    td.insert(v);
                }
                td
            },
            |mut td| {
                td.insert(black_box(999.0)); // 500th insert triggers compress
                td
            },
            BatchSize::SmallInput,
        );
    });

    // Weighted insert.
    group.bench_function("weighted", |b| {
        b.iter_batched(
            || build_digest(1000),
            |mut td| {
                td.insert_weighted(black_box(500.0), black_box(10.0));
                td
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: insert throughput at different scales.
fn bench_insert_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/insert_throughput");

    for count in [100, 1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(count));
        let data = random_values(count as usize);

        group.bench_with_input(
            BenchmarkId::new("random", count),
            &data,
            |b, data| {
                b.iter(|| {
                    let mut td = TDigest::new();
                    for &v in data {
                        td.insert(black_box(v));
                    }
                    black_box(td.count())
                });
            },
        );
    }

    // Bimodal distribution throughput.
    let data = bimodal_values(10_000);
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("bimodal_10K", |b| {
        b.iter(|| {
            let mut td = TDigest::new();
            for &v in &data {
                td.insert(black_box(v));
            }
            black_box(td.count())
        });
    });

    group.finish();
}

/// Benchmark: quantile queries.
fn bench_quantile(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/quantile");

    // Median query on warmed digest.
    group.bench_function("p50_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.quantile(black_box(0.5))),
            BatchSize::SmallInput,
        );
    });

    // Tail quantile (p99).
    group.bench_function("p99_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.quantile(black_box(0.99))),
            BatchSize::SmallInput,
        );
    });

    // Extreme tail (p99.9).
    group.bench_function("p999_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.quantile(black_box(0.999))),
            BatchSize::SmallInput,
        );
    });

    // Multiple quantile queries on same digest.
    group.bench_function("multi_quantiles_5", |b| {
        let quantiles = [0.1, 0.25, 0.5, 0.75, 0.99];
        b.iter_batched(
            || build_digest(10_000),
            |mut td| {
                let mut results = [0.0f64; 5];
                for (i, &q) in quantiles.iter().enumerate() {
                    results[i] = td.quantile(black_box(q));
                }
                black_box(results)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: CDF queries.
fn bench_cdf(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/cdf");

    group.bench_function("middle_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.cdf(black_box(500.0))),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("tail_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.cdf(black_box(990.0))),
            BatchSize::SmallInput,
        );
    });

    group.bench_function("below_min", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.cdf(black_box(-1.0))),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: merge two digests.
fn bench_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/merge");

    for n in [1_000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("equal_size", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let a = build_digest(n);
                        let b_td = build_digest(n);
                        (a, b_td)
                    },
                    |(mut a, b_td)| {
                        a.merge(black_box(&b_td));
                        black_box(a.count())
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Merge empty into populated.
    group.bench_function("merge_empty", |b| {
        b.iter_batched(
            || {
                let td = build_digest(10_000);
                let empty = TDigest::new();
                (td, empty)
            },
            |(mut td, empty)| {
                td.merge(black_box(&empty));
                black_box(td)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: compression parameter sensitivity.
fn bench_compression_sensitivity(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/compression_sensitivity");
    let data = random_values(10_000);
    group.throughput(Throughput::Elements(10_000));

    for compression in [20.0, 50.0, 100.0, 200.0, 500.0] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("d{compression}")),
            &compression,
            |b, &compression| {
                b.iter(|| {
                    let mut td = TDigest::with_compression(compression);
                    for &v in &data {
                        td.insert(black_box(v));
                    }
                    black_box(td.quantile(0.5))
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: stats and serde.
fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/stats");

    let td = build_digest(10_000);

    group.bench_function("stats_query", |b| {
        b.iter(|| black_box(td.stats()));
    });

    group.bench_function("stats_serde_roundtrip", |b| {
        let stats = td.stats();
        b.iter(|| {
            let json = serde_json::to_string(black_box(&stats)).unwrap();
            black_box(serde_json::from_str::<TDigestStats>(&json).unwrap())
        });
    });

    // Config serde roundtrip.
    group.bench_function("config_serde_roundtrip", |b| {
        let config = TDigestConfig::default();
        b.iter(|| {
            let json = serde_json::to_string(black_box(&config)).unwrap();
            black_box(serde_json::from_str::<TDigestConfig>(&json).unwrap())
        });
    });

    group.finish();
}

/// Benchmark: clear and reuse pattern.
fn bench_clear_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/clear_reuse");

    group.bench_function("clear_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| {
                td.clear();
                black_box(td)
            },
            BatchSize::SmallInput,
        );
    });

    // Clear + re-insert cycle.
    group.bench_function("clear_reinsert_1K", |b| {
        let values = random_values(1000);
        b.iter_batched(
            || build_digest(10_000),
            |mut td| {
                td.clear();
                for &v in &values {
                    td.insert(black_box(v));
                }
                black_box(td.count())
            },
            BatchSize::SmallInput,
        );
    });

    // Reset with new compression.
    group.bench_function("reset_new_compression", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| {
                td.reset(black_box(200.0));
                black_box(td)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: mean computation.
fn bench_mean(c: &mut Criterion) {
    let mut group = c.benchmark_group("tdigest/mean");

    group.bench_function("mean_after_10K", |b| {
        b.iter_batched(
            || build_digest(10_000),
            |mut td| black_box(td.mean()),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_insert,
    bench_insert_throughput,
    bench_quantile,
    bench_cdf,
    bench_merge,
    bench_compression_sensitivity,
    bench_stats,
    bench_clear_reuse,
    bench_mean,
);

criterion_main!(benches);
