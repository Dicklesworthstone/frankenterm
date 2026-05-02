//! Heavy-burst frame-budget bench (ft-uzj0s sub-task 8).
//!
//! Simulates 1 MB/s aggregate output across 50 panes routing through
//! the [`SustainedBurstHarness`] deferred-op queue, asserts the
//! RQ-S6 SLO: **p95 input latency < 50 ms**.
//!
//! ## Why an assertion-bearing bench
//!
//! Same shape as `dirty_tracking_typing` (ft-37j54): criterion measures
//! iteration time; the SLO target is the per-frame queue cycle latency
//! the frame-budget pipeline records. The bench's iteration body times
//! each simulated frame using `std::time::Instant`, records into a
//! local p95 histogram, and after a warmup window asserts p95 ≤ 50 ms.
//! A regression that re-introduces super-linear work on the bulk-drain
//! path will fail the assertion before criterion publishes timing.
//!
//! ## Bead acceptance (sub-task 8)
//!
//! - 1 MB/s output across 50 panes.
//! - Assert input latency p95 < 50 ms (RQ-S6).
//! - Exercise bulk-drain (try_bulk_drain after Required ops) — modelled
//!   here via `SustainedBurstHarness::drain` per frame, mirroring the
//!   integration's `try_bulk_drain` call after Required-op execution.
//!
//! Sub-task 7 (sustained_burst.rs regression test) shipped at
//! commit 492c9d623; this bench is the perf-side counterpart.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::frame_budget_signal_coupling::SustainedBurstHarness;

/// 50 panes per the bead.
const PANE_COUNT: u32 = 50;

/// Approximate per-pane bytes/second budget so the aggregate hits
/// 1 MB/s (1_000_000 / 50 = 20_000 bytes/s/pane). At ~16 ms per
/// frame (60 Hz), each pane produces ~320 bytes/frame which we
/// model as ~10 deferred-op pushes (every ~32 bytes ≈ one cell
/// boundary the renderer would consider a deferred animation).
const PUSHES_PER_PANE_PER_FRAME: u32 = 10;

/// Drain rate under load: bulk-drain pulls 60% of the per-frame
/// pushes — leaves ~40% headroom that exercises the cap-eviction
/// path the SustainedBurstHarness substrate models.
const DRAIN_RATIO_NUM: u32 = 6;
const DRAIN_RATIO_DEN: u32 = 10;

/// Frames per criterion iteration. 240 gives a stable p95 estimate
/// (target/240 ≈ 12 frames in the tail bucket).
const FRAMES_PER_ITER: u32 = 240;
const WARMUP_FRAMES: u32 = 16;

/// RQ-S6 budget: p95 < 50 ms.
const P95_BUDGET_US: u64 = 50_000;

/// Deferred-op cap. Sized so a per-frame burst overflows the queue
/// often enough to exercise the eviction (drop-counter) path under
/// the bead's "drop counter takes over at deferred_cap" wording.
const DEFERRED_CAP: usize = 256;

/// Drive one frame of the heavy-burst pattern and return the
/// per-frame elapsed micros. Inline-never so the optimiser cannot
/// fuse the timer span with the caller.
#[inline(never)]
fn drive_one_frame(harness: &mut SustainedBurstHarness) -> u64 {
    let pushes_per_frame = PANE_COUNT * PUSHES_PER_PANE_PER_FRAME;
    let drains_per_frame = (pushes_per_frame * DRAIN_RATIO_NUM) / DRAIN_RATIO_DEN;

    let start = Instant::now();
    harness.run_burst(1, pushes_per_frame, drains_per_frame);
    let elapsed = start.elapsed().as_micros() as u64;
    // Touch the queue depth so the optimiser cannot prove the
    // harness state is dead.
    black_box(harness.queue_within_cap());
    elapsed
}

/// Compute the p95 of `samples` (sort + index). 95th percentile of
/// a 224-sample vector is index 213 (round-up).
fn percentile_us(samples: &mut [u64], p: u8) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let p = u64::from(p.min(100));
    let target = ((samples.len() as u64 * p).div_ceil(100)).max(1) as usize - 1;
    samples[target.min(samples.len() - 1)]
}

fn bench_heavy_burst(c: &mut Criterion) {
    let mut group = c.benchmark_group("heavy_burst");
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("50_panes_1mbps_p95_under_50ms", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;

            for _outer in 0..iters {
                let mut harness = SustainedBurstHarness::new(DEFERRED_CAP);
                let mut samples: Vec<u64> =
                    Vec::with_capacity((FRAMES_PER_ITER - WARMUP_FRAMES) as usize);

                let outer_start = Instant::now();
                for frame in 0..FRAMES_PER_ITER {
                    let frame_us = drive_one_frame(&mut harness);
                    if frame >= WARMUP_FRAMES {
                        samples.push(frame_us);
                    }
                }
                total += outer_start.elapsed();

                let p95 = percentile_us(&mut samples, 95);
                assert!(
                    p95 <= P95_BUDGET_US,
                    "heavy_burst: p95 frame latency exceeded {} µs target. \
                     p95 = {} µs over {} samples; queue_within_cap = {}, \
                     deferred_cap = {}",
                    P95_BUDGET_US,
                    p95,
                    samples.len(),
                    harness.queue_within_cap(),
                    harness.deferred_cap(),
                );
            }

            total
        });
    });

    group.finish();
}

criterion_group!(benches, bench_heavy_burst);
criterion_main!(benches);
