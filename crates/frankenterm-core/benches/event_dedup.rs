//! Benchmarks for `EventDeduplicator` and `NotificationCooldown` throughput.
//!
//! Performance budgets:
//! - dedup check (hit): **< 100ns** per call
//! - dedup check (miss/new): **< 200ns** per call
//! - cooldown check: **< 100ns** per call
//! - eviction at capacity: **< 500ns** per call

use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::events::{EventDeduplicator, NotificationCooldown};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "event_dedup/check_duplicate",
        budget: "< 100ns per dedup hit",
    },
    bench_common::BenchBudget {
        name: "event_dedup/check_new_keys",
        budget: "< 200ns per dedup miss (new key insertion)",
    },
    bench_common::BenchBudget {
        name: "event_dedup/eviction_at_capacity",
        budget: "< 500ns per check when at max capacity",
    },
    bench_common::BenchBudget {
        name: "event_dedup/cooldown_check",
        budget: "< 100ns per cooldown check",
    },
];

/// Bench: repeated checks of the same key (all duplicates after the first).
fn bench_check_duplicate(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_dedup");

    for &count in &[100_u64, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("check_duplicate", count),
            &count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut dedup = EventDeduplicator::new();
                        dedup.check("warmup-key"); // prime the entry
                        dedup
                    },
                    |mut dedup| {
                        for _ in 0..count {
                            black_box(dedup.check("warmup-key"));
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Bench: inserting distinct keys (all new, no duplicates).
fn bench_check_new_keys(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_dedup");

    for &count in &[100_u64, 1_000, 5_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("check_new_keys", count),
            &count,
            |b, &count| {
                let keys: Vec<String> = (0..count).map(|i| format!("key-{i}")).collect();
                b.iter_batched(
                    || EventDeduplicator::new(),
                    |mut dedup| {
                        for key in &keys {
                            black_box(dedup.check(key));
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Bench: check() at max capacity, forcing eviction on every insert.
fn bench_eviction_at_capacity(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_dedup");

    let capacity = 500_usize;
    let extra = 500_u64;
    group.throughput(Throughput::Elements(extra));

    group.bench_function("eviction_at_capacity", |b| {
        // Pre-generate keys beyond capacity
        let overflow_keys: Vec<String> =
            (capacity..capacity + extra as usize).map(|i| format!("overflow-{i}")).collect();

        b.iter_batched(
            || {
                let mut dedup = EventDeduplicator::with_config(Duration::from_secs(300), capacity);
                for i in 0..capacity {
                    dedup.check(&format!("fill-{i}"));
                }
                dedup
            },
            |mut dedup| {
                for key in &overflow_keys {
                    black_box(dedup.check(key));
                }
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

/// Bench: NotificationCooldown check throughput (mixed send/suppress).
fn bench_cooldown_check(c: &mut Criterion) {
    let mut group = c.benchmark_group("event_dedup");

    for &count in &[100_u64, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count));
        group.bench_with_input(
            BenchmarkId::new("cooldown_check", count),
            &count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut cooldown = NotificationCooldown::new();
                        cooldown.check("prime"); // first call is always Send
                        cooldown
                    },
                    |mut cooldown| {
                        for _ in 0..count {
                            // Within cooldown window, so all Suppress (fast path)
                            black_box(cooldown.check("prime"));
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("event_dedup", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_check_duplicate,
        bench_check_new_keys,
        bench_eviction_at_capacity,
        bench_cooldown_check
);
criterion_main!(benches);
