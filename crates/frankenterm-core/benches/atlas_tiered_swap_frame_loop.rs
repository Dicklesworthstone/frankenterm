//! Atlas tiered-swap per-frame loop bench (br-ft-fqza3).
//!
//! Closes the bench gap surfaced by the atlas tiered-swap performance
//! review: existing criterion targets cover `atlas_packing` (glyph
//! bin-packing) and `heavy_burst` (SustainedBurstHarness), but no
//! bench drives the production-shape per-frame loop documented at
//! `docs/render/atlas-tiered-swap-wgpu-integration.md:72-85`:
//!
//!     1. take carry-over from prior frame
//!     2. drain StagingTransferQueue (events pushed since last frame)
//!     3. partition by frame budget via FrameBudgetSwapDeferrer
//!     4. emit AtlasBlitPlan::from_events(admitted) → wgpu copy commands
//!     5. carry deferred over to the next frame
//!     6. FrameBudgetSwapDeferrer::reset_for_new_frame()
//!
//! The loop is performance-sensitive — regressions like repeated
//! drain/refill allocation, admitted/deferred Vec churn through
//! `partition()`, or quadratic command-plan allocation would be
//! invisible to existing benches. This bench exercises five workload
//! shapes spanning the realistic envelope:
//!
//! - `empty`: steady-state minimum (no events queued)
//! - `steady_small`: 5 events/frame, all admit (typical idle frame)
//! - `mixed_promote_demote`: 20 events alternating direction
//!   (typical scroll: regions demote out + promote back as the
//!   viewport sweeps the atlas)
//! - `carry_over_heavy`: 100 events/frame at a tight budget (~half
//!   admit, half carry over — exercises `partition()` allocation
//!   plus carry-over re-enqueue)
//! - `zero_budget_recovery`: first frame budget = 0 (everything
//!   defers to carry-over); subsequent frames at normal budget
//!   drain the backlog. Catches regressions where carry-over
//!   unbounded-growth would not surface on a steady-load run.
//!
//! Every scenario records per-frame elapsed µs over `FRAMES_PER_ITER`
//! frames (after `WARMUP_FRAMES` warmup) and asserts p95 ≤
//! `P95_BUDGET_US`. Pure-Rust Vec ops over ≤ 100 events should
//! comfortably stay under the 200µs envelope on any modern host;
//! a regression that fuses an O(N²) traversal into the per-frame
//! path will fail the assertion before criterion publishes timing.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use frankenterm_core::atlas_staging_blit_plan::AtlasBlitPlan;
use frankenterm_core::atlas_tiered_swap::{
    FrameBudgetSwapDeferrer, HostRamStagingAllocation, StagingTransferDirection,
    StagingTransferEvent, StagingTransferQueue,
};

// ---------------------------------------------------------------------------
// Frame-loop tuning constants
// ---------------------------------------------------------------------------

/// Frames per criterion iteration. 240 gives a stable p95 estimate
/// (target/240 ≈ 12 frames in the tail bucket).
const FRAMES_PER_ITER: u32 = 240;
const WARMUP_FRAMES: u32 = 16;

/// p95 budget across all scenarios (microseconds). Pure-Rust Vec
/// ops over ≤ 100 events should not break 50 µs even on the
/// carry-over-heavy scenario; the 200 µs envelope catches
/// quadratic-blowup regressions while leaving headroom for noisy
/// CI hosts.
const P95_BUDGET_US: u64 = 200;

/// 60 fps target × 20% reserve for VRAM blits per the operator
/// playbook (`docs/render/atlas-tiered-swap-operator-playbook.md`).
/// 16.67 ms × 0.20 ≈ 3 333 µs.
const PRODUCTION_FRAME_BUDGET_US: u64 = 3_333;

/// Tight budget that forces ~half of a 100-event burst to defer
/// at the default 12 GB/s throughput (`FrameBudgetSwapDeferrer::new`).
/// 100 × 256 KiB = 25 MiB/frame; 25 MiB ÷ 12 GB/s ≈ 2.1 ms; budget
/// of 1 ms admits ~half.
const TIGHT_FRAME_BUDGET_US: u64 = 1_000;

// ---------------------------------------------------------------------------
// Event factory
// ---------------------------------------------------------------------------

fn make_event(
    region_id: u64,
    direction: StagingTransferDirection,
    offset: u64,
    bytes: u64,
    frame_id: u64,
) -> StagingTransferEvent {
    StagingTransferEvent {
        region_id,
        direction,
        allocation: HostRamStagingAllocation {
            region_id,
            offset,
            bytes,
        },
        frame_id,
    }
}

/// Push `count` events into `queue`. Direction alternates per event
/// when `mixed` is true (Demote / Promote / Demote / ...) so the
/// blit plan exercises both `download_bytes` and `upload_bytes`
/// accounting. Each event is `bytes_per_event` bytes.
fn seed_queue(
    queue: &mut StagingTransferQueue,
    count: u32,
    mixed: bool,
    bytes_per_event: u64,
    frame_id: u64,
) {
    for i in 0..count {
        let direction = if mixed && i.is_multiple_of(2) {
            StagingTransferDirection::Promote
        } else {
            StagingTransferDirection::Demote
        };
        queue.push(make_event(
            u64::from(i),
            direction,
            u64::from(i) * bytes_per_event,
            bytes_per_event,
            frame_id,
        ));
    }
}

// ---------------------------------------------------------------------------
// Per-frame production loop
// ---------------------------------------------------------------------------

/// Drive one frame of the production-shape loop:
///
/// 1. take carry-over Vec from the prior frame
/// 2. drain the StagingTransferQueue
/// 3. partition through the deferrer (frame-budget gate)
/// 4. build the AtlasBlitPlan
/// 5. carry deferred events over to the next frame
/// 6. reset the deferrer's per-frame admitted accumulator
///
/// Returns elapsed µs for this frame. `inline(never)` so the
/// optimiser cannot fuse the timer span with the caller.
#[inline(never)]
fn drive_one_frame(
    queue: &mut StagingTransferQueue,
    deferrer: &mut FrameBudgetSwapDeferrer,
    carry_over: &mut Vec<StagingTransferEvent>,
    frame_budget_us: u64,
) -> u64 {
    let start = Instant::now();

    // Step 1+2: take carry-over and append this frame's drained queue.
    let mut events = std::mem::take(carry_over);
    let drained = queue.drain_pending();
    events.extend(drained);

    // Step 3: partition by frame budget. Production API allocates
    // two Vecs per call — this is the cost the bench is auditing.
    let (admitted, deferred) = deferrer.partition(&events, frame_budget_us);

    // Step 4: build the AtlasBlitPlan from admitted events. Allocates
    // a Vec<AtlasBlitCommand> sized to admitted.len().
    let plan = AtlasBlitPlan::from_events(&admitted);

    // Touch the plan so the optimiser cannot prove it is dead.
    black_box(&plan);
    black_box(plan.total_bytes());

    // Step 5: carry deferred over to the next frame.
    *carry_over = deferred;

    // Step 6: reset for the next frame so the budget accumulator
    // starts clean.
    deferrer.reset_for_new_frame();

    start.elapsed().as_micros() as u64
}

/// Compute the pth percentile of `samples` (sort + index).
/// p95 of a 224-sample vector is index 213.
fn percentile_us(samples: &mut [u64], p: u8) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let p = u64::from(p.min(100));
    let target = ((samples.len() as u64 * p).div_ceil(100)).max(1) as usize - 1;
    samples[target.min(samples.len() - 1)]
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Scenario shape — controls what each frame's `seed_queue` call
/// pushes plus the per-frame budget the deferrer gates against.
#[derive(Debug, Clone, Copy)]
struct Scenario {
    name: &'static str,
    events_per_frame: u32,
    bytes_per_event: u64,
    mixed_directions: bool,
    frame_budget_us: u64,
    /// When true, the first frame runs with budget = 0 so every
    /// event defers; subsequent frames run with `frame_budget_us`
    /// to drain the backlog. Tests carry-over recovery.
    zero_budget_first_frame: bool,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "empty",
        events_per_frame: 0,
        bytes_per_event: 0,
        mixed_directions: false,
        frame_budget_us: PRODUCTION_FRAME_BUDGET_US,
        zero_budget_first_frame: false,
    },
    Scenario {
        name: "steady_small",
        events_per_frame: 5,
        bytes_per_event: 1024, // 1 KiB
        mixed_directions: false,
        frame_budget_us: PRODUCTION_FRAME_BUDGET_US,
        zero_budget_first_frame: false,
    },
    Scenario {
        name: "mixed_promote_demote",
        events_per_frame: 20,
        bytes_per_event: 8 * 1024, // 8 KiB
        mixed_directions: true,
        frame_budget_us: PRODUCTION_FRAME_BUDGET_US,
        zero_budget_first_frame: false,
    },
    Scenario {
        name: "carry_over_heavy",
        events_per_frame: 100,
        bytes_per_event: 256 * 1024, // 256 KiB
        mixed_directions: true,
        frame_budget_us: TIGHT_FRAME_BUDGET_US,
        zero_budget_first_frame: false,
    },
    Scenario {
        name: "zero_budget_recovery",
        events_per_frame: 100,
        bytes_per_event: 8 * 1024, // 8 KiB — small enough that the
        // recovery frame admits the whole backlog plus its own push.
        mixed_directions: true,
        frame_budget_us: PRODUCTION_FRAME_BUDGET_US,
        zero_budget_first_frame: true,
    },
];

fn drive_scenario(scenario: Scenario, frames: u32) -> Vec<u64> {
    let mut queue = StagingTransferQueue::new();
    let mut deferrer = FrameBudgetSwapDeferrer::new();
    let mut carry_over: Vec<StagingTransferEvent> = Vec::new();
    let mut samples: Vec<u64> = Vec::with_capacity(frames.saturating_sub(WARMUP_FRAMES) as usize);

    for frame in 0..frames {
        if scenario.events_per_frame > 0 {
            seed_queue(
                &mut queue,
                scenario.events_per_frame,
                scenario.mixed_directions,
                scenario.bytes_per_event,
                u64::from(frame),
            );
        }
        let budget = if scenario.zero_budget_first_frame && frame == 0 {
            0
        } else {
            scenario.frame_budget_us
        };
        let elapsed = drive_one_frame(&mut queue, &mut deferrer, &mut carry_over, budget);
        if frame >= WARMUP_FRAMES {
            samples.push(elapsed);
        }
    }
    samples
}

fn bench_atlas_frame_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("atlas_tiered_swap_frame_loop");
    group.measurement_time(Duration::from_secs(3));

    for scenario in SCENARIOS {
        group.bench_with_input(
            BenchmarkId::from_parameter(scenario.name),
            scenario,
            |b, scenario| {
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _outer in 0..iters {
                        let outer_start = Instant::now();
                        let mut samples = drive_scenario(*scenario, FRAMES_PER_ITER);
                        total += outer_start.elapsed();

                        let p50 = percentile_us(&mut samples, 50);
                        let p95 = percentile_us(&mut samples, 95);
                        assert!(
                            p95 <= P95_BUDGET_US,
                            "atlas_tiered_swap_frame_loop[{name}]: p95 frame latency \
                             exceeded {budget} µs target. p50 = {p50} µs, p95 = {p95} µs \
                             over {samples} samples (events/frame = {events}, \
                             bytes/event = {bytes}, frame_budget = {frame_budget} µs)",
                            name = scenario.name,
                            budget = P95_BUDGET_US,
                            p50 = p50,
                            p95 = p95,
                            samples = samples.len(),
                            events = scenario.events_per_frame,
                            bytes = scenario.bytes_per_event,
                            frame_budget = scenario.frame_budget_us,
                        );
                        // Surface p50 to the optimiser so the
                        // assertion's branch isn't elided.
                        black_box(p50);
                    }
                    total
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_atlas_frame_loop);
criterion_main!(benches);
