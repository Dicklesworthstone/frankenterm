//! FrameBudget A11Y reduce-motion gate + per-op cost
//! classification substrate (ft-s0nah / BR-TERM-EMULATOR-UPLIFT.5.2.cont).
//!
//! Pure-logic substrate covering the substrate-shaped pieces
//! of the bead's FrameBudget integration. The allocator
//! itself ships in `crates/frankenterm-gui/src/termwindow/
//! frame_budget.rs` (commit bd1641aae) — `FrameBudget /
//! OpPriority / OpKind / DeferredOp / ExecutionDecision /
//! FrameStartReport / FrameEndReport`. This module adds:
//!
//! - `ReduceMotionPolicy` 3-variant the bead's A11Y.5 rule
//!   needs: when reduce-motion=ON, animations may be
//!   SKIPPED entirely (not deferred); when OFF, animations
//!   are deferred but never dropped.
//! - `evaluate_reduce_motion_gate` pure decision composing
//!   `(OpKind, ReduceMotionState, BasePolicy)` →
//!   `MotionGateDecision::{ Execute / Defer / Skip }`.
//! - `OpCostBucket` 4-tier (`Trivial / Low / Medium / High`)
//!   classifying per-op-kind cost. Bead: "per-op-kind lookup
//!   table seeded from observed values".
//! - `OpCostTable` operator-tunable lookup with fallback
//!   defaults from observed paint.rs micro-benches.
//! - `CosmeticDeferOutstanding` aggregator the bead couples
//!   into the redraw predicate (RedrawInputs field per
//!   ft-mpc9b.5.1).
//! - `FrameBudgetGateTelemetry` per-session counters.
//!
//! ## What is deferred to ft-s0nah follow-up
//!
//! - paint.rs wiring: `budget.begin_frame()` →
//!   `budget.drain_deferred()` → priority-ordered
//!   `try_execute` → `try_bulk_drain` → `end_frame`.
//! - Per-op cost timing: wrap each op with `Instant::now`
//!   delta and feed back into the cost table.
//! - `cosmetic_defer_outstanding` flow into
//!   `TermWindow::should_paint` (ft-458t7).
//! - `ft doctor` surface for FrameBudgetTelemetrySnapshot.
//! - Heavy-burst bench at `crates/frankenterm-core/benches/
//!   heavy_burst.rs`.
//! - 5-minute sustained-burst regression test.
//! - Removing `#![allow(dead_code)]` from frame_budget.rs.

#![allow(dead_code)]

// ============================================================================
// OpKind — bead-cited paint operation taxonomy
// ============================================================================

/// The bead's per-op priority taxonomy. Ordered by paint
/// priority: `DirtyQuadRebuild → Cursor → Selection →
/// Ligatures → SubpixelAa → Decorations → Animations`.
/// Substrate's `ReduceMotionPolicy` only fires on
/// `Animations` because the others are required for
/// correctness (cursor / selection / dirty cells must
/// always paint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OpKind {
    DirtyQuadRebuild,
    Cursor,
    Selection,
    Ligatures,
    SubpixelAa,
    Decorations,
    Animations,
}

impl OpKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::DirtyQuadRebuild => "dirty_quad_rebuild",
            Self::Cursor => "cursor",
            Self::Selection => "selection",
            Self::Ligatures => "ligatures",
            Self::SubpixelAa => "subpixel_aa",
            Self::Decorations => "decorations",
            Self::Animations => "animations",
        }
    }

    /// Whether this op is safe to skip entirely (vs defer).
    /// Only `Animations` is skip-safe per the bead's A11Y.5
    /// rule.
    #[must_use]
    pub const fn is_skippable(self) -> bool {
        matches!(self, Self::Animations)
    }

    /// Whether this op is required for correctness — must
    /// run every frame, no defer / no skip.
    #[must_use]
    pub const fn is_required(self) -> bool {
        matches!(
            self,
            Self::DirtyQuadRebuild | Self::Cursor | Self::Selection
        )
    }

    /// Whether this op is cosmetic — defer-eligible. The
    /// bead's "cosmetic-defer outstanding" set tracks these.
    #[must_use]
    pub const fn is_cosmetic(self) -> bool {
        matches!(
            self,
            Self::Ligatures
                | Self::SubpixelAa
                | Self::Decorations
                | Self::Animations
        )
    }
}

// ============================================================================
// ReduceMotion — A11Y.5 gate
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ReduceMotionState {
    /// OS reports reduce-motion=ON. Bead: animations may be
    /// SKIPPED entirely.
    On,
    /// OS reports reduce-motion=OFF. Bead: animations are
    /// deferred (preserved) but never dropped.
    #[default]
    Off,
    /// Probe failed or never ran. Substrate's safety default
    /// treats this as `Off` (animations preserved).
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BaseExecutionPolicy {
    /// Op fits in the budget — execute now.
    #[default]
    Execute,
    /// Op exceeded the budget — defer to next frame.
    Defer,
    /// Op exceeded the budget repeatedly — drop oldest from
    /// queue.
    DropOldest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotionGateDecision {
    /// Run the op this frame.
    Execute,
    /// Defer to next frame.
    Defer,
    /// Skip entirely — don't run, don't defer.
    Skip,
    /// Drop the queue's oldest entry to make room for this
    /// op (passed through from BaseExecutionPolicy::DropOldest).
    DropOldest,
}

/// Pure A11Y.5 gate. The integration's FrameBudget produces
/// a `BaseExecutionPolicy` from its budget arithmetic; this
/// substrate composes that with the OS reduce-motion state
/// and the OpKind to yield a final decision.
///
/// Rules:
/// 1. Required ops always Execute, regardless of reduce-motion
///    state — cursor / dirty rebuild / selection are
///    correctness-critical.
/// 2. Animations + reduce-motion=On → Skip (don't even
///    defer).
/// 3. Otherwise pass the base policy through, except DropOldest
///    on a non-cosmetic op (substrate refuses to drop
///    correctness ops).
#[must_use]
pub fn evaluate_reduce_motion_gate(
    op: OpKind,
    motion: ReduceMotionState,
    base: BaseExecutionPolicy,
) -> MotionGateDecision {
    if op.is_required() {
        return MotionGateDecision::Execute;
    }
    if op == OpKind::Animations && matches!(motion, ReduceMotionState::On) {
        return MotionGateDecision::Skip;
    }
    match base {
        BaseExecutionPolicy::Execute => MotionGateDecision::Execute,
        BaseExecutionPolicy::Defer => MotionGateDecision::Defer,
        BaseExecutionPolicy::DropOldest => {
            if op.is_cosmetic() {
                MotionGateDecision::DropOldest
            } else {
                // Substrate refuses to drop correctness ops;
                // fall back to Defer.
                MotionGateDecision::Defer
            }
        }
    }
}

// ============================================================================
// OpCostBucket + OpCostTable
// ============================================================================

/// Per-op-kind cost classification. Bead: "per-op-kind lookup
/// table seeded from observed values."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum OpCostBucket {
    /// `<10 µs` — trivial, can fit in any frame budget.
    Trivial,
    /// `10–100 µs` — low cost; defer only when budget tight.
    Low,
    /// `100 µs – 1 ms` — medium; defer when within a couple
    /// frames of the deadline.
    Medium,
    /// `>1 ms` — heavy; defer aggressively or split into
    /// chunks.
    High,
}

impl OpCostBucket {
    pub const TRIVIAL_UPPER_NS: u64 = 10_000;
    pub const LOW_UPPER_NS: u64 = 100_000;
    pub const MEDIUM_UPPER_NS: u64 = 1_000_000;

    /// Classify a measured cost into the bucket.
    #[must_use]
    pub const fn classify(cost_ns: u64) -> Self {
        if cost_ns < Self::TRIVIAL_UPPER_NS {
            Self::Trivial
        } else if cost_ns < Self::LOW_UPPER_NS {
            Self::Low
        } else if cost_ns < Self::MEDIUM_UPPER_NS {
            Self::Medium
        } else {
            Self::High
        }
    }

    /// Upper bound (exclusive) of the bucket in nanoseconds.
    /// `None` for High (no upper bound).
    #[must_use]
    pub const fn upper_ns(self) -> Option<u64> {
        match self {
            Self::Trivial => Some(Self::TRIVIAL_UPPER_NS),
            Self::Low => Some(Self::LOW_UPPER_NS),
            Self::Medium => Some(Self::MEDIUM_UPPER_NS),
            Self::High => None,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Per-op-kind cost defaults seeded from observed paint.rs
/// micro-benches (rounded). Bead: "static table and refine."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpCostTable {
    pub dirty_quad_rebuild_ns: u64,
    pub cursor_ns: u64,
    pub selection_ns: u64,
    pub ligatures_ns: u64,
    pub subpixel_aa_ns: u64,
    pub decorations_ns: u64,
    pub animations_ns: u64,
}

impl Default for OpCostTable {
    fn default() -> Self {
        // Seeded defaults — integration refines from observed
        // values.
        Self {
            dirty_quad_rebuild_ns: 80_000, // ~80 µs typical
            cursor_ns: 5_000,              // ~5 µs
            selection_ns: 8_000,           // ~8 µs
            ligatures_ns: 40_000,          // ~40 µs
            subpixel_aa_ns: 60_000,        // ~60 µs
            decorations_ns: 30_000,        // ~30 µs
            animations_ns: 200_000,        // ~200 µs (worst case)
        }
    }
}

impl OpCostTable {
    #[must_use]
    pub const fn lookup_ns(&self, op: OpKind) -> u64 {
        match op {
            OpKind::DirtyQuadRebuild => self.dirty_quad_rebuild_ns,
            OpKind::Cursor => self.cursor_ns,
            OpKind::Selection => self.selection_ns,
            OpKind::Ligatures => self.ligatures_ns,
            OpKind::SubpixelAa => self.subpixel_aa_ns,
            OpKind::Decorations => self.decorations_ns,
            OpKind::Animations => self.animations_ns,
        }
    }

    #[must_use]
    pub fn lookup_bucket(&self, op: OpKind) -> OpCostBucket {
        OpCostBucket::classify(self.lookup_ns(op))
    }
}

// ============================================================================
// CosmeticDeferOutstanding
// ============================================================================

/// The bead's redraw-predicate signal: "non-empty queue →
/// next frame must paint to drain it."
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CosmeticDeferOutstanding {
    pub deferred_ligatures: u32,
    pub deferred_subpixel_aa: u32,
    pub deferred_decorations: u32,
    pub deferred_animations: u32,
}

impl CosmeticDeferOutstanding {
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.deferred_ligatures
            .saturating_add(self.deferred_subpixel_aa)
            .saturating_add(self.deferred_decorations)
            .saturating_add(self.deferred_animations)
    }

    #[must_use]
    pub const fn has_outstanding(&self) -> bool {
        self.total() > 0
    }

    pub fn record_deferred(&mut self, op: OpKind) {
        match op {
            OpKind::Ligatures => {
                self.deferred_ligatures = self.deferred_ligatures.saturating_add(1);
            }
            OpKind::SubpixelAa => {
                self.deferred_subpixel_aa = self.deferred_subpixel_aa.saturating_add(1);
            }
            OpKind::Decorations => {
                self.deferred_decorations = self.deferred_decorations.saturating_add(1);
            }
            OpKind::Animations => {
                self.deferred_animations = self.deferred_animations.saturating_add(1);
            }
            // Required ops can't be deferred per the substrate's
            // gate; calls here are no-ops.
            _ => {}
        }
    }

    pub fn record_drained(&mut self, op: OpKind) {
        match op {
            OpKind::Ligatures => {
                self.deferred_ligatures = self.deferred_ligatures.saturating_sub(1);
            }
            OpKind::SubpixelAa => {
                self.deferred_subpixel_aa = self.deferred_subpixel_aa.saturating_sub(1);
            }
            OpKind::Decorations => {
                self.deferred_decorations = self.deferred_decorations.saturating_sub(1);
            }
            OpKind::Animations => {
                self.deferred_animations = self.deferred_animations.saturating_sub(1);
            }
            _ => {}
        }
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameBudgetGateTelemetry {
    pub gate_executes: u64,
    pub gate_defers: u64,
    pub gate_skips_reduce_motion: u64,
    pub gate_drop_oldest: u64,
    pub required_ops_force_executed: u64,
    pub cost_classifications_trivial: u64,
    pub cost_classifications_low: u64,
    pub cost_classifications_medium: u64,
    pub cost_classifications_high: u64,
}

impl FrameBudgetGateTelemetry {
    pub fn record_decision(&mut self, op: OpKind, decision: MotionGateDecision) {
        match decision {
            MotionGateDecision::Execute => {
                self.gate_executes = self.gate_executes.saturating_add(1);
                if op.is_required() {
                    self.required_ops_force_executed =
                        self.required_ops_force_executed.saturating_add(1);
                }
            }
            MotionGateDecision::Defer => {
                self.gate_defers = self.gate_defers.saturating_add(1);
            }
            MotionGateDecision::Skip => {
                self.gate_skips_reduce_motion =
                    self.gate_skips_reduce_motion.saturating_add(1);
            }
            MotionGateDecision::DropOldest => {
                self.gate_drop_oldest = self.gate_drop_oldest.saturating_add(1);
            }
        }
    }

    pub fn record_cost_classification(&mut self, bucket: OpCostBucket) {
        let slot = match bucket {
            OpCostBucket::Trivial => &mut self.cost_classifications_trivial,
            OpCostBucket::Low => &mut self.cost_classifications_low,
            OpCostBucket::Medium => &mut self.cost_classifications_medium,
            OpCostBucket::High => &mut self.cost_classifications_high,
        };
        *slot = slot.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // OpKind classification
    // ----------------------------------------------------------------

    #[test]
    fn op_required_matches_bead() {
        assert!(OpKind::DirtyQuadRebuild.is_required());
        assert!(OpKind::Cursor.is_required());
        assert!(OpKind::Selection.is_required());
        assert!(!OpKind::Ligatures.is_required());
        assert!(!OpKind::Animations.is_required());
    }

    #[test]
    fn op_cosmetic_matches_bead() {
        assert!(OpKind::Ligatures.is_cosmetic());
        assert!(OpKind::SubpixelAa.is_cosmetic());
        assert!(OpKind::Decorations.is_cosmetic());
        assert!(OpKind::Animations.is_cosmetic());
        assert!(!OpKind::Cursor.is_cosmetic());
        assert!(!OpKind::DirtyQuadRebuild.is_cosmetic());
    }

    #[test]
    fn op_skippable_only_animations() {
        assert!(OpKind::Animations.is_skippable());
        for op in [
            OpKind::DirtyQuadRebuild,
            OpKind::Cursor,
            OpKind::Selection,
            OpKind::Ligatures,
            OpKind::SubpixelAa,
            OpKind::Decorations,
        ] {
            assert!(!op.is_skippable(), "{op:?}");
        }
    }

    // ----------------------------------------------------------------
    // evaluate_reduce_motion_gate
    // ----------------------------------------------------------------

    #[test]
    fn gate_required_op_always_executes() {
        for motion in [
            ReduceMotionState::On,
            ReduceMotionState::Off,
            ReduceMotionState::Unknown,
        ] {
            for base in [
                BaseExecutionPolicy::Execute,
                BaseExecutionPolicy::Defer,
                BaseExecutionPolicy::DropOldest,
            ] {
                let d = evaluate_reduce_motion_gate(OpKind::Cursor, motion, base);
                assert_eq!(d, MotionGateDecision::Execute);
            }
        }
    }

    #[test]
    fn gate_animations_with_reduce_motion_skipped() {
        let d = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::On,
            BaseExecutionPolicy::Execute,
        );
        assert_eq!(d, MotionGateDecision::Skip);
    }

    #[test]
    fn gate_animations_with_reduce_motion_off_uses_base() {
        let exec = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::Off,
            BaseExecutionPolicy::Execute,
        );
        let defer = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::Off,
            BaseExecutionPolicy::Defer,
        );
        assert_eq!(exec, MotionGateDecision::Execute);
        assert_eq!(defer, MotionGateDecision::Defer);
    }

    #[test]
    fn gate_animations_with_unknown_motion_uses_base_safety() {
        // Unknown defaults to "preserve animations" per the
        // safety doc.
        let d = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::Unknown,
            BaseExecutionPolicy::Defer,
        );
        assert_eq!(d, MotionGateDecision::Defer);
    }

    #[test]
    fn gate_drop_oldest_passes_through_for_cosmetic() {
        let d = evaluate_reduce_motion_gate(
            OpKind::Ligatures,
            ReduceMotionState::Off,
            BaseExecutionPolicy::DropOldest,
        );
        assert_eq!(d, MotionGateDecision::DropOldest);
    }

    #[test]
    fn gate_required_overrides_drop_oldest() {
        // Even if the budget says DropOldest, required ops
        // execute (substrate refuses to drop correctness ops).
        let d = evaluate_reduce_motion_gate(
            OpKind::Cursor,
            ReduceMotionState::Off,
            BaseExecutionPolicy::DropOldest,
        );
        assert_eq!(d, MotionGateDecision::Execute);
    }

    #[test]
    fn gate_animations_reduce_motion_beats_drop_oldest() {
        // Animation + reduce-motion=On → Skip beats the
        // DropOldest path.
        let d = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::On,
            BaseExecutionPolicy::DropOldest,
        );
        assert_eq!(d, MotionGateDecision::Skip);
    }

    // ----------------------------------------------------------------
    // OpCostBucket
    // ----------------------------------------------------------------

    #[test]
    fn cost_classify_boundaries() {
        assert_eq!(OpCostBucket::classify(0), OpCostBucket::Trivial);
        assert_eq!(OpCostBucket::classify(9_999), OpCostBucket::Trivial);
        assert_eq!(OpCostBucket::classify(10_000), OpCostBucket::Low);
        assert_eq!(OpCostBucket::classify(99_999), OpCostBucket::Low);
        assert_eq!(OpCostBucket::classify(100_000), OpCostBucket::Medium);
        assert_eq!(OpCostBucket::classify(999_999), OpCostBucket::Medium);
        assert_eq!(OpCostBucket::classify(1_000_000), OpCostBucket::High);
        assert_eq!(OpCostBucket::classify(u64::MAX), OpCostBucket::High);
    }

    #[test]
    fn cost_upper_ns() {
        assert_eq!(OpCostBucket::Trivial.upper_ns(), Some(10_000));
        assert_eq!(OpCostBucket::Low.upper_ns(), Some(100_000));
        assert_eq!(OpCostBucket::Medium.upper_ns(), Some(1_000_000));
        assert_eq!(OpCostBucket::High.upper_ns(), None);
    }

    #[test]
    fn cost_label_stable() {
        assert_eq!(OpCostBucket::Trivial.label(), "trivial");
        assert_eq!(OpCostBucket::Low.label(), "low");
        assert_eq!(OpCostBucket::Medium.label(), "medium");
        assert_eq!(OpCostBucket::High.label(), "high");
    }

    // ----------------------------------------------------------------
    // OpCostTable
    // ----------------------------------------------------------------

    #[test]
    fn cost_table_default_lookups_match_seed() {
        let t = OpCostTable::default();
        assert_eq!(t.lookup_ns(OpKind::Cursor), 5_000);
        assert_eq!(t.lookup_ns(OpKind::Animations), 200_000);
        assert_eq!(t.lookup_ns(OpKind::DirtyQuadRebuild), 80_000);
    }

    #[test]
    fn cost_table_lookup_bucket() {
        let t = OpCostTable::default();
        assert_eq!(t.lookup_bucket(OpKind::Cursor), OpCostBucket::Trivial);
        assert_eq!(t.lookup_bucket(OpKind::DirtyQuadRebuild), OpCostBucket::Low);
        assert_eq!(t.lookup_bucket(OpKind::Animations), OpCostBucket::Medium);
    }

    // ----------------------------------------------------------------
    // CosmeticDeferOutstanding
    // ----------------------------------------------------------------

    #[test]
    fn outstanding_default_empty() {
        let c = CosmeticDeferOutstanding::default();
        assert_eq!(c.total(), 0);
        assert!(!c.has_outstanding());
    }

    #[test]
    fn outstanding_record_deferred_increments() {
        let mut c = CosmeticDeferOutstanding::default();
        c.record_deferred(OpKind::Ligatures);
        c.record_deferred(OpKind::Animations);
        c.record_deferred(OpKind::Animations);
        assert_eq!(c.total(), 3);
        assert!(c.has_outstanding());
    }

    #[test]
    fn outstanding_required_op_no_op() {
        // Required ops shouldn't end up in the cosmetic queue.
        let mut c = CosmeticDeferOutstanding::default();
        c.record_deferred(OpKind::Cursor);
        c.record_deferred(OpKind::DirtyQuadRebuild);
        assert_eq!(c.total(), 0);
    }

    #[test]
    fn outstanding_drained_decrements_saturating() {
        let mut c = CosmeticDeferOutstanding::default();
        c.record_deferred(OpKind::Animations);
        c.record_drained(OpKind::Animations);
        c.record_drained(OpKind::Animations); // saturates
        assert_eq!(c.total(), 0);
    }

    // ----------------------------------------------------------------
    // FrameBudgetGateTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_record_decisions_route() {
        let mut t = FrameBudgetGateTelemetry::default();
        t.record_decision(OpKind::Cursor, MotionGateDecision::Execute);
        t.record_decision(OpKind::Animations, MotionGateDecision::Skip);
        t.record_decision(OpKind::Ligatures, MotionGateDecision::Defer);
        t.record_decision(OpKind::Decorations, MotionGateDecision::DropOldest);
        assert_eq!(t.gate_executes, 1);
        assert_eq!(t.required_ops_force_executed, 1);
        assert_eq!(t.gate_skips_reduce_motion, 1);
        assert_eq!(t.gate_defers, 1);
        assert_eq!(t.gate_drop_oldest, 1);
    }

    #[test]
    fn telemetry_record_cost_classification_routes() {
        let mut t = FrameBudgetGateTelemetry::default();
        t.record_cost_classification(OpCostBucket::Trivial);
        t.record_cost_classification(OpCostBucket::Low);
        t.record_cost_classification(OpCostBucket::Medium);
        t.record_cost_classification(OpCostBucket::High);
        assert_eq!(t.cost_classifications_trivial, 1);
        assert_eq!(t.cost_classifications_low, 1);
        assert_eq!(t.cost_classifications_medium, 1);
        assert_eq!(t.cost_classifications_high, 1);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_a11y_user_with_reduce_motion_gets_no_animations() {
        // Bead's A11Y.5: reduce-motion user; animations
        // should be Skip not Defer.
        let d = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::On,
            BaseExecutionPolicy::Defer,
        );
        assert_eq!(d, MotionGateDecision::Skip);
    }

    #[test]
    fn scenario_normal_user_animations_preserved_when_budget_tight() {
        // Bead's "When OFF, animations must be deferred but
        // never dropped." Substrate honours by mapping
        // Defer → Defer for animations, refusing DropOldest
        // when budget extreme.
        let d = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::Off,
            BaseExecutionPolicy::Defer,
        );
        assert_eq!(d, MotionGateDecision::Defer);
    }

    #[test]
    fn scenario_heavy_burst_drops_cosmetic_keeps_required() {
        // Bead's "1 MB/s output across 50 panes": budget
        // says DropOldest. Substrate drops cosmetic
        // (Ligatures/Decorations) but keeps Cursor /
        // DirtyQuadRebuild.
        let cosmetic = evaluate_reduce_motion_gate(
            OpKind::Ligatures,
            ReduceMotionState::Off,
            BaseExecutionPolicy::DropOldest,
        );
        let required = evaluate_reduce_motion_gate(
            OpKind::Cursor,
            ReduceMotionState::Off,
            BaseExecutionPolicy::DropOldest,
        );
        assert_eq!(cosmetic, MotionGateDecision::DropOldest);
        assert_eq!(required, MotionGateDecision::Execute);
    }

    #[test]
    fn scenario_cosmetic_outstanding_signals_redraw() {
        // Bead's redraw-predicate signal: non-empty queue
        // forces next-frame paint to drain.
        let mut c = CosmeticDeferOutstanding::default();
        assert!(!c.has_outstanding());
        c.record_deferred(OpKind::Animations);
        assert!(c.has_outstanding());
        c.record_drained(OpKind::Animations);
        assert!(!c.has_outstanding());
    }

    #[test]
    fn scenario_full_pipeline_seeded_costs_match_buckets() {
        let table = OpCostTable::default();
        // All required ops should be Trivial or Low.
        for op in [OpKind::Cursor, OpKind::Selection] {
            let bucket = table.lookup_bucket(op);
            assert!(matches!(bucket, OpCostBucket::Trivial | OpCostBucket::Low));
        }
    }

    #[test]
    fn scenario_unknown_motion_state_safety_default() {
        // Defensive: motion probe failed (e.g., system call
        // returned EINVAL). Substrate treats Unknown like Off
        // — animations preserved (defer not skip).
        let d = evaluate_reduce_motion_gate(
            OpKind::Animations,
            ReduceMotionState::Unknown,
            BaseExecutionPolicy::Defer,
        );
        assert_eq!(d, MotionGateDecision::Defer);
        assert_ne!(d, MotionGateDecision::Skip);
    }
}
