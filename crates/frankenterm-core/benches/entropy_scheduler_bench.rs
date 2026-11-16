//! Criterion benchmarks for entropy-aware capture scheduling.
//!
//! Bead: wa-283h4.8
//!
//! Performance budgets:
//! - Single byte update: **< 50ns**
//! - 1 KB block update:  **< 1µs**
//! - Per-pane entropy:   **< 10µs** (64 KB window)
//! - Scheduling decision (50 panes): **< 500µs total**
//! - Sliding window eviction: **O(1) per byte**

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;

use frankenterm_core::entropy_accounting::{EntropyEstimator, compute_entropy};
use frankenterm_core::entropy_scheduler::{
    EntropyScheduler, EntropySchedulerConfig, schedule_interval, schedule_interval_default,
};

mod bench_common;

/// Performance budgets for CI regression detection.
#[allow(dead_code)]
const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "entropy_single_byte_update",
        budget: "p50 < 50ns (single byte into estimator)",
    },
    bench_common::BenchBudget {
        name: "entropy_1kb_block",
        budget: "p50 < 1us (1 KB block into estimator)",
    },
    bench_common::BenchBudget {
        name: "entropy_per_pane",
        budget: "p50 < 10us (entropy computation for one pane, 64 KB window)",
    },
    bench_common::BenchBudget {
        name: "scheduling_decision_50_panes",
        budget: "p50 < 500us (schedule() over 50 panes)",
    },
    bench_common::BenchBudget {
        name: "sliding_window_eviction",
        budget: "O(1) per byte (decay overhead amortised)",
    },
    bench_common::BenchBudget {
        name: "schedule_interval_oneshot",
        budget: "p50 < 5us (one-shot interval for 1 KB slice)",
    },
];

// ── Data generators ────────────────────────────────────────────────────

/// Constant stream: all zeros (entropy ≈ 0).
fn constant_data(n: usize) -> Vec<u8> {
    vec![0u8; n]
}

/// Uniform random data (entropy ≈ 8 bits/byte).
fn uniform_data(n: usize) -> Vec<u8> {
    // Deterministic pseudo-random via simple LCG for reproducibility.
    let mut out = Vec::with_capacity(n);
    let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
    for _ in 0..n {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 33) as u8);
    }
    out
}

/// English-like data (entropy ≈ 4-5 bits/byte).
fn english_data(n: usize) -> Vec<u8> {
    let text = b"The quick brown fox jumps over the lazy dog. \
                 FrankenTerm monitors pane output entropy to schedule captures. \
                 High-entropy streams get polled more frequently than repetitive ones. ";
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let remaining = n - out.len();
        let chunk = remaining.min(text.len());
        out.extend_from_slice(&text[..chunk]);
    }
    out
}

// ── Benchmarks ─────────────────────────────────────────────────────────

/// Benchmark: update estimator with a single byte.
/// Target: < 50 ns.
fn bench_single_byte_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/single_byte_update");

    group.bench_function("constant_byte", |b| {
        b.iter_batched(
            || EntropyEstimator::new(65_536),
            |mut est| {
                est.update(black_box(0u8));
                est
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("after_warmup", |b| {
        b.iter_batched(
            || {
                let mut est = EntropyEstimator::new(65_536);
                for i in 0..1000u32 {
                    est.update((i % 256) as u8);
                }
                est
            },
            |mut est| {
                est.update(black_box(42u8));
                est
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: update estimator with a 1 KB block.
/// Target: < 1 µs.
fn bench_1kb_block(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/1kb_block");
    group.throughput(Throughput::Bytes(1024));

    let uniform = uniform_data(1024);
    let english = english_data(1024);
    let constant = constant_data(1024);

    for (label, data) in [
        ("uniform", &uniform),
        ("english", &english),
        ("constant", &constant),
    ] {
        group.bench_with_input(BenchmarkId::from_parameter(label), data, |b, data| {
            b.iter_batched(
                || EntropyEstimator::new(65_536),
                |mut est| {
                    est.update_block(black_box(data));
                    est
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Benchmark: full entropy computation for one pane (64 KB window).
/// Target: < 10 µs.
fn bench_entropy_per_pane(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/entropy_per_pane");

    // Pre-fill an estimator with 64 KB of data, then measure entropy() call cost.
    for (label, data_fn) in [
        ("uniform_64kb", uniform_data as fn(usize) -> Vec<u8>),
        ("english_64kb", english_data as fn(usize) -> Vec<u8>),
        ("constant_64kb", constant_data as fn(usize) -> Vec<u8>),
    ] {
        group.bench_function(label, |b| {
            b.iter_batched(
                || {
                    let mut est = EntropyEstimator::new(65_536);
                    est.update_block(&data_fn(65_536));
                    est
                },
                |mut est| black_box(est.entropy()),
                BatchSize::SmallInput,
            );
        });
    }

    // Also measure compute_entropy() on a full 64 KB slice (no estimator).
    let data_64k = uniform_data(65_536);
    group.bench_function("compute_entropy_64kb", |b| {
        b.iter(|| black_box(compute_entropy(black_box(&data_64k))));
    });

    group.finish();
}

/// Benchmark: schedule() over 50 panes.
/// Target: < 500 µs total.
fn bench_scheduling_decision(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/scheduling_decision");

    for pane_count in [10, 50, 100] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{pane_count}_panes")),
            &pane_count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
                        let uniform = uniform_data(2048);
                        let english = english_data(2048);
                        for i in 0..count {
                            sched.register_pane(i as u64);
                            // Alternate data types for realistic variance.
                            let data = if i % 2 == 0 { &uniform } else { &english };
                            sched.feed_bytes(i as u64, data);
                        }
                        sched
                    },
                    |mut sched| black_box(sched.schedule()),
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Benchmark: sliding window decay overhead.
/// Target: O(1) amortised per byte.
///
/// Measures the cost of feeding bytes through the decay boundary
/// (when total exceeds 2× window_size).
fn bench_sliding_window_eviction(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/sliding_window_eviction");
    group.throughput(Throughput::Bytes(4096));

    // Feed 4 KB chunks into an estimator that has already filled its window,
    // triggering periodic decay.
    let chunk = uniform_data(4096);

    group.bench_function("4kb_post_fill", |b| {
        b.iter_batched(
            || {
                let mut est = EntropyEstimator::new(65_536);
                // Fill past the window so decay triggers on next large feed.
                est.update_block(&uniform_data(120_000));
                est
            },
            |mut est| {
                est.update_block(black_box(&chunk));
                est
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Benchmark: one-shot schedule_interval for a data slice.
/// Target: < 5 µs for 1 KB.
fn bench_schedule_interval_oneshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/schedule_interval_oneshot");

    let config = EntropySchedulerConfig::default();
    let data_1k = uniform_data(1024);
    let data_4k = english_data(4096);

    group.bench_function("uniform_1kb", |b| {
        b.iter(|| black_box(schedule_interval(black_box(&data_1k), &config)));
    });

    group.bench_function("english_4kb", |b| {
        b.iter(|| black_box(schedule_interval(black_box(&data_4k), &config)));
    });

    group.bench_function("default_config_1kb", |b| {
        b.iter(|| black_box(schedule_interval_default(black_box(&data_1k))));
    });

    group.finish();
}

/// Benchmark: feed_bytes throughput into EntropyScheduler.
/// Measures end-to-end scheduler overhead (lookup + estimator update + recompute).
fn bench_feed_bytes_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/feed_bytes_throughput");

    for size in [256, 1024, 4096, 16384] {
        group.throughput(Throughput::Bytes(size as u64));
        let data = uniform_data(size);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{size}B")),
            &data,
            |b, data| {
                b.iter_batched(
                    || {
                        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
                        sched.register_pane(0);
                        sched
                    },
                    |mut sched| {
                        sched.feed_bytes(0, black_box(data));
                        sched
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Benchmark: snapshot serialization.
fn bench_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("entropy_scheduler/snapshot");

    for pane_count in [10, 50] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{pane_count}_panes")),
            &pane_count,
            |b, &count| {
                b.iter_batched(
                    || {
                        let mut sched = EntropyScheduler::new(EntropySchedulerConfig::default());
                        let data = english_data(1024);
                        for i in 0..count {
                            sched.register_pane(i as u64);
                            sched.feed_bytes(i as u64, &data);
                        }
                        sched
                    },
                    |sched| {
                        let snap = sched.snapshot();
                        black_box(serde_json::to_string(&snap).unwrap())
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

// ── Criterion harness ──────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_single_byte_update,
    bench_1kb_block,
    bench_entropy_per_pane,
    bench_scheduling_decision,
    bench_sliding_window_eviction,
    bench_schedule_interval_oneshot,
    bench_feed_bytes_throughput,
    bench_snapshot,
);

criterion_main!(benches);
