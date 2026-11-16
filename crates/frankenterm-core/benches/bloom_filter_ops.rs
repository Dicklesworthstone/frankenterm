//! Benchmarks for `BloomFilter` and `CountingBloomFilter` hot-path ops.
//!
//! Performance budgets:
//! - insert: **< 50ns** per element (10K-capacity filter)
//! - contains (hit): **< 40ns** per lookup
//! - contains (miss): **< 40ns** per lookup
//! - counting insert+remove cycle: **< 120ns** per cycle

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::bloom_filter::{BloomFilter, CountingBloomFilter};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "bloom_filter/insert",
        budget: "< 50ns per insert (10K capacity, 1% FP)",
    },
    bench_common::BenchBudget {
        name: "bloom_filter/contains_hit",
        budget: "< 40ns per positive lookup",
    },
    bench_common::BenchBudget {
        name: "bloom_filter/contains_miss",
        budget: "< 40ns per negative lookup",
    },
    bench_common::BenchBudget {
        name: "bloom_filter/counting_insert_remove",
        budget: "< 120ns per insert+remove cycle",
    },
];

/// Pre-generate byte keys for consistent benchmarking.
fn make_keys(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|i| format!("key-{i:06}").into_bytes()).collect()
}

/// Bench: BloomFilter insert throughput at varying capacities.
fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");

    for &cap in &[1_000_usize, 10_000, 100_000] {
        let fill = cap / 2; // insert half capacity
        group.throughput(Throughput::Elements(fill as u64));
        group.bench_with_input(
            BenchmarkId::new("insert", cap),
            &(cap, fill),
            |b, &(cap, fill)| {
                let keys = make_keys(fill);
                b.iter_batched(
                    || BloomFilter::with_capacity(cap, 0.01),
                    |mut bf| {
                        for k in &keys {
                            bf.insert(k);
                        }
                        black_box(bf.count());
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Bench: BloomFilter contains() for elements that ARE present (hit path).
fn bench_contains_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");

    let cap = 10_000_usize;
    let n = 5_000_usize;
    let keys = make_keys(n);
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("contains_hit", |b| {
        b.iter_batched(
            || {
                let mut bf = BloomFilter::with_capacity(cap, 0.01);
                for k in &keys {
                    bf.insert(k);
                }
                bf
            },
            |bf| {
                let mut hits = 0_u64;
                for k in &keys {
                    if bf.contains(k) {
                        hits += 1;
                    }
                }
                black_box(hits);
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

/// Bench: BloomFilter contains() for elements that are NOT present (miss path).
fn bench_contains_miss(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");

    let cap = 10_000_usize;
    let n = 5_000_usize;
    let inserted_keys = make_keys(n);
    // Generate different keys guaranteed not inserted
    let miss_keys: Vec<Vec<u8>> = (0..n)
        .map(|i| format!("miss-{i:06}").into_bytes())
        .collect();
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("contains_miss", |b| {
        b.iter_batched(
            || {
                let mut bf = BloomFilter::with_capacity(cap, 0.01);
                for k in &inserted_keys {
                    bf.insert(k);
                }
                bf
            },
            |bf| {
                let mut misses = 0_u64;
                for k in &miss_keys {
                    if !bf.contains(k) {
                        misses += 1;
                    }
                }
                black_box(misses);
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

/// Bench: CountingBloomFilter insert+remove cycle throughput.
fn bench_counting_insert_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("bloom_filter");

    let cap = 10_000_usize;
    let n = 2_000_usize;
    let keys = make_keys(n);
    group.throughput(Throughput::Elements(n as u64));

    group.bench_function("counting_insert_remove", |b| {
        b.iter_batched(
            || CountingBloomFilter::with_capacity(cap, 0.01),
            |mut cbf| {
                for k in &keys {
                    cbf.insert(k);
                }
                for k in &keys {
                    cbf.remove(k);
                }
                black_box(cbf.count());
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("bloom_filter", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_insert,
        bench_contains_hit,
        bench_contains_miss,
        bench_counting_insert_remove
);
criterion_main!(benches);
