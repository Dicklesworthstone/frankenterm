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
//!   `Deferred`, or `Dropped` with the oldest evicted queue entry.
//! - `FrameStartReport` / `FrameEndReport` — typed payloads the
//!   structured-log emission and the `ft doctor` summary
//!   consume.
//! - `BUDGET_DEFER_THRESHOLD` (0.9) and `BULK_DRAIN_THRESHOLD`
//!   (0.5) — named constants matching the bead's algorithm.
//!
//! ## Continuation Surface
//!
//! - Wiring `FrameBudget::begin_frame` / `end_frame` /
//!   `try_execute` calls is owned by the paint pipeline at
//!   `crates/frankenterm-gui/src/termwindow/render/paint.rs`.
//! - Per-op cost estimation is driven by the caller's seeded
//!   per-op-kind table or measured values.
//! - `ft doctor` consumes the queue depth / drop rate counters.
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

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use frankenterm_core::frame_budget_a11y_gate::{OpCostBucket, OpCostTable};

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
    /// dropped to make room. Telemetry counters bump and the
    /// evicted entry is returned so outstanding-work accounting
    /// can reconcile the queue.
    Dropped { evicted: DeferredOp },
}

/// Result of a measured FrameBudget execution attempt. Deferred
/// and dropped ops do not run their closure, so they carry no
/// output or observed cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredExecution<T> {
    pub decision: ExecutionDecision,
    pub output: Option<T>,
    pub measured_cost_ns: Option<u64>,
    pub cost_bucket: Option<OpCostBucket>,
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

/// Adaptive per-op estimate table for paint-site FrameBudget
/// calls. The core `OpCostTable` provides seeded defaults; this
/// GUI-side table records observed costs and smooths them with a
/// cheap EWMA so the next frame budgets from live renderer data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBudgetCostFeedback {
    table: OpCostTable,
    custom_estimates_ns: HashMap<u8, u64>,
}

impl Default for FrameBudgetCostFeedback {
    fn default() -> Self {
        Self {
            table: OpCostTable::default(),
            custom_estimates_ns: HashMap::new(),
        }
    }
}

impl FrameBudgetCostFeedback {
    pub const CUSTOM_DEFAULT_NS: u64 = 100_000;
    const EWMA_OLD_WEIGHT: u128 = 7;
    const EWMA_TOTAL_WEIGHT: u128 = 8;

    #[must_use]
    pub fn estimate_ns(&self, kind: OpKind) -> u64 {
        match kind {
            OpKind::DirtyQuadRebuild => self.table.dirty_quad_rebuild_ns,
            OpKind::Cursor => self.table.cursor_ns,
            OpKind::Selection => self.table.selection_ns,
            OpKind::Ligatures => self.table.ligatures_ns,
            OpKind::SubpixelAa => self.table.subpixel_aa_ns,
            OpKind::Decorations => self.table.decorations_ns,
            OpKind::Animations => self.table.animations_ns,
            OpKind::Custom(id) => self
                .custom_estimates_ns
                .get(&id)
                .copied()
                .unwrap_or(Self::CUSTOM_DEFAULT_NS),
        }
    }

    pub fn record_observed_cost(&mut self, kind: OpKind, observed_ns: u64) -> OpCostBucket {
        let prior = self.estimate_ns(kind);
        let updated = ewma_cost_ns(prior, observed_ns);
        match kind {
            OpKind::DirtyQuadRebuild => self.table.dirty_quad_rebuild_ns = updated,
            OpKind::Cursor => self.table.cursor_ns = updated,
            OpKind::Selection => self.table.selection_ns = updated,
            OpKind::Ligatures => self.table.ligatures_ns = updated,
            OpKind::SubpixelAa => self.table.subpixel_aa_ns = updated,
            OpKind::Decorations => self.table.decorations_ns = updated,
            OpKind::Animations => self.table.animations_ns = updated,
            OpKind::Custom(id) => {
                self.custom_estimates_ns.insert(id, updated);
            }
        }
        OpCostBucket::classify(updated)
    }

    #[must_use]
    pub fn table(&self) -> &OpCostTable {
        &self.table
    }
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

    /// Schedule an op using the current adaptive estimate, run it
    /// only if the budget decision executes, then feed the measured
    /// duration back into the estimate table for future frames.
    pub fn try_execute_measured<T>(
        &mut self,
        feedback: &mut FrameBudgetCostFeedback,
        kind: OpKind,
        priority: OpPriority,
        op: impl FnOnce() -> T,
    ) -> MeasuredExecution<T> {
        let estimated_ns = feedback.estimate_ns(kind);
        let decision = self.try_execute(kind, priority, estimated_ns);
        if !matches!(decision, ExecutionDecision::Executed { .. }) {
            return MeasuredExecution {
                decision,
                output: None,
                measured_cost_ns: None,
                cost_bucket: None,
            };
        }

        let start = Instant::now();
        let output = op();
        let measured_ns = elapsed_ns_u64(start);
        self.replace_last_estimated_cost(estimated_ns, measured_ns);
        let cost_bucket = feedback.record_observed_cost(kind, measured_ns);

        MeasuredExecution {
            decision: ExecutionDecision::Executed {
                spent_ns: self.spent_ns,
            },
            output: Some(output),
            measured_cost_ns: Some(measured_ns),
            cost_bucket: Some(cost_bucket),
        }
    }

    /// Drain queued ops in bulk when the frame's budget is
    /// healthy (`spent_ns / budget_ns < BULK_DRAIN_THRESHOLD`).
    /// Returns the drained ops. Each drained op
    /// advances `spent_ns` by its `estimated_cost_ns` and stops
    /// when either the queue is empty or the budget threshold is
    /// reached.
    pub fn try_bulk_drain_ops(&mut self) -> Vec<DeferredOp> {
        if !self.under_bulk_drain_threshold() {
            return Vec::new();
        }
        let mut drained_ops = Vec::new();
        while let Some(front) = self.deferred.front() {
            // Stop draining if executing this op would push us
            // back over the defer threshold.
            let projected = self.spent_ns.saturating_add(front.estimated_cost_ns);
            if projected as f64 / self.budget_ns as f64 >= BUDGET_DEFER_THRESHOLD {
                break;
            }
            let op = self.deferred.pop_front().expect("front existed");
            self.spent_ns = self.spent_ns.saturating_add(op.estimated_cost_ns);
            drained_ops.push(op);
        }
        let drained = drained_ops.len().min(u32::MAX as usize) as u32;
        if drained > 0 {
            self.bulk_drains_this_frame = self.bulk_drains_this_frame.saturating_add(drained);
            self.bulk_drains_lifetime =
                self.bulk_drains_lifetime.saturating_add(u64::from(drained));
        }
        drained_ops
    }

    pub fn try_bulk_drain(&mut self) -> u32 {
        self.try_bulk_drain_ops().len().min(u32::MAX as usize) as u32
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
    /// priority over fresh cosmetic ops. Returns the drained ops.
    /// Stops when either the queue is empty or the
    /// budget threshold is reached.
    pub fn drain_deferred_ops(&mut self) -> Vec<DeferredOp> {
        let mut drained_ops = Vec::new();
        while let Some(front) = self.deferred.front() {
            let projected = self.spent_ns.saturating_add(front.estimated_cost_ns);
            if projected as f64 / self.budget_ns as f64 >= BUDGET_DEFER_THRESHOLD {
                break;
            }
            let op = self.deferred.pop_front().expect("front existed");
            self.spent_ns = self.spent_ns.saturating_add(op.estimated_cost_ns);
            drained_ops.push(op);
        }
        drained_ops
    }

    pub fn drain_deferred(&mut self) -> u32 {
        self.drain_deferred_ops().len().min(u32::MAX as usize) as u32
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

    /// Whether the current frame is already past the cosmetic
    /// defer threshold. Integration code uses this to compose the
    /// A11Y reduce-motion gate before mutating the deferred queue.
    #[must_use]
    pub fn would_defer_cosmetic_now(&self) -> bool {
        self.over_defer_threshold()
    }

    /// Whether the next deferred op would evict the oldest queue
    /// entry. Used by the reduce-motion gate bridge to translate
    /// the frame-budget state into the substrate's base policy
    /// without queueing an op that may be skipped.
    #[must_use]
    pub fn deferred_queue_is_at_capacity(&self) -> bool {
        self.deferred.len() >= self.deferred_cap
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

    fn replace_last_estimated_cost(&mut self, estimated_ns: u64, measured_ns: u64) {
        if measured_ns >= estimated_ns {
            self.spent_ns = self
                .spent_ns
                .saturating_add(measured_ns.saturating_sub(estimated_ns));
        } else {
            self.spent_ns = self
                .spent_ns
                .saturating_sub(estimated_ns.saturating_sub(measured_ns));
        }
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
            let evicted = self
                .deferred
                .pop_front()
                .expect("deferred queue at capacity");
            self.drops_this_frame = self.drops_this_frame.saturating_add(1);
            self.drops_lifetime = self.drops_lifetime.saturating_add(1);
            self.deferred.push_back(op);
            return ExecutionDecision::Dropped { evicted };
        }
        self.deferred.push_back(op);
        self.deferrals_this_frame = self.deferrals_this_frame.saturating_add(1);
        self.deferrals_lifetime = self.deferrals_lifetime.saturating_add(1);
        ExecutionDecision::Deferred
    }
}

fn ewma_cost_ns(prior_ns: u64, observed_ns: u64) -> u64 {
    let numerator = u128::from(prior_ns)
        .saturating_mul(FrameBudgetCostFeedback::EWMA_OLD_WEIGHT)
        .saturating_add(u128::from(observed_ns));
    let updated = numerator / FrameBudgetCostFeedback::EWMA_TOTAL_WEIGHT;
    updated.min(u128::from(u64::MAX)) as u64
}

fn elapsed_ns_u64(start: Instant) -> u64 {
    start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
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
    fn cost_feedback_defaults_match_seed_table() {
        let feedback = FrameBudgetCostFeedback::default();
        assert_eq!(feedback.estimate_ns(OpKind::Cursor), 5_000);
        assert_eq!(feedback.estimate_ns(OpKind::Animations), 200_000);
        assert_eq!(
            feedback.estimate_ns(OpKind::Custom(9)),
            FrameBudgetCostFeedback::CUSTOM_DEFAULT_NS
        );
        assert_eq!(feedback.table().cursor_ns, 5_000);
    }

    #[test]
    fn cost_feedback_smooths_observed_costs() {
        let mut feedback = FrameBudgetCostFeedback::default();
        let bucket = feedback.record_observed_cost(OpKind::Cursor, 85_000);

        // Cursor default is 5_000 ns. EWMA with 7/8 old + 1/8 new:
        // ((5_000 * 7) + 85_000) / 8 = 15_000.
        assert_eq!(feedback.estimate_ns(OpKind::Cursor), 15_000);
        assert_eq!(bucket, OpCostBucket::Low);
    }

    #[test]
    fn cost_feedback_keeps_custom_op_estimates_independent() {
        let mut feedback = FrameBudgetCostFeedback::default();
        feedback.record_observed_cost(OpKind::Custom(1), 900_000);

        assert!(
            feedback.estimate_ns(OpKind::Custom(1)) > FrameBudgetCostFeedback::CUSTOM_DEFAULT_NS
        );
        assert_eq!(
            feedback.estimate_ns(OpKind::Custom(2)),
            FrameBudgetCostFeedback::CUSTOM_DEFAULT_NS
        );
    }

    #[test]
    fn measured_execution_runs_closure_and_replaces_estimate_with_measured_spend() {
        let mut b = budget_60hz();
        let mut feedback = FrameBudgetCostFeedback::default();
        let measured =
            b.try_execute_measured(&mut feedback, OpKind::Cursor, OpPriority::Required, || {
                42_u8
            });

        assert!(matches!(
            measured.decision,
            ExecutionDecision::Executed { .. }
        ));
        assert_eq!(measured.output, Some(42));
        assert_eq!(b.spent_ns(), measured.measured_cost_ns.unwrap());
        assert!(measured.cost_bucket.is_some());
    }

    #[test]
    fn measured_execution_defers_without_running_closure() {
        let mut b = budget_60hz();
        let mut feedback = FrameBudgetCostFeedback::default();
        let mut ran = false;
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );

        let measured = b.try_execute_measured(
            &mut feedback,
            OpKind::Ligatures,
            OpPriority::Cosmetic,
            || {
                ran = true;
                99_u8
            },
        );

        assert_eq!(measured.decision, ExecutionDecision::Deferred);
        assert_eq!(measured.output, None);
        assert_eq!(measured.measured_cost_ns, None);
        assert!(!ran);
        assert_eq!(b.queue_depth(), 1);
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
        assert!(
            matches!(
                decision,
                ExecutionDecision::Dropped {
                    evicted: DeferredOp {
                        kind: OpKind::Ligatures,
                        ..
                    }
                }
            ),
            "overflow must report the oldest evicted op; got {decision:?}",
        );
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
        let drained = b.drain_deferred_ops();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, OpKind::Animations);
        assert_eq!(drained[0].estimated_cost_ns, 12_345);
    }

    #[test]
    fn bulk_drain_ops_returns_drained_kinds_for_telemetry() {
        let mut b = budget_60hz();
        b.try_execute(
            OpKind::DirtyQuadRebuild,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.95) as u64,
        );
        let small = (b.budget_ns() as f64 * 0.05) as u64;
        b.try_execute(OpKind::Ligatures, OpPriority::Cosmetic, small);
        b.try_execute(OpKind::Decorations, OpPriority::Cosmetic, small);
        b.end_frame();

        b.begin_frame();
        b.try_execute(
            OpKind::Cursor,
            OpPriority::Required,
            (b.budget_ns() as f64 * 0.1) as u64,
        );
        let drained = b.try_bulk_drain_ops();
        assert_eq!(
            drained.iter().map(|op| op.kind).collect::<Vec<_>>(),
            vec![OpKind::Ligatures, OpKind::Decorations],
        );
        assert_eq!(b.queue_depth(), 0);
    }
}
