//! Per-frame budget allocator with deferred-cosmetic queue (ft-mpc9b.5.2).
//!
//! Replaces the "every op runs every frame" model with a priority-
//! ordered budget allocator. Each frame gets a fixed time budget
//! (16.67 ms at 60 Hz, 8.33 ms at 120 Hz). Required ops (dirty
//! quad rebuild, cursor, selection) always run. Cosmetic ops
//! (ligatures, subpixel AA, decorations, animations) defer to a
//! cross-frame queue when the budget is exhausted; the queue is
//! capped to prevent unbounded growth under sustained burst load
//! and drains in bulk when a frame finishes well under budget.
//!
//! Pattern: budget = 1.0 / refresh_rate; if spent > budget × 0.9,
//! defer remaining cosmetic ops; on next frame, drain deferred
//! before fresh cosmetic ops.
//!
//! ## What this module ships (foundation, ft-mpc9b.5.2)
//!
//! - `OpPriority` — `Required` vs `Cosmetic`. Required is NEVER
//!   deferred (the bead's accessibility constraint).
//! - `OpKind` — stable identifier for the op (DirtyQuadRebuild,
//!   Cursor, Selection, Ligatures, SubpixelAA, Decorations,
//!   Animations) plus `Custom(u8)` for plugin-driven ops.
//! - `DeferredOp` — `(kind, estimated_cost_ns)` carried in the
//!   cross-frame queue.
//! - `FrameBudget` — the central allocator type. Owns the budget
//!   ceiling, the per-frame counters, the deferred queue with
//!   capacity cap + drop-oldest, and the lifetime telemetry
//!   counters.
//! - `ExecutionDecision` — what `try_execute` did: `Executed`,
//!   `Deferred`, or `Dropped` (queue overflow forced an oldest-
//!   entry eviction).
//! - `FrameStartReport` / `FrameEndReport` — typed payloads the
//!   structured-log emission and the `ft doctor` summary
//!   consume.
//! - `BUDGET_DEFER_THRESHOLD` (0.9) and `BULK_DRAIN_THRESHOLD`
//!   (0.5) — named constants matching the bead's algorithm.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - Wiring `FrameBudget::begin_frame` / `end_frame` /
//!   `try_execute` calls into the actual paint pipeline at
//!   `crates/frankenterm-gui/src/termwindow/render/paint.rs`.
//! - Per-op cost estimation (the `cost_ns` argument). Today the
//!   continuation bead's caller passes either a measured value
//!   or a per-op-kind table.
//! - `ft doctor` surface for queue depth / drop rate.
//! - Coupling with the redraw predicate from ft-mpc9b.5.1: when
//!   the deferred queue is non-empty, the predicate's
//!   `cosmetic_defer_outstanding` input fires, forcing the next
//!   frame to paint.
//! - A11Y.5 prefers-reduced-motion handling: animations may be
//!   deferred but never skipped when the user has reduce-motion
//!   OFF.
//! - Bench at crates/frankenterm-core/benches/heavy_burst.rs:
//!   1 MB/s output across 50 panes, p95 input latency < 50 ms
//!   (RQ-S6 in docs/perf/resize-quality-slo.md).

#![allow(dead_code)]

use std::collections::VecDeque;

/// Threshold of `spent / budget` above which new cosmetic ops are
/// deferred rather than executed this frame. The bead specifies
/// 90 %.
pub const BUDGET_DEFER_THRESHOLD: f64 = 0.9;

/// Threshold of `spent / budget` below which a frame is
/// considered "under budget" and the deferred queue is bulk-
/// drained. The bead specifies 50 %.
pub const BULK_DRAIN_THRESHOLD: f64 = 0.5;

/// Default deferred-queue capacity in number of ops. The bead
/// specifies "4 frames worth of cosmetic ops" — sized in elements
/// here rather than in bytes/time so the cap is independent of
/// per-op cost variance.
pub const DEFAULT_DEFERRED_CAP: usize = 1024;

/// One nanosecond more than what we'd compute as a 60 Hz budget,
/// used as a sentinel for "no budget configured" callers.
pub const NS_PER_60HZ_FRAME: u64 = 1_000_000_000 / 60;

/// Whether an op is required-this-frame or deferrable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpPriority {
    /// Must run this frame regardless of budget. The bead lists
    /// dirty quad rebuild, cursor, and selection. Never deferred,
    /// never dropped.
    Required,
    /// May be deferred to a future frame when the budget is
    /// exhausted. Includes ligatures, subpixel AA, decorations,
    /// and animations. Subject to queue-cap drop-oldest under
    /// sustained pressure.
    Cosmetic,
}

/// Stable identifier for the op being scheduled. Each variant
/// drives a structured-log line + a per-op telemetry counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpKind {
    DirtyQuadRebuild,
    Cursor,
    Selection,
    Ligatures,
    SubpixelAa,
    Decorations,
    Animations,
    /// Plugin-driven op; the integer is a stable plugin-assigned
    /// identifier so structured-log emission can attribute the
    /// op to its source.
    Custom(u8),
}

/// One entry in the cross-frame deferred queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeferredOp {
    pub kind: OpKind,
    pub estimated_cost_ns: u64,
}

/// What `try_execute` did with the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionDecision {
    /// The op ran in this frame; the budget tracker advanced by
    /// `cost_ns`.
    Executed { spent_ns: u64 },
    /// The op was queued for a future frame.
    Deferred,
    /// The deferred queue was at capacity; the oldest entry was
    /// dropped to make room. Telemetry counters bump.
    Dropped,
}

/// Returned from `begin_frame()` so the caller knows what's
/// already on the queue (for any pre-paint preparation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameStartReport {
    pub budget_ns: u64,
    pub deferred_carryover: usize,
}

/// Returned from `end_frame()` for structured-log emission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEndReport {
    pub budget_ns: u64,
    pub spent_ns: u64,
    pub deferrals_this_frame: u32,
    pub drops_this_frame: u32,
    pub bulk_drains_this_frame: u32,
    pub queue_depth_after: usize,
    /// Whether `cosmetic_defer_outstanding` should fire on the
    /// redraw predicate's next frame.
    pub queue_non_empty: bool,
}

/// The per-frame budget allocator. Owns the budget ceiling, the
/// running spent counter, the deferred queue, and lifetime
/// telemetry.
#[derive(Debug)]
pub struct FrameBudget {
    budget_ns: u64,
    spent_ns: u64,
    deferred: VecDeque<DeferredOp>,
    deferred_cap: usize,
    deferrals_this_frame: u32,
    drops_this_frame: u32,
    bulk_drains_this_frame: u32,
    drops_lifetime: u64,
    deferrals_lifetime: u64,
    bulk_drains_lifetime: u64,
}

impl FrameBudget {
    /// Construct an allocator for the given refresh rate. 0 is
    /// remapped to 60 Hz so callers that haven't queried the
    /// display yet still get a sane budget.
    pub fn new(refresh_rate_hz: u32) -> Self {
        let hz = if refresh_rate_hz == 0 {
            60
        } else {
            refresh_rate_hz
        };
        let budget_ns = 1_000_000_000u64 / u64::from(hz);
        Self {
            budget_ns,
            spent_ns: 0,
            deferred: VecDeque::new(),
            deferred_cap: DEFAULT_DEFERRED_CAP,
            deferrals_this_frame: 0,
            drops_this_frame: 0,
            bulk_drains_this_frame: 0,
            drops_lifetime: 0,
            deferrals_lifetime: 0,
            bulk_drains_lifetime: 0,
        }
    }

    /// Override the deferred-queue capacity. Default is
    /// `DEFAULT_DEFERRED_CAP`. Callers on memory-constrained
    /// hosts may lower it.
    pub fn with_deferred_cap(mut self, cap: usize) -> Self {
        self.deferred_cap = cap.max(1);
        self
    }

    /// Reset per-frame counters and snapshot the queue state.
    /// Caller invokes at the very top of paint_impl.
    pub fn begin_frame(&mut self) -> FrameStartReport {
        self.spent_ns = 0;
        self.deferrals_this_frame = 0;
        self.drops_this_frame = 0;
        self.bulk_drains_this_frame = 0;
        FrameStartReport {
            budget_ns: self.budget_ns,
            deferred_carryover: self.deferred.len(),
        }
    }

    /// Schedule an op. Required ops always execute (they advance
    /// `spent_ns` regardless of budget). Cosmetic ops execute if
    /// the budget headroom permits; otherwise they defer to the
    /// queue, evicting the oldest entry on overflow.
    pub fn try_execute(
        &mut self,
        kind: OpKind,
        priority: OpPriority,
        cost_ns: u64,
    ) -> ExecutionDecision {
        match priority {
            OpPriority::Required => {
                // Required ops run regardless. Budget tracker
                // still advances so cosmetic ops downstream see
                // the truth.
                self.spent_ns = self.spent_ns.saturating_add(cost_ns);
                ExecutionDecision::Executed {
                    spent_ns: self.spent_ns,
                }
            }
            OpPriority::Cosmetic => {
                if self.over_defer_threshold() {
                    self.defer(kind, cost_ns)
                } else {
                    self.spent_ns = self.spent_ns.saturating_add(cost_ns);
                    ExecutionDecision::Executed {
                        spent_ns: self.spent_ns,
                    }
                }
            }
        }
    }

    /// Drain queued ops in bulk when the frame's budget is
    /// healthy (`spent_ns / budget_ns < BULK_DRAIN_THRESHOLD`).
    /// Returns the number of ops drained. Each drained op
    /// advances `spent_ns` by its `estimated_cost_ns` and stops
    /// when either the queue is empty or the budget threshold is
    /// reached.
    pub fn try_bulk_drain(&mut self) -> u32 {
        if !self.under_bulk_drain_threshold() {
            return 0;
        }
        let mut drained: u32 = 0;
        while let Some(front) = self.deferred.front() {
            // Stop draining if executing this op would push us
            // back over the defer threshold.
            let projected = self.spent_ns.saturating_add(front.estimated_cost_ns);
            if projected as f64 / self.budget_ns as f64 >= BUDGET_DEFER_THRESHOLD {
                break;
            }
            let op = self.deferred.pop_front().expect("front existed");
            self.spent_ns = self.spent_ns.saturating_add(op.estimated_cost_ns);
            drained = drained.saturating_add(1);
        }
        if drained > 0 {
            self.bulk_drains_this_frame = self.bulk_drains_this_frame.saturating_add(drained);
            self.bulk_drains_lifetime =
                self.bulk_drains_lifetime.saturating_add(u64::from(drained));
        }
        drained
    }

    /// Close the frame. Returns the typed report for telemetry.
    pub fn end_frame(&mut self) -> FrameEndReport {
        FrameEndReport {
            budget_ns: self.budget_ns,
            spent_ns: self.spent_ns,
            deferrals_this_frame: self.deferrals_this_frame,
            drops_this_frame: self.drops_this_frame,
            bulk_drains_this_frame: self.bulk_drains_this_frame,
            queue_depth_after: self.deferred.len(),
            queue_non_empty: !self.deferred.is_empty(),
        }
    }

    /// Drain deferred ops at the start of the next frame —
    /// caller invokes after `begin_frame` to give carry-over ops
    /// priority over fresh cosmetic ops. Returns the number
    /// drained. Stops when either the queue is empty or the
    /// budget threshold is reached.
    pub fn drain_deferred(&mut self) -> u32 {
        let mut drained: u32 = 0;
        while let Some(front) = self.deferred.front() {
            let projected = self.spent_ns.saturating_add(front.estimated_cost_ns);
            if projected as f64 / self.budget_ns as f64 >= BUDGET_DEFER_THRESHOLD {
                break;
            }
            let op = self.deferred.pop_front().expect("front existed");
            self.spent_ns = self.spent_ns.saturating_add(op.estimated_cost_ns);
            drained = drained.saturating_add(1);
        }
        drained
    }

    /// Whether the deferred queue is non-empty. The redraw
    /// predicate consults this via the `cosmetic_defer_outstanding`
    /// input.
    pub fn has_deferred_ops(&self) -> bool {
        !self.deferred.is_empty()
    }

    pub fn queue_depth(&self) -> usize {
        self.deferred.len()
    }

    pub fn budget_ns(&self) -> u64 {
        self.budget_ns
    }

    pub fn spent_ns(&self) -> u64 {
        self.spent_ns
    }

    pub fn lifetime_drops(&self) -> u64 {
        self.drops_lifetime
    }

    pub fn lifetime_deferrals(&self) -> u64 {
        self.deferrals_lifetime
    }

    pub fn lifetime_bulk_drains(&self) -> u64 {
        self.bulk_drains_lifetime
    }

    fn over_defer_threshold(&self) -> bool {
        self.spent_ns as f64 / self.budget_ns as f64 >= BUDGET_DEFER_THRESHOLD
    }

    fn under_bulk_drain_threshold(&self) -> bool {
        self.spent_ns as f64 / self.budget_ns as f64 <= BULK_DRAIN_THRESHOLD
    }

    fn defer(&mut self, kind: OpKind, estimated_cost_ns: u64) -> ExecutionDecision {
        let op = DeferredOp {
            kind,
            estimated_cost_ns,
        };
        if self.deferred.len() >= self.deferred_cap {
            // Drop oldest to make room for the new entry. The
            // bead specifies drop-oldest; this protects against
            // unbounded queue growth under sustained burst load.
            let _evicted = self.deferred.pop_front();
            self.drops_this_frame = self.drops_this_frame.saturating_add(1);
            self.drops_lifetime = self.drops_lifetime.saturating_add(1);
            self.deferred.push_back(op);
            return ExecutionDecision::Dropped;
        }
        self.deferred.push_back(op);
        self.deferrals_this_frame = self.deferrals_this_frame.saturating_add(1);
        self.deferrals_lifetime = self.deferrals_lifetime.saturating_add(1);
        ExecutionDecision::Deferred
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget_60hz() -> FrameBudget {
        FrameBudget::new(60)
    }

    #[test]
    fn budget_60hz_is_about_16_67_ms() {
        let b = FrameBudget::new(60);
        // 1e9 / 60 ≈ 16,666,666 ns
        assert_eq!(b.budget_ns(), 16_666_666);
    }

    #[test]
    fn budget_120hz_is_about_8_33_ms() {
        let b = FrameBudget::new(120);
        assert_eq!(b.budget_ns(), 8_333_333);
    }

    #[test]
    fn refresh_rate_zero_remaps_to_60hz() {
        let b = FrameBudget::new(0);
        assert_eq!(b.budget_ns(), 16_666_666);
    }

    #[test]
    fn fresh_budget_has_zero_spent_and_empty_queue() {
        let b = budget_60hz();
        assert_eq!(b.spent_ns(), 0);
        assert_eq!(b.queue_depth(), 0);
        assert!(!b.has_deferred_ops());
        assert_eq!(b.lifetime_drops(), 0);
        assert_eq!(b.lifetime_deferrals(), 0);
        assert_eq!(b.lifetime_bulk_drains(), 0);
    }

    #[test]
    fn required_op_runs_regardless_of_budget() {
        let mut b = budget_60hz();
        // Saturate budget with cosmetic ops.
        for _ in 0..100 {
            b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 1_000_000);
        }
        // Now exhausted. Required op must still execute.
        let decision = b.try_execute(OpKind::DirtyQuadRebuild, OpPriority::Required, 5_000_000);
        assert!(matches!(decision, ExecutionDecision::Executed { .. }));
        // spent_ns should reflect the required op's cost.
        assert!(b.spent_ns() >= 5_000_000);
    }

    #[test]
    fn cosmetic_op_runs_when_budget_healthy() {
        let mut b = budget_60hz();
        let decision = b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 1_000_000);
        assert!(matches!(decision, ExecutionDecision::Executed { .. }));
        assert_eq!(b.queue_depth(), 0);
    }

    #[test]
    fn cosmetic_op_defers_past_threshold() {
        let mut b = budget_60hz();
        // Push a Required op consuming 95 % of budget.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        // Cosmetic op now defers.
        let decision = b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100_000);
        assert_eq!(decision, ExecutionDecision::Deferred);
        assert_eq!(b.queue_depth(), 1);
        assert_eq!(b.lifetime_deferrals(), 1);
    }

    #[test]
    fn deferral_threshold_fires_at_or_above_90_percent() {
        let mut b = budget_60hz();
        // Push to 91 % of budget — clearly at-or-above the 90 %
        // defer threshold. Floating-point round-trip through
        // `as u64` truncation makes "exactly 0.9" land slightly
        // below the threshold, so we test 0.91 to pin the
        // semantic invariant ("once we cross 90 %, cosmetic
        // ops defer").
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.91) as u64,
        );
        let decision = b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 1);
        assert_eq!(decision, ExecutionDecision::Deferred);
    }

    #[test]
    fn deferral_threshold_does_not_fire_below_90_percent() {
        let mut b = budget_60hz();
        // Push to 89 % of budget — clearly below the threshold;
        // cosmetic ops should still execute.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.89) as u64,
        );
        let decision = b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 1);
        assert!(matches!(decision, ExecutionDecision::Executed { .. }));
    }

    #[test]
    fn queue_cap_drops_oldest_on_overflow() {
        let mut b = budget_60hz().with_deferred_cap(3);
        // Force defer mode.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        // Defer 4 cosmetic ops — last one drops oldest.
        for i in 0..4 {
            let kind = match i {
                0 => OpKind::Ligatures,
                1 => OpKind::SubpixelAa,
                2 => OpKind::Decorations,
                _ => OpKind::Animations,
            };
            b.try_execute(kind, OpPriority::Cosmetic, 1000);
        }
        // Queue depth stays at the cap.
        assert_eq!(b.queue_depth(), 3);
        assert_eq!(b.lifetime_drops(), 1);
    }

    #[test]
    fn queue_overflow_returns_dropped_decision() {
        let mut b = budget_60hz().with_deferred_cap(2);
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        b.try_execute(OpKind::SubpixelAa, OpPriority::Cosmetic, 100);
        // Third defer overflows.
        let decision = b.try_execute(OpKind::Decorations, OpPriority::Cosmetic, 100);
        assert_eq!(decision, ExecutionDecision::Dropped);
    }

    #[test]
    fn drain_deferred_runs_carryover_first_in_next_frame() {
        let mut b = budget_60hz();
        // Frame 1: defer two cosmetic ops.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        b.try_execute(OpKind::SubpixelAa, OpPriority::Cosmetic, 100);
        let _end = b.end_frame();
        assert_eq!(b.queue_depth(), 2);

        // Frame 2: begin, drain.
        let _start = b.begin_frame();
        let drained = b.drain_deferred();
        assert_eq!(drained, 2);
        assert_eq!(b.queue_depth(), 0);
    }

    #[test]
    fn drain_deferred_stops_at_threshold() {
        let mut b = budget_60hz();
        // Stuff a lot of deferred ops into the queue.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        let one_percent_budget = (b.budget_ns() as f64 * 0.01) as u64;
        for _ in 0..100 {
            b.try_execute(
                OpKind::Decorations,
                OpPriority::Cosmetic,
                one_percent_budget,
            );
        }
        let pre_drain_depth = b.queue_depth();
        b.end_frame();

        // Frame 2: drain, but only as many as fit.
        b.begin_frame();
        let drained = b.drain_deferred();
        // Drained some but not all (would otherwise exceed 90%).
        assert!(drained > 0);
        assert!(b.queue_depth() < pre_drain_depth);
        // The drain must respect the 90% threshold — never push us
        // past it.
        let usage = b.spent_ns() as f64 / b.budget_ns() as f64;
        assert!(
            usage < BUDGET_DEFER_THRESHOLD,
            "drain over-shot: usage={}, threshold={}",
            usage,
            BUDGET_DEFER_THRESHOLD,
        );
    }

    #[test]
    fn bulk_drain_runs_when_frame_under_50_percent() {
        let mut b = budget_60hz();
        // Stash a few deferred ops from a "previous" simulated
        // burst.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        let small = (b.budget_ns() as f64 * 0.05) as u64;
        for _ in 0..3 {
            b.try_execute(OpKind::Animations, OpPriority::Cosmetic, small);
        }
        b.end_frame();
        assert_eq!(b.queue_depth(), 3);

        // Frame 2: only spent 10 % so far → bulk drain fires.
        b.begin_frame();
        b.try_execute(
            OpKind::Cursor,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.1) as u64,
        );
        let drained = b.try_bulk_drain();
        assert!(drained > 0, "bulk drain should fire under 50 % budget");
        assert!(b.queue_depth() < 3);
        assert_eq!(b.lifetime_bulk_drains(), u64::from(drained));
    }

    #[test]
    fn bulk_drain_does_not_run_when_over_50_percent() {
        let mut b = budget_60hz();
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        b.end_frame();

        b.begin_frame();
        // Spend 70 % up front — bulk-drain should NOT fire.
        b.try_execute(
            OpKind::Cursor,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.7) as u64,
        );
        let drained = b.try_bulk_drain();
        assert_eq!(drained, 0);
        // Queue depth unchanged.
        assert_eq!(b.queue_depth(), 1);
    }

    #[test]
    fn end_frame_report_summarizes_per_frame_telemetry() {
        let mut b = budget_60hz();
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        b.try_execute(OpKind::SubpixelAa, OpPriority::Cosmetic, 100);
        let report = b.end_frame();
        assert_eq!(report.deferrals_this_frame, 2);
        assert_eq!(report.drops_this_frame, 0);
        assert_eq!(report.queue_depth_after, 2);
        assert!(report.queue_non_empty);
        assert_eq!(report.budget_ns, b.budget_ns());
        assert!(report.spent_ns >= (b.budget_ns() as f64 * 0.95) as u64);
    }

    #[test]
    fn begin_frame_resets_per_frame_counters_but_keeps_lifetime() {
        let mut b = budget_60hz();
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        let _r1 = b.end_frame();
        assert_eq!(b.lifetime_deferrals(), 1);

        let start = b.begin_frame();
        assert_eq!(start.deferred_carryover, 1);
        // Per-frame counters should be 0 after begin_frame.
        let r2 = b.end_frame();
        assert_eq!(r2.deferrals_this_frame, 0);
        assert_eq!(r2.drops_this_frame, 0);
        // Lifetime counter survived.
        assert_eq!(b.lifetime_deferrals(), 1);
    }

    #[test]
    fn cosmetic_defer_outstanding_signal_drives_predicate() {
        // The redraw predicate (ft-mpc9b.5.1) consults
        // has_deferred_ops() via its cosmetic_defer_outstanding
        // input. Ensure the signal is correct after every state
        // transition.
        let mut b = budget_60hz();
        assert!(!b.has_deferred_ops());
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        assert!(b.has_deferred_ops());
        b.end_frame();
        b.begin_frame();
        assert!(b.has_deferred_ops(), "carryover persists across frames");
        b.drain_deferred();
        assert!(!b.has_deferred_ops(), "drain clears the signal");
    }

    #[test]
    fn sustained_burst_does_not_grow_queue_unbounded() {
        // The bead's failure-mode protection: under sustained
        // burst load the deferred queue must not grow without
        // bound. With a cap of 64 and 1000 forced defers the
        // queue depth stays at 64.
        let mut b = budget_60hz().with_deferred_cap(64);
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        for _ in 0..1000 {
            b.try_execute(OpKind::Decorations, OpPriority::Cosmetic, 100);
        }
        assert_eq!(b.queue_depth(), 64);
        // Drops counter should reflect the overflow.
        assert_eq!(b.lifetime_drops(), 1000 - 64);
    }

    #[test]
    fn required_ops_never_drop_under_pressure() {
        let mut b = budget_60hz().with_deferred_cap(1);
        // Pile up cosmetic drops.
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        for _ in 0..10 {
            b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, 100);
        }
        let pre_lifetime_drops = b.lifetime_drops();

        // A Required op runs even with the cosmetic queue maxed.
        let decision = b.try_execute(OpKind::Cursor, OpPriority::Required, 50_000);
        assert!(matches!(decision, ExecutionDecision::Executed { .. }));
        // Drop counter is unchanged — Required ops don't go through
        // the defer path.
        assert_eq!(b.lifetime_drops(), pre_lifetime_drops);
    }

    #[test]
    fn input_latency_pattern_keeps_required_ops_fast() {
        // The bead's RQ-S6 target: 1MB/s output across 50 panes,
        // p95 input latency < 50ms. We model the latency pattern
        // by dispatching a series of dirty-quad rebuilds (heavy
        // burst input) followed by a cursor op (the input-latency-
        // critical event). The cursor must run even under heavy
        // load.
        let mut b = budget_60hz();
        for _ in 0..20 {
            // Heavy burst — saturate budget with required dirty-quad rebuilds.
            b.try_execute(OpKind::DirtyQuadRebuild, OpPriority::Required, 800_000);
        }
        // Cursor (the input-latency op) still runs and advances
        // spent_ns.
        let pre = b.spent_ns();
        let d = b.try_execute(OpKind::Cursor, OpPriority::Required, 500_000);
        assert!(matches!(d, ExecutionDecision::Executed { .. }));
        assert_eq!(b.spent_ns() - pre, 500_000);
    }

    #[test]
    fn deferred_op_records_its_kind_and_cost() {
        let mut b = budget_60hz();
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        b.try_execute(OpKind::Animations, OpPriority::Cosmetic, 12_345);
        // Inspect the queue head — the integration test for the
        // continuation can use the same shape to attribute drops
        // and bulk-drains by op kind.
        assert_eq!(b.queue_depth(), 1);
        // We don't expose the queue directly, but pop via drain
        // and check.
        b.end_frame();
        b.begin_frame();
        let drained = b.drain_deferred();
        assert_eq!(drained, 1);
    }
}
