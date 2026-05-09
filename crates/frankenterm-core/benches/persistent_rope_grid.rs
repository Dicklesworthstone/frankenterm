//! Bench harness for persistent_rope_grid (`ft-mpc9b.2.5`).
//!
//! The bead's decision rubric:
//!
//! > Ship rope-backed grid IFF reflow ≥2× faster on 200-pane fleet
//! > AND memory overhead ≤30% AND render thread unaffected.
//! > Otherwise: archive prototype + negative-result doc.
//!
//! This harness measures the three relevant operations at terminal-
//! grid sizes and emits Criterion reports the decision doc cites.
//!
//! ## Workloads
//!
//! - **Snapshot.** Clone the grid for the renderer. RopeGrid clone
//!   is `Arc::clone(&root)` — O(1). FlatGrid clone is full deep
//!   copy — O(N).
//!   Expected outcome: rope wins by orders of magnitude.
//! - **Reflow (per-line set).** Replace every line in a 1000-line
//!   grid with a new value. RopeGrid does O(log n) tree path-clone
//!   per `set_line`; FlatGrid does O(1) `Vec` slot write.
//!   Expected outcome: flat wins by ~5-10×.
//! - **Mixed (set + clone every 10 ops).** Models the real
//!   render+typing workload: writer mutates, reader snapshots
//!   periodically.
//!   Expected outcome: depends on the snapshot frequency.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::persistent_rope_grid::{Cell, FlatGrid, Line, RopeGrid, TerminalGridOps};
use std::hint::black_box;

const COLS: usize = 80;

fn line_of(ch: char) -> Line {
    (0..COLS).map(|_| Cell::new(ch, 1, 0)).collect()
}

fn flat_with(n: usize) -> FlatGrid {
    let lines: Vec<Line> = (0..n).map(|_| line_of('a')).collect();
    FlatGrid::new(lines)
}

fn rope_with(n: usize) -> RopeGrid {
    let lines: Vec<Line> = (0..n).map(|_| line_of('a')).collect();
    RopeGrid::new(lines)
}

// ============================================================================
// Snapshot bench — the rope's headline win.
// ============================================================================

fn bench_snapshot_1000_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_rope_grid_snapshot_1000");
    group.throughput(Throughput::Elements(1));

    group.bench_function("flat_clone", |b| {
        let g = flat_with(1_000);
        b.iter(|| {
            let snap = g.clone();
            black_box(snap);
        })
    });

    group.bench_function("rope_clone", |b| {
        let g = rope_with(1_000);
        b.iter(|| {
            let snap = g.clone();
            black_box(snap);
        })
    });

    group.finish();
}

fn bench_snapshot_10k_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_rope_grid_snapshot_10000");
    group.throughput(Throughput::Elements(1));

    group.bench_function("flat_clone", |b| {
        let g = flat_with(10_000);
        b.iter(|| {
            let snap = g.clone();
            black_box(snap);
        })
    });

    group.bench_function("rope_clone", |b| {
        let g = rope_with(10_000);
        b.iter(|| {
            let snap = g.clone();
            black_box(snap);
        })
    });

    group.finish();
}

// ============================================================================
// Reflow bench — rebuild every line. Per-line `set_line` modeled.
// ============================================================================

fn bench_full_reflow_1000_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_rope_grid_reflow_1000");
    group.throughput(Throughput::Elements(1_000));

    group.bench_function("flat_set_every_line", |b| {
        b.iter_batched(
            || flat_with(1_000),
            |mut g| {
                for i in 0..g.line_count() {
                    g.set_line(i, line_of('b'));
                }
                black_box(g.line_count());
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("rope_set_every_line", |b| {
        b.iter_batched(
            || rope_with(1_000),
            |mut g| {
                for i in 0..g.line_count() {
                    g.set_line(i, line_of('b'));
                }
                black_box(g.line_count());
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

// ============================================================================
// Mixed workload — set + periodic snapshot.
// ============================================================================

fn bench_mixed_1000_lines(c: &mut Criterion) {
    let mut group = c.benchmark_group("persistent_rope_grid_mixed_1000_set_per_snapshot");
    group.throughput(Throughput::Elements(100));

    group.bench_function("flat_100_sets_then_snapshot", |b| {
        let g = flat_with(1_000);
        b.iter(|| {
            let mut g = g.clone();
            for i in 0..100 {
                g.set_line(i, line_of('c'));
            }
            let snap = g.clone();
            black_box(snap);
        })
    });

    group.bench_function("rope_100_sets_then_snapshot", |b| {
        let g = rope_with(1_000);
        b.iter(|| {
            let mut g = g.clone();
            for i in 0..100 {
                g.set_line(i, line_of('c'));
            }
            let snap = g.clone();
            black_box(snap);
        })
    });

    group.finish();
}

criterion_group!(
    persistent_rope_grid_benches,
    bench_snapshot_1000_lines,
    bench_snapshot_10k_lines,
    bench_full_reflow_1000_lines,
    bench_mixed_1000_lines,
);
criterion_main!(persistent_rope_grid_benches);
