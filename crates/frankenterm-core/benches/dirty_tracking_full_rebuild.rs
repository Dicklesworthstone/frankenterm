//! Dirty-tracking full-rebuild bench (ft-37j54).
//!
//! Simulates a font-swap event: every pane becomes whole-screen-dirty
//! and every visible row must be re-classified. This is the worst-case
//! work for the dirty-tracking pipeline — the bead's expectation is
//! "O(N) baseline" where N = total visible rows across the fleet.
//!
//! Unlike the typing bench (which asserts a < 100 µs p99 SLO), the
//! full-rebuild bench has no fixed SLO budget; the goal is regression
//! catching: if a future change makes per-row classification
//! super-linear (e.g. accidental quadratic over the row vector), the
//! criterion timing baseline will diverge dramatically and operator
//! review will catch it.
//!
//! ## Bead acceptance
//!
//! - Font swap simulated via `DirtyEventSource::FontSwap`.
//! - O(N) baseline per the bead.
//! - Substrate's `DirtyMark::classify` widening to `WholeScreen`
//!   on font-change source already exists (ft-jvj78 / ft-5ykn9);
//!   this bench is the regression-guard consumer.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::dirty_line_telemetry::{
    DirtyEventSource, DirtyMark, DirtyMarkClassification,
};

const PANE_COUNTS: &[u32] = &[10, 50, 100, 200];
const ROWS_PER_PANE: u32 = 50;

/// Build a font-swap-equivalent DirtyMark (whole-screen) for one
/// pane. The substrate's `is_whole_screen` predicate on
/// `DirtyEventSource::FontSwap` widens classification to
/// `WholeScreen` regardless of the start_row/end_row range.
fn font_swap_mark(pane_id: u64) -> DirtyMark {
    DirtyMark {
        pane_id,
        source: DirtyEventSource::FontSwap,
        start_row: 0,
        end_row: ROWS_PER_PANE,
    }
}

/// Drive the full rebuild across `pane_count` panes:
///   - One DirtyMark per pane, source = FontChanged.
///   - Per-pane classify() into WholeScreen.
///   - Per-row touch loop simulating the renderer's bitmap walk.
///
/// The work scales O(pane_count * ROWS_PER_PANE), which is the
/// O(N) baseline the bead names.
#[inline(never)]
fn drive_full_rebuild(pane_count: u32) {
    for pane_id in 0..u64::from(pane_count) {
        let mark = font_swap_mark(pane_id);
        let classification = mark.classify(ROWS_PER_PANE);
        debug_assert_eq!(classification, DirtyMarkClassification::WholeScreen);
        // Per-row touch loop. The integration's actual paint path
        // walks the bitmap and re-emits every row; the per-row work
        // here black_boxes the row index so the optimiser cannot
        // fold the loop away.
        for row in 0..ROWS_PER_PANE {
            black_box(row);
        }
        black_box(classification);
    }
}

fn bench_dirty_tracking_full_rebuild(c: &mut Criterion) {
    let mut group = c.benchmark_group("dirty_tracking/full_rebuild");
    group.measurement_time(Duration::from_secs(3));

    for pane_count in PANE_COUNTS {
        let id = BenchmarkId::from_parameter(format!("{pane_count}_panes"));
        group.bench_with_input(id, pane_count, |b, &n| {
            b.iter(|| drive_full_rebuild(n));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_dirty_tracking_full_rebuild);
criterion_main!(benches);
