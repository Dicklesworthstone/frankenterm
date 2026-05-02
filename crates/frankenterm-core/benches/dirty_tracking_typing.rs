//! Dirty-tracking typing-cadence bench (ft-37j54).
//!
//! Simulates a 200-pane fleet with 1 cell/frame typed across all panes,
//! routes the per-frame work through the substrate's `DirtyMark`
//! classification + `FramePaintLatencyHistogram`, and asserts the
//! RQ-S1 / RQ-S8 SLO: **p99 frame-paint latency < 100 µs**.
//!
//! ## Why an assertion-bearing bench
//!
//! Criterion measures iteration time, but the SLO target is the
//! per-frame paint latency the dirty-tracking pipeline records. The
//! bench's iteration body times each simulated frame using
//! `std::time::Instant`, records into the histogram, and after a
//! warm-up window asserts the histogram's p99 against the SLO config.
//! A regression that re-introduces O(N) work per cell on the typing
//! hot path will fail the assertion before criterion publishes
//! its timing summary.
//!
//! ## Bead acceptance
//!
//! - 200-pane fleet, 1 cell/frame.
//! - p99 frame-paint < 100 µs (RQ-S1 / RQ-S8).
//! - Substrate's `FramePaintLatencyHistogram` + `meets_p99_target`
//!   already exist (ft-jvj78 / ft-5ykn9); this bench is the
//!   regression-guard consumer.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::dirty_line_telemetry::{
    DirtyEventSource, DirtyMark, DirtyMarkClassification, DirtyTelemetryConfig,
    FramePaintLatencyHistogram,
};

const PANE_COUNT: u32 = 200;
const ROWS_PER_PANE: u32 = 50;
/// Frames recorded into the histogram per criterion iteration. 256
/// gives a stable p99 estimate (target/256 ≈ 2.5 frames in the tail
/// bucket — coarse but enough to catch a hot-path regression).
const FRAMES_PER_ITER: u32 = 256;
/// Warm-up frames discarded before histogram recording starts. Keeps
/// the cold-cache first-frame outliers out of the SLO check.
const WARMUP_FRAMES: u32 = 16;

/// Build the per-pane DirtyMark for "1 cell/frame typed at row R".
/// Pane / row chosen pseudo-deterministically from the frame index so
/// the same iteration produces the same trace (criterion stability).
fn typing_mark(frame: u32, pane_id: u64) -> DirtyMark {
    let row = (frame.wrapping_mul(2654435761)) % ROWS_PER_PANE;
    DirtyMark {
        pane_id,
        source: DirtyEventSource::Pty,
        start_row: row,
        end_row: row + 1,
    }
}

/// Drive one synthetic frame through the dirty-tracking pipeline:
/// mark every pane with one Single-classified row, classify, and
/// black_box the result so the optimiser cannot fold the work away.
#[inline(never)]
fn drive_one_frame(frame: u32) {
    for pane_id in 0..u64::from(PANE_COUNT) {
        let mark = typing_mark(frame, pane_id);
        let classification = mark.classify(ROWS_PER_PANE);
        black_box(classification);
        // The substrate's per-frame work for a typed cell is:
        // mark + classify + record into the bitmap. The bitmap
        // structure is not in this bench (it lives in the GUI
        // crate); the mark+classify is the core-side hot path.
        debug_assert_eq!(classification, DirtyMarkClassification::Single);
    }
}

fn bench_dirty_tracking_typing(c: &mut Criterion) {
    let mut group = c.benchmark_group("dirty_tracking/typing");
    // Long enough for criterion to settle without dragging out CI.
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("200_panes_1_cell_per_frame", |b| {
        b.iter_custom(|iters| {
            let config = DirtyTelemetryConfig::default();
            let mut histogram = FramePaintLatencyHistogram::new();
            let mut total = Duration::ZERO;

            for _outer in 0..iters {
                let outer_start = Instant::now();
                for frame in 0..FRAMES_PER_ITER {
                    let frame_start = Instant::now();
                    drive_one_frame(frame);
                    let elapsed_us = frame_start.elapsed().as_micros() as u32;
                    if frame >= WARMUP_FRAMES {
                        histogram.record(elapsed_us);
                    }
                }
                total += outer_start.elapsed();
            }

            // SLO check — fail loudly if a regression introduces
            // >100 µs p99 for the typing hot path. The substrate's
            // meets_p99_target consults the histogram bucket
            // boundaries (≤100 µs bucket = pass).
            assert!(
                histogram.meets_p99_target(config),
                "dirty_tracking/typing: p99 paint latency exceeded {} µs target. \
                 Histogram percentile_us(99) = {:?}, total samples = {}",
                config.p99_budget_us,
                histogram.percentile_us(99),
                histogram.total,
            );

            total
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dirty_tracking_typing);
criterion_main!(benches);
