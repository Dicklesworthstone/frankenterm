//! Criterion benchmarks for RateLimitTracker hot-path operations.
//!
//! `record_at` is called per detection, `provider_status_at` per scheduling
//! decision, and `is_pane_rate_limited_at` per ingest filter check. All are
//! synchronous and O(n) in tracked panes.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::patterns::AgentType;
use frankenterm_core::rate_limit_tracker::RateLimitTracker;
use std::hint::black_box;
use std::time::{Duration, Instant};

// =============================================================================
// Helpers
// =============================================================================

fn populate_tracker(n_panes: usize) -> (RateLimitTracker, Instant) {
    let mut tracker = RateLimitTracker::new();
    let base = Instant::now();
    for i in 0..n_panes {
        tracker.record_at(
            i as u64,
            AgentType::ClaudeCode,
            format!("rule-{i}"),
            Some("30 seconds".to_string()),
            base,
        );
    }
    (tracker, base)
}

// =============================================================================
// Benchmark: record_at (hot path per detection)
// =============================================================================

fn bench_record_at(c: &mut Criterion) {
    let mut group = c.benchmark_group("rate_limit/record_at");

    // Empty tracker — first insert
    group.bench_function("empty_first_insert", |b| {
        b.iter_batched(
            RateLimitTracker::new,
            |mut tracker| {
                tracker.record_at(
                    black_box(1),
                    AgentType::ClaudeCode,
                    "bench-rule".to_string(),
                    Some("60 seconds".to_string()),
                    Instant::now(),
                );
                tracker
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Tracker with 100 panes — new pane insert
    group.bench_function("100_panes_new_insert", |b| {
        b.iter_batched(
            || populate_tracker(100),
            |(mut tracker, base)| {
                tracker.record_at(
                    black_box(999),
                    AgentType::Gemini,
                    "bench-rule".to_string(),
                    Some("5 minutes".to_string()),
                    base + Duration::from_secs(1),
                );
                tracker
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Tracker with 100 panes — existing pane update
    group.bench_function("100_panes_existing_update", |b| {
        b.iter_batched(
            || populate_tracker(100),
            |(mut tracker, base)| {
                tracker.record_at(
                    black_box(50),
                    AgentType::ClaudeCode,
                    "bench-rule-update".to_string(),
                    Some("120 seconds".to_string()),
                    base + Duration::from_secs(1),
                );
                tracker
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // No retry-after text — uses default cooldown
    group.bench_function("no_retry_after", |b| {
        b.iter_batched(
            RateLimitTracker::new,
            |mut tracker| {
                tracker.record_at(
                    black_box(1),
                    AgentType::ClaudeCode,
                    "bench-rule".to_string(),
                    None,
                    Instant::now(),
                );
                tracker
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

// =============================================================================
// Benchmark: is_pane_rate_limited_at (called per ingest check)
// =============================================================================

fn bench_is_pane_rate_limited(c: &mut Criterion) {
    let pane_counts: &[usize] = &[10, 50, 100, 500];

    let mut group = c.benchmark_group("rate_limit/is_pane_rate_limited");
    for &n in pane_counts {
        let (tracker, base) = populate_tracker(n);
        let now = base + Duration::from_secs(1);

        // Check existing pane (hit)
        group.bench_with_input(
            BenchmarkId::new("existing", n),
            &(n / 2) as &usize,
            |b, &pane| {
                b.iter(|| tracker.is_pane_rate_limited_at(black_box(pane as u64), now));
            },
        );

        // Check non-existing pane (miss)
        group.bench_with_input(
            BenchmarkId::new("missing", n),
            &(n + 100) as &usize,
            |b, &pane| {
                b.iter(|| tracker.is_pane_rate_limited_at(black_box(pane as u64), now));
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark: provider_status_at (O(n) aggregation)
// =============================================================================

fn bench_provider_status(c: &mut Criterion) {
    let pane_counts: &[usize] = &[10, 50, 100, 500];

    let mut group = c.benchmark_group("rate_limit/provider_status");
    for &n in pane_counts {
        let (tracker, base) = populate_tracker(n);
        let now = base + Duration::from_secs(1);

        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{n}_panes")),
            &n,
            |b, _| {
                b.iter(|| tracker.provider_status_at(black_box(AgentType::ClaudeCode), now));
            },
        );
    }
    group.finish();
}

// =============================================================================
// Benchmark: gc_at (periodic garbage collection)
// =============================================================================

fn bench_gc(c: &mut Criterion) {
    let mut group = c.benchmark_group("rate_limit/gc");

    // GC with all panes still active (no collection) — use iter_batched
    // to get fresh trackers since gc_at mutates state.
    group.bench_function("100_panes_all_active", |b| {
        b.iter_batched(
            || {
                let (tracker, base) = populate_tracker(100);
                (tracker, base + Duration::from_secs(10))
            },
            |(mut tracker, gc_now)| {
                tracker.gc_at(black_box(gc_now));
                tracker
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // GC with all panes expired
    group.bench_function("100_panes_all_expired", |b| {
        b.iter_batched(
            || {
                let (tracker, base) = populate_tracker(100);
                (tracker, base + Duration::from_secs(600))
            },
            |(mut tracker, gc_expired)| {
                tracker.gc_at(black_box(gc_expired));
                tracker
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_record_at,
    bench_is_pane_rate_limited,
    bench_provider_status,
    bench_gc,
);
criterion_main!(benches);
