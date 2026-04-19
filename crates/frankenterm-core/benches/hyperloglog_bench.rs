//! Criterion benchmarks for HyperLogLog++ approximate distinct count estimator.
//!
//! Bead: ft-283h4.21
//!
//! Performance budgets:
//! - Single insert:       **< 50ns** (hash + register update)
//! - Cardinality query:   **< 100µs** at p=14 (16K registers)
//! - Merge two HLLs:      **< 50µs** at p=14
//! - 10K inserts:         **< 500µs** (amortised throughput)
//! - Jaccard similarity:  **< 300µs** at p=14

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::hyperloglog::HyperLogLog;

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "hll_single_insert",
        budget: "p50 < 50ns (hash + register update)",
    },
    bench_common::BenchBudget {
        name: "hll_cardinality_query",
        budget: "p50 < 100us at p=14 (16K registers)",
    },
    bench_common::BenchBudget {
        name: "hll_merge",
        budget: "p50 < 50us at p=14",
    },
    bench_common::BenchBudget {
        name: "hll_insert_throughput",
        budget: "10K inserts < 500us",
    },
    bench_common::BenchBudget {
        name: "hll_jaccard",
        budget: "p50 < 300us at p=14",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Deterministic pseudo-random u64 sequence via LCG.
fn random_u64s(n: usize) -> Vec<u64> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push(state);
    }
    out
}

/// Pre-build an HLL with `n` distinct elements.
fn build_hll(precision: u8, n: usize) -> HyperLogLog {
    let mut hll = HyperLogLog::with_precision(precision);
    for i in 0..n as u64 {
        hll.insert(&i);
    }
    hll
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: insert throughput at different cardinalities.
fn bench_insert_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/insert_throughput");

    for count in [100, 1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(count));
        let values = random_u64s(count as usize);

        group.bench_with_input(
            BenchmarkId::new("distinct", count),
            &values,
            |b, values| {
                b.iter(|| {
                    let mut hll = HyperLogLog::new();
                    for v in values {
                        hll.insert(black_box(v));
                    }
                    black_box(hll.cardinality())
                });
            },
        );
    }

    // Insert duplicates only (same element repeated).
    group.bench_function("10K_duplicates", |b| {
        b.iter(|| {
            let mut hll = HyperLogLog::new();
            for _ in 0..10_000 {
                hll.insert(black_box(&42u64));
            }
            black_box(hll.cardinality())
        });
    });

    group.finish();
}

/// Benchmark: single insert latency.
fn bench_single_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/single_insert");

    // Cold insert (empty HLL).
    group.bench_function("cold", |b| {
        b.iter_batched(
            HyperLogLog::new,
            |mut hll| {
                hll.insert(black_box(&42u64));
                hll
            },
            BatchSize::SmallInput,
        );
    });

    // Warm insert (HLL already has 10K elements).
    group.bench_function("after_10K", |b| {
        b.iter_batched(
            || build_hll(14, 10_000),
            |mut hll| {
                hll.insert(black_box(&99999u64));
                hll
            },
            BatchSize::SmallInput,
        );
    });

    // Insert pre-computed hash.
    group.bench_function("insert_hash", |b| {
        b.iter_batched(
            HyperLogLog::new,
            |mut hll| {
                hll.insert_hash(black_box(0x1234_5678_9ABC_DEF0));
                hll
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: cardinality estimation at different sizes and precisions.
fn bench_cardinality(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/cardinality");

    // Vary cardinality at default precision (p=14).
    for n in [100, 1_000, 10_000, 100_000] {
        group.bench_with_input(
            BenchmarkId::new("p14", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || build_hll(14, n),
                    |hll| black_box(hll.cardinality()),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Vary precision at fixed cardinality (10K).
    for p in [4, 8, 12, 14, 16, 18] {
        group.bench_with_input(
            BenchmarkId::new("10K_elements", format!("p{p}")),
            &p,
            |b, &p| {
                b.iter_batched(
                    || build_hll(p, 10_000),
                    |hll| black_box(hll.cardinality()),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Empty HLL cardinality (linear counting path).
    group.bench_function("empty", |b| {
        let hll = HyperLogLog::new();
        b.iter(|| black_box(hll.cardinality()));
    });

    group.finish();
}

/// Benchmark: merge two HyperLogLogs.
fn bench_merge(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/merge");

    for p in [8, 14, 18] {
        let n = 10_000;
        group.bench_with_input(
            BenchmarkId::new("10K_each", format!("p{p}")),
            &p,
            |b, &p| {
                b.iter_batched(
                    || {
                        let mut a = HyperLogLog::with_precision(p);
                        let mut b = HyperLogLog::with_precision(p);
                        for i in 0..n as u64 {
                            a.insert(&i);
                            b.insert(&(i + n as u64 / 2));
                        }
                        (a, b)
                    },
                    |(mut a, b)| {
                        a.merge(&b).unwrap();
                        black_box(a.cardinality())
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    // Merge empty into populated.
    group.bench_function("merge_empty_p14", |b| {
        b.iter_batched(
            || {
                let hll = build_hll(14, 10_000);
                let empty = HyperLogLog::new();
                (hll, empty)
            },
            |(mut hll, empty)| {
                hll.merge(&empty).unwrap();
                black_box(hll)
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: Jaccard similarity estimation.
fn bench_jaccard(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/jaccard");

    // High overlap (50% shared elements).
    group.bench_function("high_overlap_10K", |b| {
        b.iter_batched(
            || {
                let mut a = HyperLogLog::new();
                let mut b_hll = HyperLogLog::new();
                for i in 0..10_000u64 {
                    a.insert(&i);
                }
                for i in 5_000..15_000u64 {
                    b_hll.insert(&i);
                }
                (a, b_hll)
            },
            |(a, b_hll)| black_box(a.jaccard(&b_hll)),
            BatchSize::SmallInput,
        );
    });

    // No overlap.
    group.bench_function("no_overlap_10K", |b| {
        b.iter_batched(
            || {
                let mut a = HyperLogLog::new();
                let mut b_hll = HyperLogLog::new();
                for i in 0..10_000u64 {
                    a.insert(&i);
                }
                for i in 100_000..110_000u64 {
                    b_hll.insert(&i);
                }
                (a, b_hll)
            },
            |(a, b_hll)| black_box(a.jaccard(&b_hll)),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: precision vs accuracy trade-off.
/// Measures how precision affects both insert speed and estimation quality.
fn bench_precision_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/precision_scaling");

    let n = 50_000;
    for p in [4, 8, 12, 14, 16, 18] {
        group.bench_with_input(
            BenchmarkId::new("insert_50K", format!("p{p}")),
            &p,
            |b, &p| {
                let values = random_u64s(n);
                b.iter(|| {
                    let mut hll = HyperLogLog::with_precision(p);
                    for v in &values {
                        hll.insert(black_box(v));
                    }
                    black_box(hll.cardinality())
                });
            },
        );
    }

    group.finish();
}

/// Benchmark: stats() and auxiliary queries.
fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/stats");

    let hll = build_hll(14, 10_000);

    group.bench_function("stats_10K", |b| {
        b.iter(|| black_box(hll.stats()));
    });

    group.bench_function("nonzero_registers", |b| {
        b.iter(|| black_box(hll.nonzero_registers()));
    });

    group.bench_function("standard_error", |b| {
        b.iter(|| black_box(hll.standard_error()));
    });

    group.bench_function("memory_bytes", |b| {
        b.iter(|| black_box(hll.memory_bytes()));
    });

    group.finish();
}

/// Benchmark: clear and reuse pattern.
fn bench_clear_reuse(c: &mut Criterion) {
    let mut group = c.benchmark_group("hyperloglog/clear_reuse");

    group.bench_function("clear_p14_after_10K", |b| {
        b.iter_batched(
            || build_hll(14, 10_000),
            |mut hll| {
                hll.clear();
                black_box(hll)
            },
            BatchSize::SmallInput,
        );
    });

    // Clear + re-insert cycle.
    group.bench_function("clear_reinsert_1K", |b| {
        let values = random_u64s(1000);
        b.iter_batched(
            || build_hll(14, 10_000),
            |mut hll| {
                hll.clear();
                for v in &values {
                    hll.insert(black_box(v));
                }
                black_box(hll.cardinality())
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_insert_throughput,
    bench_single_insert,
    bench_cardinality,
    bench_merge,
    bench_jaccard,
    bench_precision_scaling,
    bench_stats,
    bench_clear_reuse,
);

criterion_main!(benches);
