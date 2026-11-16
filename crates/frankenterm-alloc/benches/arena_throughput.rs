//! Benchmarks for per-pane arena allocation throughput.
//!
//! Performance budgets:
//! - Single arena reserve: **< 500ns**
//! - Single arena release: **< 500ns**
//! - Tracked bytes update: **< 200ns**
//! - 200-pane reserve cycle: **< 100us**
//! - 200-pane full lifecycle (reserve + track + release): **< 500us**
//! - Stats snapshot (200 panes): **< 50us**

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_alloc::PaneArenaRegistry;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fresh_registry_with_panes(count: u64) -> PaneArenaRegistry {
    let registry = PaneArenaRegistry::new();
    for pane_id in 0..count {
        registry.reserve(pane_id);
    }
    registry
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_single_reserve(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_reserve");
    group.throughput(Throughput::Elements(1));

    group.bench_function("single_reserve", |b| {
        let registry = PaneArenaRegistry::new();
        let mut pane_id = 0u64;
        b.iter(|| {
            registry.reserve(pane_id);
            registry.release(pane_id);
            pane_id = pane_id.wrapping_add(1);
        });
    });

    group.finish();
}

fn bench_single_release(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_release");
    group.throughput(Throughput::Elements(1));

    group.bench_function("single_release", |b| {
        let registry = PaneArenaRegistry::new();
        let mut pane_id = 0u64;
        b.iter_batched(
            || {
                let id = pane_id;
                registry.reserve(id);
                pane_id = pane_id.wrapping_add(1);
                id
            },
            |id| registry.release(id),
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_tracked_bytes_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_tracked_bytes");
    group.throughput(Throughput::Elements(1));

    group.bench_function("set_tracked_bytes", |b| {
        let registry = PaneArenaRegistry::new();
        registry.reserve(0);
        let mut bytes = 0usize;
        b.iter(|| {
            bytes = bytes.wrapping_add(4096);
            registry.set_tracked_bytes(0, bytes);
        });
    });

    group.finish();
}

fn bench_batch_reserve(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_batch_reserve");

    for count in [10, 50, 100, 200] {
        group.throughput(Throughput::Elements(count));

        group.bench_with_input(
            BenchmarkId::new("reserve_n_panes", count),
            &count,
            |b, &n| {
                b.iter(|| {
                    let registry = PaneArenaRegistry::new();
                    for pane_id in 0..n {
                        registry.reserve(pane_id);
                    }
                    registry
                });
            },
        );
    }

    group.finish();
}

fn bench_full_lifecycle(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_full_lifecycle");

    for count in [10, 50, 100, 200] {
        group.throughput(Throughput::Elements(count));

        group.bench_with_input(
            BenchmarkId::new("reserve_track_release", count),
            &count,
            |b, &n| {
                b.iter(|| {
                    let registry = PaneArenaRegistry::new();

                    // Reserve all panes
                    for pane_id in 0..n {
                        registry.reserve(pane_id);
                    }

                    // Simulate scrollback accounting updates (5 rounds)
                    for round in 0..5u64 {
                        for pane_id in 0..n {
                            let bytes = ((pane_id + 1) * 4096 * (round + 1)) as usize;
                            registry.set_tracked_bytes(pane_id, bytes);
                        }
                    }

                    // Release all panes
                    for pane_id in 0..n {
                        registry.release(pane_id);
                    }

                    assert!(registry.is_empty());
                });
            },
        );
    }

    group.finish();
}

fn bench_stats_snapshot(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_stats_snapshot");

    for count in [10, 50, 100, 200] {
        group.throughput(Throughput::Elements(count));

        group.bench_with_input(
            BenchmarkId::new("stats_snapshot", count),
            &count,
            |b, &n| {
                let registry = fresh_registry_with_panes(n);
                // Populate some tracked bytes
                for pane_id in 0..n {
                    registry.set_tracked_bytes(pane_id, (pane_id as usize + 1) * 1024);
                }

                b.iter(|| registry.stats_snapshot());
            },
        );
    }

    group.finish();
}

fn bench_concurrent_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_contention");

    // Simulate the pattern of concurrent reserve + set_tracked_bytes
    // that happens during runtime (ingest threads updating arenas).
    group.bench_function("interleaved_reserve_and_track_200", |b| {
        b.iter(|| {
            let registry = PaneArenaRegistry::new();

            // Interleave reserve and tracking (realistic pattern)
            for pane_id in 0..200u64 {
                registry.reserve(pane_id);
                // Immediately start tracking
                registry.set_tracked_bytes(pane_id, 4096);
            }

            // Second accounting pass
            for pane_id in 0..200u64 {
                registry.set_tracked_bytes(pane_id, 8192);
            }

            // Snapshot for diagnostics
            let snap = registry.stats_snapshot();
            assert_eq!(snap.len(), 200);

            // Teardown
            for pane_id in 0..200u64 {
                registry.release(pane_id);
            }
        });
    });

    group.finish();
}

fn bench_rapid_churn(c: &mut Criterion) {
    let mut group = c.benchmark_group("arena_rapid_churn");

    // Simulate rapid pane creation/destruction (agent restarts)
    group.bench_function("churn_200_panes_10_rounds", |b| {
        b.iter(|| {
            let registry = PaneArenaRegistry::new();

            for _round in 0..10 {
                // Create 200 panes
                for pane_id in 0..200u64 {
                    registry.reserve(pane_id);
                    registry.set_tracked_bytes(pane_id, 4096);
                }
                // Destroy them all
                for pane_id in 0..200u64 {
                    registry.release(pane_id);
                }
                assert!(registry.is_empty());
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion harness
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_single_reserve,
    bench_single_release,
    bench_tracked_bytes_update,
    bench_batch_reserve,
    bench_full_lifecycle,
    bench_stats_snapshot,
    bench_concurrent_contention,
    bench_rapid_churn,
);
criterion_main!(benches);
