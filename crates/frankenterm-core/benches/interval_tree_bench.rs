//! Criterion benchmarks for augmented interval tree.
//!
//! Bead: (proactive — covers interval_tree.rs module)
//!
//! Performance budgets:
//! - Single insert:          **< 200ns** (BST insert + AVL rebalance + max update)
//! - Point query:            **< 500ns** per query (O(log n + k))
//! - Overlap query:          **< 1µs** per query (O(log n + k))
//! - Remove:                 **< 500ns** per remove
//! - 10K inserts:            **< 2ms** total throughput
//! - iter all:               **< 10µs** for 1K elements

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::interval_tree::{Interval, IntervalTree};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "interval_tree_insert",
        budget: "p50 < 200ns (BST insert + AVL rebalance + max update)",
    },
    bench_common::BenchBudget {
        name: "interval_tree_point_query",
        budget: "p50 < 500ns (O(log n + k) stabbing query)",
    },
    bench_common::BenchBudget {
        name: "interval_tree_overlap_query",
        budget: "p50 < 1us (O(log n + k) overlap query)",
    },
    bench_common::BenchBudget {
        name: "interval_tree_remove",
        budget: "p50 < 500ns per remove",
    },
    bench_common::BenchBudget {
        name: "interval_tree_throughput",
        budget: "10K inserts < 2ms",
    },
    bench_common::BenchBudget {
        name: "interval_tree_iter",
        budget: "< 10us for 1K elements",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Deterministic pseudo-random intervals via LCG.
fn random_intervals(n: usize, range: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let low = (state >> 33) % range;
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let width = 1 + (state >> 33) % (range / 10).max(1);
        out.push((low, low + width));
    }
    out
}

/// Non-overlapping intervals (no collisions).
fn non_overlapping_intervals(n: usize) -> Vec<(u64, u64)> {
    (0..n as u64).map(|i| (i * 100, i * 100 + 50)).collect()
}

/// Highly overlapping intervals (all centered around the same region).
fn dense_intervals(n: usize) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xCAFE_1234;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let center = 500 + (state >> 33) % 100;
        let half_width = 10 + (state >> 50) % 50;
        out.push((center - half_width, center + half_width));
    }
    out
}

/// Pre-build a tree with `n` random intervals.
fn build_tree(n: usize) -> IntervalTree<u64, u64> {
    let mut tree = IntervalTree::new();
    for (i, (low, high)) in random_intervals(n, 10_000).into_iter().enumerate() {
        tree.insert(Interval::new(low, high), i as u64);
    }
    tree
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: insert latency.
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/insert");

    // Cold insert (first element).
    group.bench_function("cold", |b| {
        b.iter_batched(
            IntervalTree::<u64, u64>::new,
            |mut tree| {
                tree.insert(Interval::new(black_box(10), black_box(20)), black_box(1));
                tree
            },
            BatchSize::SmallInput,
        );
    });

    // Insert into tree with 1K elements.
    group.bench_function("after_1K", |b| {
        b.iter_batched(
            || build_tree(1000),
            |mut tree| {
                tree.insert(
                    Interval::new(black_box(5000), black_box(5100)),
                    black_box(9999),
                );
                tree
            },
            BatchSize::SmallInput,
        );
    });

    // Insert into tree with 10K elements.
    group.bench_function("after_10K", |b| {
        b.iter_batched(
            || build_tree(10_000),
            |mut tree| {
                tree.insert(
                    Interval::new(black_box(5000), black_box(5100)),
                    black_box(9999),
                );
                tree
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: insert throughput.
fn bench_insert_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/insert_throughput");

    for count in [100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count));
        let intervals = random_intervals(count as usize, 100_000);

        group.bench_with_input(
            BenchmarkId::new("random", count),
            &intervals,
            |b, intervals| {
                b.iter(|| {
                    let mut tree = IntervalTree::new();
                    for (i, &(low, high)) in intervals.iter().enumerate() {
                        tree.insert(Interval::new(low, high), i as u64);
                    }
                    black_box(tree.len())
                });
            },
        );
    }

    // Non-overlapping intervals (worst case for BST: sorted input).
    let intervals = non_overlapping_intervals(1000);
    group.throughput(Throughput::Elements(1000));
    group.bench_function("non_overlapping_1K", |b| {
        b.iter(|| {
            let mut tree = IntervalTree::new();
            for (i, &(low, high)) in intervals.iter().enumerate() {
                tree.insert(Interval::new(low, high), i as u64);
            }
            black_box(tree.len())
        });
    });

    group.finish();
}

/// Benchmark: point (stabbing) queries.
fn bench_point_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/point_query");

    // Point query on sparse tree (few matches).
    group.bench_function("sparse_1K", |b| {
        let tree = build_tree(1000);
        b.iter(|| black_box(tree.query_point(black_box(&5000))));
    });

    // Point query on dense tree (many matches).
    group.bench_function("dense_1K", |b| {
        let mut tree = IntervalTree::new();
        for (i, (low, high)) in dense_intervals(1000).into_iter().enumerate() {
            tree.insert(Interval::new(low, high), i as u64);
        }
        b.iter(|| black_box(tree.query_point(black_box(&530))));
    });

    // Point query miss (point outside all intervals).
    group.bench_function("miss_1K", |b| {
        let tree = build_tree(1000);
        b.iter(|| black_box(tree.query_point(black_box(&999_999))));
    });

    // Point query at different tree sizes.
    for n in [100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::new("scaling", n), &n, |b, &n| {
            let tree = build_tree(n);
            b.iter(|| black_box(tree.query_point(black_box(&5000))));
        });
    }

    group.finish();
}

/// Benchmark: overlap queries.
fn bench_overlap_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/overlap_query");

    // Narrow query (small interval, few matches).
    group.bench_function("narrow_1K", |b| {
        let tree = build_tree(1000);
        let query = Interval::new(5000, 5010);
        b.iter(|| black_box(tree.query_overlap(black_box(&query))));
    });

    // Wide query (large interval, many matches).
    group.bench_function("wide_1K", |b| {
        let tree = build_tree(1000);
        let query = Interval::new(0, 10_000);
        b.iter(|| black_box(tree.query_overlap(black_box(&query))));
    });

    // No-overlap query.
    group.bench_function("miss_1K", |b| {
        let tree = build_tree(1000);
        let query = Interval::new(999_000, 999_100);
        b.iter(|| black_box(tree.query_overlap(black_box(&query))));
    });

    // Scaling: overlap query at different tree sizes.
    for n in [100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::new("scaling", n), &n, |b, &n| {
            let tree = build_tree(n);
            let query = Interval::new(5000, 5100);
            b.iter(|| black_box(tree.query_overlap(black_box(&query))));
        });
    }

    group.finish();
}

/// Benchmark: remove operations.
fn bench_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/remove");

    // Remove an existing interval.
    group.bench_function("existing_1K", |b| {
        b.iter_batched(
            || {
                let mut tree = IntervalTree::new();
                let intervals = random_intervals(1000, 10_000);
                for (i, (low, high)) in intervals.iter().enumerate() {
                    tree.insert(Interval::new(*low, *high), i as u64);
                }
                let target = Interval::new(intervals[500].0, intervals[500].1);
                (tree, target)
            },
            |(mut tree, target)| {
                black_box(tree.remove(&target));
                tree
            },
            BatchSize::SmallInput,
        );
    });

    // Remove non-existent interval.
    group.bench_function("nonexistent_1K", |b| {
        b.iter_batched(
            || build_tree(1000),
            |mut tree| {
                black_box(tree.remove(&Interval::new(999_999, 1_000_000)));
                tree
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: iteration and auxiliary queries.
fn bench_iteration(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/iteration");

    let tree_1k = build_tree(1000);

    // Iterate all elements.
    group.bench_function("iter_all_1K", |b| {
        b.iter(|| {
            let mut count = 0u64;
            for (interval, value) in tree_1k.iter() {
                black_box((interval, value));
                count += 1;
            }
            black_box(count)
        });
    });

    // Sorted intervals.
    group.bench_function("intervals_sorted_1K", |b| {
        b.iter(|| black_box(tree_1k.intervals_sorted()));
    });

    // min_low / max_high queries.
    group.bench_function("min_low", |b| {
        b.iter(|| black_box(tree_1k.min_low()));
    });

    group.bench_function("max_high", |b| {
        b.iter(|| black_box(tree_1k.max_high()));
    });

    group.bench_function("height", |b| {
        b.iter(|| black_box(tree_1k.height()));
    });

    group.finish();
}

/// Benchmark: Interval type operations.
fn bench_interval_ops(c: &mut Criterion) {
    let mut group = c.benchmark_group("interval_tree/interval_ops");

    let a = Interval::new(10u64, 20);
    let b = Interval::new(15u64, 25);
    let c_interval = Interval::new(30u64, 40);

    group.bench_function("overlaps_true", |bencher| {
        bencher.iter(|| black_box(a.overlaps(black_box(&b))));
    });

    group.bench_function("overlaps_false", |bencher| {
        bencher.iter(|| black_box(a.overlaps(black_box(&c_interval))));
    });

    group.bench_function("contains_point_true", |bencher| {
        bencher.iter(|| black_box(a.contains_point(black_box(&15))));
    });

    group.bench_function("contains_point_false", |bencher| {
        bencher.iter(|| black_box(a.contains_point(black_box(&25))));
    });

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_insert,
    bench_insert_throughput,
    bench_point_query,
    bench_overlap_query,
    bench_remove,
    bench_iteration,
    bench_interval_ops,
);

criterion_main!(benches);
