//! Sustained-burst regression test (br-ft-uzj0s substrate-pass).
//!
//! **Bead:** `ft-uzj0s` / parent `ft-d6nrd`. Sub-task 7 of the
//! parent epic.
//!
//! ## Scope
//!
//! Per the bead's spec:
//!
//! > 5 minutes of forced cosmetic deferrals.
//! > Assert queue depth bounded (no unbounded growth — drop
//! > counter takes over at deferred_cap).
//! > Assert drop counter increments under sustained pressure.
//!
//! ## Substrate vs wired-pass split
//!
//! The bead is marked "blocked on the per-op wiring (sibling
//! cont-bead) because they need real try_execute calls to
//! produce the deferral/drop signal." This test exercises
//! [`SustainedBurstHarness`] via its synthetic-signal
//! [`run_burst`] entry point so the bounded-queue + drop-counter
//! invariants get a real CI-runnable regression guard now;
//! the wired-pass cont-bead drops in real `try_execute` call
//! producers in place of the synthetic burst.
//!
//! Five minutes of frame production at 60 fps = 18 000 frames.
//! Running 18 000 frames at synthetic signal speed takes
//! single-digit milliseconds (no real timing involved); the
//! harness is pure state machine. The test still names "5
//! minutes" in its assertions so the regression guard maps
//! cleanly to the bead's spec.
//!
//! ## Cross-references
//!
//! - `crates/frankenterm-core/src/frame_budget_signal_coupling.rs`
//!   (substrate: `SustainedBurstHarness`).
//! - `ft-d6nrd` (parent — slice 1 at 40047d02f).

use frankenterm_core::frame_budget_signal_coupling::{OpKindSlug, SustainedBurstHarness};

const FRAMES_PER_SECOND: u32 = 60;
const FIVE_MINUTE_FRAMES: u32 = FRAMES_PER_SECOND * 60 * 5; // 18 000

#[test]
fn five_minutes_of_overpressure_keeps_queue_bounded() {
    // Bead spec: "queue depth bounded (no unbounded growth)".
    // We push 4 ops/frame and only drain 1 — net +3/frame. Over
    // 18 000 frames that's 54 000 net ops trying to enter a
    // 32-cap queue. The drop path takes over once we hit cap,
    // so queue.len() must NEVER exceed cap.
    let cap = 32;
    let mut harness = SustainedBurstHarness::new(cap);
    let pushes_per_frame = 4;
    let drains_per_frame = 1;

    harness.run_burst(FIVE_MINUTE_FRAMES, pushes_per_frame, drains_per_frame);

    assert!(
        harness.queue_within_cap(),
        "queue.len() = {} exceeds cap = {} after 5min sustained burst — \
         the eviction path failed to keep up with overpressure",
        harness.queue_len(),
        cap
    );
    assert!(
        harness.queue_len() <= cap,
        "queue must remain bounded by deferred_cap"
    );
}

#[test]
fn drop_counter_increments_under_sustained_pressure() {
    // Bead spec: "drop counter increments under sustained pressure".
    let cap = 16;
    let mut harness = SustainedBurstHarness::new(cap);
    // Inflict 1000 frames at 4 push / 0 drain — every push past
    // the cap evicts one entry → telemetry.lifetime_drops ticks.
    harness.run_burst(1000, 4, 0);

    let telem = harness.telemetry();
    assert!(
        telem.lifetime_drops > 0,
        "lifetime_drops must increment when sustained pressure exceeds cap; \
         got {} drops over 1000 frames at 4 push/frame against cap {}",
        telem.lifetime_drops,
        cap
    );
    // We pushed 4000 ops; the first `cap` fit without drops; the
    // remaining (4000 - cap) all evicted one entry each.
    let expected_drops = (4 * 1000_u64) - cap as u64;
    assert_eq!(
        telem.lifetime_drops, expected_drops,
        "drop count should equal (total_pushes - cap) for zero-drain workload"
    );
    assert_eq!(
        telem.lifetime_deferrals, 4000,
        "every push records a deferral regardless of whether it later evicted"
    );
}

#[test]
fn balanced_pressure_records_zero_drops() {
    // Symmetric workload: push 2 / drain 2 per frame. Queue
    // never reaches cap → telemetry.lifetime_drops stays 0.
    let cap = 32;
    let mut harness = SustainedBurstHarness::new(cap);
    harness.run_burst(FIVE_MINUTE_FRAMES, 2, 2);

    let telem = harness.telemetry();
    assert_eq!(
        telem.lifetime_drops, 0,
        "balanced push/drain workload must not trigger any drops"
    );
    assert_eq!(
        telem.lifetime_deferrals,
        2 * FIVE_MINUTE_FRAMES as u64,
        "every push still counts as a deferral even when drained the same frame"
    );
    assert!(harness.queue_within_cap(), "queue must remain ≤ cap");
}

#[test]
fn drain_eventually_empties_after_burst_subsides() {
    // Burst: 100 frames at 4 push / 0 drain → queue saturates
    // at cap with many drops. Recovery: 100 frames at 0 push /
    // 1 drain → queue empties.
    let cap = 16;
    let mut harness = SustainedBurstHarness::new(cap);
    harness.run_burst(100, 4, 0);
    assert_eq!(harness.queue_len(), cap, "queue should saturate to cap");

    harness.run_burst(100, 0, 1);
    assert_eq!(
        harness.queue_len(),
        0,
        "queue should drain to empty when push pressure subsides"
    );
}

#[test]
fn deferrals_by_op_kind_telemetry_carries_animations_label() {
    // run_burst pushes OpKindSlug::Animations exclusively. The
    // per-op-kind histogram should carry that label.
    let mut harness = SustainedBurstHarness::new(8);
    harness.run_burst(10, 1, 0);

    let telem = harness.telemetry();
    let count = telem
        .deferrals_by_op_kind
        .get(OpKindSlug::Animations.slug())
        .copied()
        .unwrap_or(0);
    assert_eq!(
        count, 10,
        "every Animations push should appear in deferrals_by_op_kind histogram"
    );
}

#[test]
fn telemetry_queue_depth_tracks_live_state() {
    // queue_depth in telemetry is updated on every push + drain.
    let mut harness = SustainedBurstHarness::new(16);
    harness.run_burst(5, 4, 1); // 5 frames × (4 push - 1 drain) = +15 net
    assert_eq!(harness.telemetry().queue_depth, 15);
    assert_eq!(harness.queue_len(), 15);
}
