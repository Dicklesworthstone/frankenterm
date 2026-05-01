//! FrameBudget → redraw-predicate signal coupling
//! ([BR-TERM-EMULATOR-UPLIFT.5.2.cont] / `ft-s0nah`,
//! foundation slice).
//!
//! The `FrameBudget` allocator lives in
//! `frankenterm-gui/src/termwindow/frame_budget.rs`
//! (substrate `bd1641aae`). This module ships **the pure-
//! logic adapter substrate** the bead's continuation work
//! depends on, without touching `paint.rs`:
//!
//! 1. **Cosmetic-defer signal contract** (sub-task 3) —
//!    [`CosmeticDeferSignal`] structurally encodes
//!    `frame_budget.has_deferred_ops()` for consumption by
//!    the redraw predicate's `RedrawInputs` struct. Pure
//!    data; the GUI integration projects the gui-crate
//!    `FrameBudget` into one of these.
//! 2. **Reduce-motion policy** (sub-task 5) —
//!    [`AnimationDeferPolicy`] encodes the bead's stated
//!    rule: "When the OS reports `reduce-motion=ON`,
//!    animations may be SKIPPED entirely (not deferred).
//!    When OFF, animations must be deferred (preserved)
//!    but never dropped from the queue." Pure decision
//!    tree.
//! 3. **Telemetry projection contract** (sub-task 4) —
//!    [`FrameBudgetTelemetrySnapshot`] mirrors the bead's
//!    "queue_depth, lifetime_drops, lifetime_deferrals,
//!    lifetime_bulk_drains, last_spent_ns, last_budget_ns"
//!    indicators. Doctor-friendly, serde-clean, lives in
//!    core so the doctor surface (in core) can read it.
//! 4. **Sustained-burst harness** (sub-task 7) —
//!    [`SustainedBurstHarness`] is a pure-state-machine
//!    harness that simulates 5 minutes of forced cosmetic
//!    deferrals and asserts queue depth stays bounded +
//!    drop counter increments correctly.
//!
//! ## What this module is NOT
//!
//! - Not the FrameBudget allocator. That lives in the gui
//!   crate (would require gui-crate compilation, which is
//!   notoriously contested under concurrent agents).
//! - Not paint.rs wiring. The integration follow-on edits
//!   `paint.rs` and feeds these contracts.
//! - Not the heavy-burst bench. Sub-task 6 is integration.
//! - Not the structured-log writer. Sub-task 1 emits
//!   structured-log lines from `paint_impl`; this module
//!   ships the emit-shape contract.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Op-kind taxonomy (mirror of gui-crate FrameBudget's OpKind)
// ============================================================================

/// Per-op-kind contract for callers that don't link the
/// gui crate. Names mirror `frankenterm-gui` `OpKind` 1:1
/// so the projection from gui-crate to core-crate is
/// straightforward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKindSlug {
    DirtyQuadRebuild,
    Cursor,
    Selection,
    Ligatures,
    SubpixelAa,
    Decorations,
    Animations,
    Plugin,
}

impl OpKindSlug {
    /// Stable telemetry slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::DirtyQuadRebuild => "dirty_quad_rebuild",
            Self::Cursor => "cursor",
            Self::Selection => "selection",
            Self::Ligatures => "ligatures",
            Self::SubpixelAa => "subpixel_aa",
            Self::Decorations => "decorations",
            Self::Animations => "animations",
            Self::Plugin => "plugin",
        }
    }

    #[must_use]
    pub const fn all() -> [Self; 8] {
        [
            Self::DirtyQuadRebuild,
            Self::Cursor,
            Self::Selection,
            Self::Ligatures,
            Self::SubpixelAa,
            Self::Decorations,
            Self::Animations,
            Self::Plugin,
        ]
    }
}

/// `OpPriority` mirror — same 4 levels as the gui-crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpPrioritySlug {
    Required,
    User,
    Cosmetic,
    Plugin,
}

// ============================================================================
// Cosmetic-defer signal (sub-task 3)
// ============================================================================

/// Pure-data view of `FrameBudget.has_deferred_ops()` for
/// the redraw predicate to consume. Bead sub-task 3:
///
/// > When `TermWindow::should_paint()` lands (`ft-458t7`),
/// > gather `frame_budget.has_deferred_ops()` into the
/// > `cosmetic_defer_outstanding` `RedrawInputs` field.
/// > Non-empty queue → next frame must paint to drain it.
///
/// This shape is what the gui integration projects;
/// `redraw_predicate` consumes it via the
/// `cosmetic_defer_outstanding: bool` field in
/// `RedrawInputs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CosmeticDeferSignal {
    /// `true` iff the FrameBudget has any deferred ops
    /// queued at frame-start time.
    pub deferred_ops_outstanding: bool,
    /// Snapshot of `queue_depth()` at signal-time —
    /// observability only, not a gating field.
    pub queue_depth: u32,
}

impl CosmeticDeferSignal {
    /// True iff the redraw predicate must force paint to
    /// drain the queue.
    #[must_use]
    pub fn must_paint_to_drain(self) -> bool {
        self.deferred_ops_outstanding
    }
}

// ============================================================================
// Reduce-motion policy (sub-task 5)
// ============================================================================

/// OS reduce-motion preference state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReduceMotionPreference {
    /// User has explicitly turned reduce-motion ON.
    On,
    /// User has explicitly turned reduce-motion OFF (or
    /// the OS does not support the preference).
    Off,
}

/// Decision the policy emits for a given (op_kind,
/// reduce-motion) pair under budget pressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationDeferDecision {
    /// Op runs this frame.
    Execute,
    /// Op is deferred to the deferred-op queue (preserved,
    /// drained on the next frame). Default for `Animations`
    /// when reduce-motion=OFF.
    Defer,
    /// Op is dropped entirely (not enqueued). Default for
    /// `Animations` when reduce-motion=ON — the user has
    /// asked for reduced motion, so we don't even buffer
    /// them.
    Skip,
}

/// Pure-logic decision tree per the bead's reduce-motion
/// rule.
///
/// Inputs:
/// - `op_kind`: which op the budget is considering.
/// - `would_exceed_budget`: the budget allocator's
///   verdict — `true` means the op cannot run this frame.
/// - `reduce_motion`: the OS preference state.
///
/// Output: [`AnimationDeferDecision`]. The bead names
/// these rules:
///
/// 1. **Non-Animations op + budget OK** → `Execute`.
/// 2. **Non-Animations op + budget over** → `Defer`
///    (preserve in queue).
/// 3. **Animations + reduce-motion=ON + budget OK** →
///    `Execute` (the user can still see one frame of
///    motion; the OS preference only kicks in under
///    pressure per the bead's "may be SKIPPED").
/// 4. **Animations + reduce-motion=ON + budget over** →
///    `Skip` (don't even queue).
/// 5. **Animations + reduce-motion=OFF + budget OK** →
///    `Execute`.
/// 6. **Animations + reduce-motion=OFF + budget over** →
///    `Defer` (preserve in queue, drain next frame).
#[must_use]
pub fn decide_animation_defer(
    op_kind: OpKindSlug,
    would_exceed_budget: bool,
    reduce_motion: ReduceMotionPreference,
) -> AnimationDeferDecision {
    if !would_exceed_budget {
        return AnimationDeferDecision::Execute;
    }
    match op_kind {
        OpKindSlug::Animations => match reduce_motion {
            ReduceMotionPreference::On => AnimationDeferDecision::Skip,
            ReduceMotionPreference::Off => AnimationDeferDecision::Defer,
        },
        _ => AnimationDeferDecision::Defer,
    }
}

// ============================================================================
// Telemetry projection (sub-task 4)
// ============================================================================

/// Doctor-friendly snapshot of `FrameBudget` state.
/// Mirrors the bead's "`FrameBudgetTelemetrySnapshot {
/// queue_depth, lifetime_drops, lifetime_deferrals,
/// lifetime_bulk_drains, last_spent_ns, last_budget_ns }`"
/// requirement.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FrameBudgetTelemetrySnapshot {
    pub queue_depth: u32,
    pub lifetime_drops: u64,
    pub lifetime_deferrals: u64,
    pub lifetime_bulk_drains: u64,
    pub last_spent_ns: u64,
    pub last_budget_ns: u64,
    /// Per-op-kind counter for the structured-log line
    /// per the bead's "Per-frame: ts, kind, priority,
    /// cost_ns, decision" requirement.
    pub deferrals_by_op_kind: BTreeMap<String, u64>,
    pub drops_by_op_kind: BTreeMap<String, u64>,
}

impl FrameBudgetTelemetrySnapshot {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    /// True iff queue isn't backlogged (depth ≤ 64) AND
    /// drop rate per deferral is ≤ 5%.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        const QUEUE_DEPTH_HEALTHY: u32 = 64;
        if self.queue_depth > QUEUE_DEPTH_HEALTHY {
            return false;
        }
        let total = self.lifetime_drops + self.lifetime_deferrals;
        if total == 0 {
            return true;
        }
        let drop_ratio = self.lifetime_drops as f64 / total as f64;
        drop_ratio <= 0.05
    }

    /// Record a deferral into the per-kind counter.
    pub fn record_deferral(&mut self, op_kind: OpKindSlug) {
        self.lifetime_deferrals = self.lifetime_deferrals.saturating_add(1);
        *self
            .deferrals_by_op_kind
            .entry(op_kind.slug().to_string())
            .or_insert(0) += 1;
    }

    /// Record a drop into the per-kind counter.
    pub fn record_drop(&mut self, op_kind: OpKindSlug) {
        self.lifetime_drops = self.lifetime_drops.saturating_add(1);
        *self
            .drops_by_op_kind
            .entry(op_kind.slug().to_string())
            .or_insert(0) += 1;
    }
}

// ============================================================================
// Sustained-burst harness (sub-task 7)
// ============================================================================

/// Pure-state-machine harness simulating a 5-minute burst
/// of forced cosmetic deferrals. Asserts:
///
/// - Queue depth never exceeds `deferred_cap`.
/// - Drop counter increments as expected (every push past
///   the cap evicts the oldest entry → 1 drop).
/// - Telemetry shape matches bead's per-op-kind histogram.
///
/// Bead sub-task 7: "5 minutes of forced cosmetic
/// deferrals; assert queue depth stays bounded (never
/// exceeds deferred_cap); assert drop counter increments
/// as expected."
#[derive(Debug, Clone)]
pub struct SustainedBurstHarness {
    pub deferred_cap: usize,
    pub queue: Vec<OpKindSlug>,
    pub telemetry: FrameBudgetTelemetrySnapshot,
}

impl SustainedBurstHarness {
    #[must_use]
    pub fn new(deferred_cap: usize) -> Self {
        Self {
            deferred_cap,
            queue: Vec::with_capacity(deferred_cap),
            telemetry: FrameBudgetTelemetrySnapshot::baseline(),
        }
    }

    /// Push one deferred op. Drops the oldest if at cap.
    pub fn push(&mut self, op_kind: OpKindSlug) {
        if self.queue.len() >= self.deferred_cap {
            // Evict oldest; that's a drop.
            let dropped = self.queue.remove(0);
            self.telemetry.record_drop(dropped);
        }
        self.queue.push(op_kind);
        self.telemetry.record_deferral(op_kind);
        self.telemetry.queue_depth = self.queue.len() as u32;
    }

    /// Drain N ops from the front of the queue.
    pub fn drain(&mut self, n: usize) {
        let drain_count = n.min(self.queue.len());
        self.queue.drain(0..drain_count);
        self.telemetry.queue_depth = self.queue.len() as u32;
    }

    /// Run a burst: push `pushes_per_frame` ops per frame
    /// for `frames` frames, drain `drains_per_frame` per
    /// frame.
    pub fn run_burst(
        &mut self,
        frames: u32,
        pushes_per_frame: u32,
        drains_per_frame: u32,
    ) {
        for _ in 0..frames {
            for _ in 0..pushes_per_frame {
                self.push(OpKindSlug::Animations);
            }
            self.drain(drains_per_frame as usize);
        }
    }

    /// Invariant: queue depth never exceeds cap.
    #[must_use]
    pub fn queue_within_cap(&self) -> bool {
        self.queue.len() <= self.deferred_cap
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // OpKindSlug
    // ------------------------------------------------------------------------

    #[test]
    fn every_op_kind_has_distinct_slug() {
        let slugs: Vec<&str> = OpKindSlug::all().iter().map(|k| k.slug()).collect();
        let unique: std::collections::HashSet<_> = slugs.iter().collect();
        assert_eq!(unique.len(), 8);
    }

    #[test]
    fn op_kind_slugs_match_gui_crate_field_names() {
        // Pin the slug strings against the gui-crate
        // OpKind variant names (snake-case) so the projection
        // never silently drifts.
        assert_eq!(OpKindSlug::DirtyQuadRebuild.slug(), "dirty_quad_rebuild");
        assert_eq!(OpKindSlug::Cursor.slug(), "cursor");
        assert_eq!(OpKindSlug::Selection.slug(), "selection");
        assert_eq!(OpKindSlug::Ligatures.slug(), "ligatures");
        assert_eq!(OpKindSlug::SubpixelAa.slug(), "subpixel_aa");
        assert_eq!(OpKindSlug::Decorations.slug(), "decorations");
        assert_eq!(OpKindSlug::Animations.slug(), "animations");
        assert_eq!(OpKindSlug::Plugin.slug(), "plugin");
    }

    // ------------------------------------------------------------------------
    // Cosmetic-defer signal
    // ------------------------------------------------------------------------

    #[test]
    fn empty_signal_does_not_force_paint() {
        let s = CosmeticDeferSignal {
            deferred_ops_outstanding: false,
            queue_depth: 0,
        };
        assert!(!s.must_paint_to_drain());
    }

    #[test]
    fn nonempty_signal_forces_paint() {
        let s = CosmeticDeferSignal {
            deferred_ops_outstanding: true,
            queue_depth: 5,
        };
        assert!(s.must_paint_to_drain());
    }

    // ------------------------------------------------------------------------
    // Reduce-motion policy
    // ------------------------------------------------------------------------

    #[test]
    fn under_budget_always_executes_regardless_of_motion() {
        for op in OpKindSlug::all() {
            for motion in [
                ReduceMotionPreference::On,
                ReduceMotionPreference::Off,
            ] {
                assert_eq!(
                    decide_animation_defer(op, false, motion),
                    AnimationDeferDecision::Execute,
                    "op={op:?} motion={motion:?}"
                );
            }
        }
    }

    #[test]
    fn over_budget_non_animations_defers_regardless_of_motion() {
        for op in [
            OpKindSlug::DirtyQuadRebuild,
            OpKindSlug::Cursor,
            OpKindSlug::Selection,
            OpKindSlug::Ligatures,
            OpKindSlug::SubpixelAa,
            OpKindSlug::Decorations,
            OpKindSlug::Plugin,
        ] {
            for motion in [
                ReduceMotionPreference::On,
                ReduceMotionPreference::Off,
            ] {
                assert_eq!(
                    decide_animation_defer(op, true, motion),
                    AnimationDeferDecision::Defer,
                    "op={op:?} motion={motion:?}"
                );
            }
        }
    }

    #[test]
    fn over_budget_animations_with_reduce_motion_on_skips() {
        assert_eq!(
            decide_animation_defer(
                OpKindSlug::Animations,
                true,
                ReduceMotionPreference::On
            ),
            AnimationDeferDecision::Skip
        );
    }

    #[test]
    fn over_budget_animations_with_reduce_motion_off_defers() {
        assert_eq!(
            decide_animation_defer(
                OpKindSlug::Animations,
                true,
                ReduceMotionPreference::Off
            ),
            AnimationDeferDecision::Defer
        );
    }

    // ------------------------------------------------------------------------
    // Telemetry snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn baseline_snapshot_is_safe() {
        assert!(FrameBudgetTelemetrySnapshot::baseline().is_safe());
    }

    #[test]
    fn snapshot_unsafe_when_queue_depth_exceeds_64() {
        let mut s = FrameBudgetTelemetrySnapshot::baseline();
        s.queue_depth = 65;
        assert!(!s.is_safe());
    }

    #[test]
    fn snapshot_unsafe_when_drop_rate_exceeds_5pct() {
        let mut s = FrameBudgetTelemetrySnapshot::baseline();
        s.lifetime_deferrals = 9;
        s.lifetime_drops = 1; // 10% drop rate
        assert!(!s.is_safe());
    }

    #[test]
    fn snapshot_safe_at_5pct_drop_rate_boundary() {
        let mut s = FrameBudgetTelemetrySnapshot::baseline();
        s.lifetime_deferrals = 95;
        s.lifetime_drops = 5; // exactly 5% → safe
        assert!(s.is_safe());
    }

    #[test]
    fn record_deferral_increments_per_kind_counter() {
        let mut s = FrameBudgetTelemetrySnapshot::baseline();
        s.record_deferral(OpKindSlug::Cursor);
        s.record_deferral(OpKindSlug::Cursor);
        s.record_deferral(OpKindSlug::Animations);
        assert_eq!(s.lifetime_deferrals, 3);
        assert_eq!(s.deferrals_by_op_kind.get("cursor"), Some(&2));
        assert_eq!(s.deferrals_by_op_kind.get("animations"), Some(&1));
    }

    #[test]
    fn record_drop_increments_per_kind_counter() {
        let mut s = FrameBudgetTelemetrySnapshot::baseline();
        s.record_drop(OpKindSlug::Plugin);
        assert_eq!(s.lifetime_drops, 1);
        assert_eq!(s.drops_by_op_kind.get("plugin"), Some(&1));
    }

    // ------------------------------------------------------------------------
    // Sustained-burst harness
    // ------------------------------------------------------------------------

    #[test]
    fn harness_within_cap_no_drops() {
        let mut h = SustainedBurstHarness::new(64);
        for _ in 0..32 {
            h.push(OpKindSlug::Animations);
        }
        assert!(h.queue_within_cap());
        assert_eq!(h.telemetry.lifetime_drops, 0);
        assert_eq!(h.telemetry.lifetime_deferrals, 32);
    }

    #[test]
    fn harness_at_cap_drops_oldest() {
        let mut h = SustainedBurstHarness::new(4);
        for _ in 0..6 {
            h.push(OpKindSlug::Animations);
        }
        assert!(h.queue_within_cap());
        assert_eq!(h.queue.len(), 4);
        assert_eq!(h.telemetry.lifetime_drops, 2);
        assert_eq!(h.telemetry.lifetime_deferrals, 6);
    }

    #[test]
    fn harness_drain_reduces_queue() {
        let mut h = SustainedBurstHarness::new(64);
        for _ in 0..10 {
            h.push(OpKindSlug::Cursor);
        }
        h.drain(4);
        assert_eq!(h.queue.len(), 6);
        assert_eq!(h.telemetry.queue_depth, 6);
    }

    #[test]
    fn five_minute_burst_at_60hz_stays_bounded() {
        // Bead sub-task 7: "5 minutes of forced cosmetic
        // deferrals; assert queue depth stays bounded
        // (never exceeds deferred_cap)."
        // 5 min @ 60 Hz = 18,000 frames.
        // Push 2 per frame, drain 1 per frame → queue
        // grows by 1/frame, capped at deferred_cap=64.
        let mut h = SustainedBurstHarness::new(64);
        h.run_burst(18_000, 2, 1);
        // Bead invariant: queue depth never exceeds cap.
        assert!(h.queue_within_cap());
        // Saturated after ~64 frames, then steady state:
        // each frame pushes 2 (one evicts oldest = 1
        // drop), then drains 1 → ends at cap-1 = 63.
        assert!(h.telemetry.lifetime_drops > 0);
        assert!(
            h.queue.len() <= h.deferred_cap,
            "queue {} exceeds cap {}",
            h.queue.len(),
            h.deferred_cap
        );
        assert!(h.queue.len() >= h.deferred_cap - 1);
    }

    #[test]
    fn burst_drain_matches_push_no_drops() {
        // Push and drain at equal rate → no drops.
        let mut h = SustainedBurstHarness::new(64);
        h.run_burst(1_000, 1, 1);
        assert_eq!(h.telemetry.lifetime_drops, 0);
        assert_eq!(h.queue.len(), 0);
    }

    // ------------------------------------------------------------------------
    // Headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn animations_skipped_under_pressure_with_reduce_motion_on() {
        // Bead sub-task 5 headline: a user with
        // reduce-motion=ON should never see animation
        // backlogs build up under load.
        let mut h = SustainedBurstHarness::new(8);

        for _ in 0..100 {
            // Simulate frame-budget pressure.
            let decision = decide_animation_defer(
                OpKindSlug::Animations,
                true,
                ReduceMotionPreference::On,
            );
            match decision {
                AnimationDeferDecision::Skip => {
                    // Don't push; just record the skip.
                    h.telemetry.record_drop(OpKindSlug::Animations);
                }
                AnimationDeferDecision::Defer => {
                    h.push(OpKindSlug::Animations);
                }
                AnimationDeferDecision::Execute => {}
            }
        }

        // Reduce-motion=ON → all pressured Animations
        // skipped, queue stays empty.
        assert_eq!(h.queue.len(), 0);
        assert_eq!(h.telemetry.lifetime_deferrals, 0);
        assert_eq!(h.telemetry.lifetime_drops, 100);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip() {
        let mut s = FrameBudgetTelemetrySnapshot::baseline();
        s.record_deferral(OpKindSlug::Cursor);
        s.record_drop(OpKindSlug::Animations);
        s.queue_depth = 5;
        s.last_spent_ns = 12_000_000;
        s.last_budget_ns = 16_666_666;
        let json = serde_json::to_string(&s).unwrap();
        let parsed: FrameBudgetTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }
}
