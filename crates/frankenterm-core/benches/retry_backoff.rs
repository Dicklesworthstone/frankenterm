//! Criterion benchmarks for retry backoff delay calculation.
//!
//! `delay_for_attempt` is called for every retry of every fallible I/O
//! operation. It must be fast even under jitter + high attempt counts.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::retry::RetryPolicy;
use std::hint::black_box;
use std::time::Duration;

// =============================================================================
// Benchmark: delay_for_attempt across attempt numbers
// =============================================================================

fn bench_delay_attempt_scaling(c: &mut Criterion) {
    let policy = RetryPolicy::default();
    let attempts: &[u32] = &[0, 1, 2, 5, 10, 20, 31];

    let mut group = c.benchmark_group("retry/delay_for_attempt");
    for &attempt in attempts {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("attempt_{attempt}")),
            &attempt,
            |b, &n| {
                b.iter(|| policy.delay_for_attempt(black_box(n)));
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark: delay_for_attempt across policy presets
// =============================================================================

fn bench_delay_presets(c: &mut Criterion) {
    let presets: Vec<(&str, RetryPolicy)> = vec![
        ("default", RetryPolicy::default()),
        ("wezterm_cli", RetryPolicy::wezterm_cli()),
        ("db_write", RetryPolicy::db_write()),
        ("webhook", RetryPolicy::webhook()),
        ("browser", RetryPolicy::browser()),
        (
            "no_jitter",
            RetryPolicy::new(
                Duration::from_millis(100),
                Duration::from_secs(30),
                2.0,
                0.0, // no jitter — avoids RNG call
                Some(5),
            ),
        ),
    ];

    let mut group = c.benchmark_group("retry/presets");
    for (name, policy) in &presets {
        group.bench_with_input(BenchmarkId::new("attempt_2", name), policy, |b, p| {
            b.iter(|| p.delay_for_attempt(black_box(2)));
        });
    }
    group.finish();
}

// =============================================================================
// Benchmark: full retry sequence (compute all delays for max_attempts)
// =============================================================================

fn bench_full_delay_sequence(c: &mut Criterion) {
    let mut group = c.benchmark_group("retry/full_sequence");

    let policy_3 = RetryPolicy::default(); // 3 attempts
    group.bench_function("3_attempts", |b| {
        b.iter(|| {
            let p = black_box(&policy_3);
            for i in 0..3 {
                black_box(p.delay_for_attempt(i));
            }
        });
    });

    let policy_10 = RetryPolicy::new(
        Duration::from_millis(50),
        Duration::from_secs(60),
        2.0,
        0.1,
        Some(10),
    );
    group.bench_function("10_attempts", |b| {
        b.iter(|| {
            let p = black_box(&policy_10);
            for i in 0..10 {
                black_box(p.delay_for_attempt(i));
            }
        });
    });

    let policy_31 = RetryPolicy::new(
        Duration::from_millis(10),
        Duration::from_secs(600),
        1.5,
        0.2,
        Some(31),
    );
    group.bench_function("31_attempts_slow_backoff", |b| {
        b.iter(|| {
            let p = black_box(&policy_31);
            for i in 0..31 {
                black_box(p.delay_for_attempt(i));
            }
        });
    });

    group.finish();
}

// =============================================================================
// Benchmark: jitter vs no-jitter overhead
// =============================================================================

fn bench_jitter_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("retry/jitter_overhead");

    let with_jitter = RetryPolicy::new(
        Duration::from_millis(100),
        Duration::from_secs(30),
        2.0,
        0.1,
        Some(5),
    );

    let no_jitter = RetryPolicy::new(
        Duration::from_millis(100),
        Duration::from_secs(30),
        2.0,
        0.0,
        Some(5),
    );

    group.bench_function("with_jitter_10pct", |b| {
        b.iter(|| with_jitter.delay_for_attempt(black_box(2)));
    });

    group.bench_function("no_jitter", |b| {
        b.iter(|| no_jitter.delay_for_attempt(black_box(2)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_delay_attempt_scaling,
    bench_delay_presets,
    bench_full_delay_sequence,
    bench_jitter_overhead,
);
criterion_main!(benches);
