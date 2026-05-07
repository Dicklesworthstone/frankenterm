//! `should_paint()` predicate telemetry contract
//! ([BR-TERM-EMULATOR-UPLIFT.5.1.cont] / `ft-458t7`).
//!
//! The parent bead (`ft-mpc9b.5.1`, shipped at `52b8238bd`)
//! landed the **foundation predicate**:
//! `crates/frankenterm-gui/src/termwindow/render/redraw_predicate.rs`
//! ships `RedrawReason`, `RedrawInputs`, `RedrawDecision`,
//! `RedrawDecisionStats`, and `evaluate(&RedrawInputs) ->
//! RedrawDecision` — pure-logic conversion from the gathered
//! signals to a paint-or-skip verdict. The predicate's
//! headline acceptance: ≥99% idle skip rate and ≥40% typing-
//! cadence skip rate.
//!
//! This continuation does the **production wiring**: the
//! `TermWindow::should_paint()` method that gathers
//! `RedrawInputs` from the live state, the
//! `paint.rs:38` short-circuit, the per-platform OS-paint-
//! signal honor list, and the bench. Production wiring
//! requires the GPU + per-platform Window stack; this commit
//! ships the contract layer the integration consumes:
//!
//! - [`RedrawDecisionHealth`] — `ft doctor` snapshot
//!   surfacing `paints_total`, `skips_total`,
//!   `idle_skip_rate`, plus per-reason counters. Same
//!   `*Health` shape as this session's other fixtures.
//! - [`OsPaintSignalSource`] — per-platform OS-paint-request
//!   sources (macOS `setNeedsDisplay`, Wayland frame-
//!   callback, X11 `ConfigureNotify`) the wiring is
//!   forbidden to drop.
//! - [`OsPaintLatch`] — the latch each OS source sets; the
//!   should_paint predicate honors a set latch.
//! - [`IdlePaintSkipBenchScenario`] + corpus — the bead's
//!   "10s idle at 60Hz on a 12-pane fleet, ≥99% skip rate"
//!   acceptance test.
//! - [`ForcePaintSignal`] — drag-resize, BEL /
//!   AT-update-pending / cosmetic-defer-outstanding signals
//!   that MUST force a paint regardless of other inputs.
//!
//! ## Headline rule
//!
//! > **≥99% idle skip rate** under a 10s idle session at 60Hz
//! > on a 12-pane fleet. RQ-S5/RQ-S8 in
//! > `docs/perf/resize-quality-slo.md`. Encoded as
//! > `RedrawDecisionHealth::meets_idle_skip_rq()`.
//!
//! Plus three force-paint paths the predicate MUST honor:
//!
//! 1. **OS-paint request** — `setNeedsDisplay` (macOS),
//!    frame-callback (Wayland), `ConfigureNotify` (X11). The
//!    OS knows when an external repaint is needed; ignoring
//!    it leaves stale pixels on screen.
//! 2. **BEL / AT-update-pending** — accessibility tree
//!    pending updates and BEL alerts must paint to keep
//!    assistive tech in sync.
//! 3. **Cosmetic-defer outstanding** (cross-link to
//!    `ft-mpc9b.5.2` frame-pacing budget allocator) — when
//!    the cosmetic-defer queue has pending work and the
//!    budget allows.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// OS-paint signal sources
// ============================================================================

/// Per-platform OS-paint-request source. Each source latches
/// `OsPaintLatch::Pending` when the OS asks for a repaint;
/// the predicate consumes the latch and clears it on the
/// next paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsPaintSignalSource {
    /// macOS — `[NSView setNeedsDisplay:YES]`. The OS calls
    /// this when window damage requires redraw (e.g.,
    /// expose, resize, theme change).
    MacosSetNeedsDisplay,
    /// Wayland — frame-callback fire. The compositor signals
    /// readiness for the next frame; cross-links to
    /// `ft-mpc9b.3.2` / `ft-28opz`.
    WaylandFrameCallback,
    /// X11 — `ConfigureNotify` event. The X server signals
    /// window geometry change.
    X11ConfigureNotify,
    /// Test / synthetic source. Not used in production.
    Synthetic,
}

impl OsPaintSignalSource {
    /// Stable slug for serialization.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::MacosSetNeedsDisplay => "macos_set_needs_display",
            Self::WaylandFrameCallback => "wayland_frame_callback",
            Self::X11ConfigureNotify => "x11_configure_notify",
            Self::Synthetic => "synthetic",
        }
    }

    /// All variants in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::MacosSetNeedsDisplay,
        Self::WaylandFrameCallback,
        Self::X11ConfigureNotify,
        Self::Synthetic,
    ];
}

/// Latch state for one OS source. The integration calls
/// `OsPaintLatch::request()` from the OS-event handler; the
/// predicate calls `consume()` after honoring the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsPaintLatch {
    /// No pending OS paint request.
    Clear,
    /// OS requested a paint; predicate must paint.
    Pending,
}

impl OsPaintLatch {
    #[must_use]
    pub const fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }
}

// ============================================================================
// Force-paint signals
// ============================================================================

/// Signals that MUST force a paint regardless of other
/// inputs. The predicate's `evaluate()` consumes these as
/// part of `RedrawInputs`; this enum names them so the
/// integration's signal gathering is a closed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForcePaintSignal {
    /// Interactive drag-resize has produced a frame that must
    /// be presented even if the idle predicate would
    /// otherwise skip.
    DragResize,
    /// `Alert::Bell` from the term layer — the screen flash
    /// or visual bell needs to render.
    Bel,
    /// Accessibility tree has a pending update.
    AtUpdatePending,
    /// Cosmetic-defer queue has outstanding work and the
    /// frame-pacing budget allows. Cross-links to
    /// `ft-mpc9b.5.2` (frame-pacing allocator).
    CosmeticDeferOutstanding,
}

impl ForcePaintSignal {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::DragResize => "drag_resize",
            Self::Bel => "bel",
            Self::AtUpdatePending => "at_update_pending",
            Self::CosmeticDeferOutstanding => "cosmetic_defer_outstanding",
        }
    }

    pub const ALL: &'static [Self] = &[
        Self::DragResize,
        Self::Bel,
        Self::AtUpdatePending,
        Self::CosmeticDeferOutstanding,
    ];
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the should_paint predicate.
/// Mirrors the `*Health` shape used across this session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedrawDecisionHealth {
    /// Total predicate evaluations.
    pub evaluations_total: u64,
    /// Total `Paint` verdicts.
    pub paints_total: u64,
    /// Total `Skip` verdicts.
    pub skips_total: u64,
    /// Per-`RedrawReason`-slug count of paint reasons. The
    /// integration projects the `RedrawReason` enum's slugs
    /// into this map.
    pub paint_reasons: BTreeMap<String, u64>,
    /// Force-paint signal counters (per
    /// `ForcePaintSignal::slug()`).
    pub force_paint_counters: BTreeMap<String, u64>,
    /// Per-OS source paint-latch consumption count.
    pub os_paint_consumptions: BTreeMap<String, u64>,
}

impl RedrawDecisionHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            evaluations_total: 0,
            paints_total: 0,
            skips_total: 0,
            paint_reasons: BTreeMap::new(),
            force_paint_counters: BTreeMap::new(),
            os_paint_consumptions: BTreeMap::new(),
        }
    }

    /// Lifetime skip-rate ratio. Returns 1.0 when no
    /// evaluations yet (vacuously perfect).
    #[must_use]
    pub fn skip_rate(&self) -> f64 {
        if self.evaluations_total == 0 {
            return 1.0;
        }
        self.skips_total as f64 / self.evaluations_total as f64
    }

    /// Lifetime skip-rate as a percentage (0.0..=100.0).
    #[must_use]
    pub fn skip_rate_pct(&self) -> f64 {
        self.skip_rate() * 100.0
    }

    /// True iff the lifetime skip rate clears the bead's
    /// idle-skip RQ-S5 acceptance bound (≥99%).
    #[must_use]
    pub fn meets_idle_skip_rq(&self) -> bool {
        self.skip_rate_pct() >= 99.0
    }

    /// True iff the lifetime skip rate clears the typing-
    /// cadence RQ (≥40%, since typing burns frames).
    #[must_use]
    pub fn meets_typing_cadence_rq(&self) -> bool {
        self.skip_rate_pct() >= 40.0
    }

    /// Predicate-snapshot is "safe" when:
    ///
    /// 1. **Vacuous** — no predicate evaluations recorded AND
    ///    no force-paint / OS-paint signals observed; or
    /// 2. The lifetime idle skip rate clears the
    ///    `meets_idle_skip_rq` bound.
    ///
    /// Per ft-yxrez (interpretation A): paint signals firing
    /// without a predicate evaluation is a contract violation
    /// — the predicate is supposed to gate every paint, so
    /// `force_paint_counters` / `os_paint_consumptions`
    /// growing while `evaluations_total == 0` indicates a
    /// paint slipped past the predicate. Such a state must
    /// surface as un-safe so the harness's safety check
    /// catches the integration bug.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        if self.evaluations_total == 0 {
            // Vacuous-safe path requires the observability
            // counters to ALSO be empty. Otherwise paints
            // happened uncoordinated and the predicate's
            // gating contract was violated.
            return self.force_paint_counters.is_empty() && self.os_paint_consumptions.is_empty();
        }
        self.meets_idle_skip_rq()
    }
}

/// Outcome of one predicate evaluation. Mirrors the parent
/// bead's `RedrawDecision` shape. The contract layer uses
/// stable slug strings rather than the enum so we don't pull
/// in the GUI-side type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionRecord {
    Paint { reason_slugs: Vec<String> },
    Skip,
}

impl DecisionRecord {
    #[must_use]
    pub fn is_paint(&self) -> bool {
        matches!(self, Self::Paint { .. })
    }
}

/// Fold one decision into a health snapshot.
pub fn fold_decision(health: &mut RedrawDecisionHealth, decision: &DecisionRecord) {
    health.evaluations_total = health.evaluations_total.saturating_add(1);
    match decision {
        DecisionRecord::Paint { reason_slugs } => {
            health.paints_total = health.paints_total.saturating_add(1);
            for slug in reason_slugs {
                *health.paint_reasons.entry(slug.clone()).or_insert(0) += 1;
            }
        }
        DecisionRecord::Skip => {
            health.skips_total = health.skips_total.saturating_add(1);
        }
    }
}

/// Record one OS-paint-source consumption. The integration
/// calls this each time it consumes a pending latch.
pub fn record_os_paint_consumption(health: &mut RedrawDecisionHealth, source: OsPaintSignalSource) {
    let counter = health
        .os_paint_consumptions
        .entry(source.slug().to_string())
        .or_insert(0);
    *counter = (*counter).saturating_add(1);
}

/// Record one force-paint signal firing.
pub fn record_force_paint(health: &mut RedrawDecisionHealth, signal: ForcePaintSignal) {
    let counter = health
        .force_paint_counters
        .entry(signal.slug().to_string())
        .or_insert(0);
    *counter = (*counter).saturating_add(1);
}

// ============================================================================
// Bench corpus
// ============================================================================

/// Bench scenarios for the predicate. The bead lists one;
/// this enum extends to two complementary scenarios so the
/// harness can prove BOTH the idle-skip and typing-cadence
/// targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdlePaintSkipBenchScenario {
    /// 10s idle at 60Hz on a 12-pane fleet. Acceptance:
    /// `skip_rate_pct >= 99.0`.
    Idle10s12PaneFleet,
    /// Typing cadence: 1 char/sec input, mostly idle frames.
    /// Acceptance: `skip_rate_pct >= 40.0` (typing burns
    /// frames; the bound is far below idle's 99%).
    TypingCadence1Hz,
    /// Force-paint stress: every frame has BEL or
    /// AT-update-pending or OS request. Acceptance: every
    /// evaluation produces `Paint` (no spurious skips).
    ForcePaintEveryFrame,
}

impl IdlePaintSkipBenchScenario {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Idle10s12PaneFleet => "idle_paint_skip_10s_12pane",
            Self::TypingCadence1Hz => "typing_cadence_1hz",
            Self::ForcePaintEveryFrame => "force_paint_every_frame",
        }
    }

    #[must_use]
    pub const fn slo_id(self) -> &'static str {
        match self {
            Self::Idle10s12PaneFleet => "RQ-S5",
            Self::TypingCadence1Hz => "RQ-S8",
            Self::ForcePaintEveryFrame => "RQ-S5",
        }
    }

    #[must_use]
    pub const fn acceptance(self) -> IdlePaintSkipAcceptance {
        match self {
            Self::Idle10s12PaneFleet => IdlePaintSkipAcceptance::SkipRatePctMin { min: 99.0 },
            Self::TypingCadence1Hz => IdlePaintSkipAcceptance::SkipRatePctMin { min: 40.0 },
            Self::ForcePaintEveryFrame => IdlePaintSkipAcceptance::EveryEvaluationPaints,
        }
    }

    pub const ALL: &'static [Self] = &[
        Self::Idle10s12PaneFleet,
        Self::TypingCadence1Hz,
        Self::ForcePaintEveryFrame,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdlePaintSkipAcceptance {
    SkipRatePctMin { min: f64 },
    EveryEvaluationPaints,
}

#[must_use]
pub fn bench_scenario_corpus() -> Vec<IdlePaintSkipBenchScenario> {
    IdlePaintSkipBenchScenario::ALL.to_vec()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdlePaintSkipBenchResult {
    pub scenario: IdlePaintSkipBenchScenario,
    pub final_health: RedrawDecisionHealth,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub notes: Option<String>,
}

impl IdlePaintSkipBenchResult {
    #[must_use]
    pub fn evaluate(
        scenario: IdlePaintSkipBenchScenario,
        final_health: RedrawDecisionHealth,
    ) -> Self {
        let passed = match scenario.acceptance() {
            IdlePaintSkipAcceptance::SkipRatePctMin { min } => final_health.skip_rate_pct() >= min,
            IdlePaintSkipAcceptance::EveryEvaluationPaints => {
                // Every evaluation produced Paint = no skips.
                final_health.skips_total == 0 && final_health.evaluations_total > 0
            }
        };
        Self {
            scenario,
            final_health,
            passed,
            notes: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdlePaintSkipBenchSnapshot {
    pub schema_version: u32,
    pub bead: String,
    pub results: Vec<IdlePaintSkipBenchResult>,
}

impl IdlePaintSkipBenchSnapshot {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: 1,
            bead: "ft-458t7".to_string(),
            results: Vec::new(),
        }
    }

    pub fn record(&mut self, result: IdlePaintSkipBenchResult) {
        if let Some(existing) = self
            .results
            .iter_mut()
            .find(|r| r.scenario == result.scenario)
        {
            *existing = result;
        } else {
            self.results.push(result);
        }
    }

    #[must_use]
    pub fn all_pass(&self) -> bool {
        IdlePaintSkipBenchScenario::ALL
            .iter()
            .all(|s| self.results.iter().any(|r| r.scenario == *s && r.passed))
    }
}

impl Default for IdlePaintSkipBenchSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // OS-paint sources
    // ------------------------------------------------------------------------

    #[test]
    fn all_os_sources_have_distinct_slugs() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for s in OsPaintSignalSource::ALL {
            assert!(seen.insert(s.slug()), "dup {}", s.slug());
        }
    }

    #[test]
    fn os_paint_latch_pending_predicate() {
        assert!(OsPaintLatch::Pending.is_pending());
        assert!(!OsPaintLatch::Clear.is_pending());
    }

    #[test]
    fn force_paint_signals_distinct() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for s in ForcePaintSignal::ALL {
            assert!(seen.insert(s.slug()), "dup {}", s.slug());
        }
    }

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn baseline_is_safe_and_vacuously_perfect() {
        let h = RedrawDecisionHealth::baseline();
        assert!(h.is_safe());
        assert_eq!(h.skip_rate(), 1.0);
        assert_eq!(h.skip_rate_pct(), 100.0);
    }

    // ----------------------------------------------------------------
    // Regression: ft-yxrez interpretation A — paint signals firing
    // without predicate evaluations is a contract violation. is_safe
    // must reject this state instead of vacuously passing.
    // ----------------------------------------------------------------

    #[test]
    fn force_paint_without_predicate_evaluation_is_unsafe() {
        let mut h = RedrawDecisionHealth::baseline();
        record_force_paint(&mut h, ForcePaintSignal::DragResize);
        assert_eq!(h.evaluations_total, 0);
        assert!(!h.force_paint_counters.is_empty());
        assert!(
            !h.is_safe(),
            "force_paint firing while evaluations_total == 0 must surface as unsafe \
             (predicate gating contract violated)",
        );
    }

    #[test]
    fn os_paint_consumption_without_predicate_evaluation_is_unsafe() {
        let mut h = RedrawDecisionHealth::baseline();
        record_os_paint_consumption(&mut h, OsPaintSignalSource::WaylandFrameCallback);
        assert_eq!(h.evaluations_total, 0);
        assert!(!h.os_paint_consumptions.is_empty());
        assert!(
            !h.is_safe(),
            "OS paint signal firing while evaluations_total == 0 must surface as unsafe \
             (predicate gating contract violated)",
        );
    }

    #[test]
    fn vacuous_baseline_is_still_safe_when_all_counters_empty() {
        // Sanity-check that the strict vacuous-safe path
        // still passes when nothing has fired at all.
        let h = RedrawDecisionHealth::baseline();
        assert_eq!(h.evaluations_total, 0);
        assert!(h.force_paint_counters.is_empty());
        assert!(h.os_paint_consumptions.is_empty());
        assert!(h.is_safe());
    }

    #[test]
    fn evaluations_recorded_predicate_path_uses_idle_skip_rq() {
        // Once evaluations_total > 0, the observability
        // counters are no longer required to be empty —
        // is_safe routes to meets_idle_skip_rq.
        let mut h = RedrawDecisionHealth {
            evaluations_total: 100,
            paints_total: 1,
            skips_total: 99,
            ..RedrawDecisionHealth::baseline()
        };
        // Add a force-paint counter — should NOT affect is_safe
        // because evaluations_total > 0.
        record_force_paint(&mut h, ForcePaintSignal::DragResize);
        assert!(h.meets_idle_skip_rq());
        assert!(h.is_safe());
    }

    #[test]
    fn skip_rate_at_99_pct_meets_idle_rq() {
        let h = RedrawDecisionHealth {
            evaluations_total: 100,
            paints_total: 1,
            skips_total: 99,
            ..RedrawDecisionHealth::baseline()
        };
        assert!(h.meets_idle_skip_rq());
        assert!(h.meets_typing_cadence_rq());
        assert!(h.is_safe());
    }

    #[test]
    fn skip_rate_below_99_fails_idle_rq() {
        let h = RedrawDecisionHealth {
            evaluations_total: 100,
            paints_total: 5,
            skips_total: 95,
            ..RedrawDecisionHealth::baseline()
        };
        assert!(!h.meets_idle_skip_rq());
        // Still meets typing cadence.
        assert!(h.meets_typing_cadence_rq());
    }

    #[test]
    fn skip_rate_below_40_fails_both() {
        let h = RedrawDecisionHealth {
            evaluations_total: 100,
            paints_total: 70,
            skips_total: 30,
            ..RedrawDecisionHealth::baseline()
        };
        assert!(!h.meets_idle_skip_rq());
        assert!(!h.meets_typing_cadence_rq());
    }

    // ------------------------------------------------------------------------
    // fold_decision
    // ------------------------------------------------------------------------

    #[test]
    fn fold_paint_increments_counters_and_reasons() {
        let mut h = RedrawDecisionHealth::baseline();
        fold_decision(
            &mut h,
            &DecisionRecord::Paint {
                reason_slugs: vec!["dirty_lines".to_string(), "cursor_blink".to_string()],
            },
        );
        assert_eq!(h.evaluations_total, 1);
        assert_eq!(h.paints_total, 1);
        assert_eq!(h.skips_total, 0);
        assert_eq!(h.paint_reasons.get("dirty_lines"), Some(&1));
        assert_eq!(h.paint_reasons.get("cursor_blink"), Some(&1));
    }

    #[test]
    fn fold_skip_increments_skips() {
        let mut h = RedrawDecisionHealth::baseline();
        fold_decision(&mut h, &DecisionRecord::Skip);
        assert_eq!(h.skips_total, 1);
        assert_eq!(h.paints_total, 0);
    }

    #[test]
    fn fold_decision_is_paint_predicate() {
        let p = DecisionRecord::Paint {
            reason_slugs: vec!["x".to_string()],
        };
        assert!(p.is_paint());
        let s = DecisionRecord::Skip;
        assert!(!s.is_paint());
    }

    // ------------------------------------------------------------------------
    // OS-paint consumption + force-paint counters
    // ------------------------------------------------------------------------

    #[test]
    fn record_os_paint_consumption_increments_counter() {
        let mut h = RedrawDecisionHealth::baseline();
        record_os_paint_consumption(&mut h, OsPaintSignalSource::MacosSetNeedsDisplay);
        record_os_paint_consumption(&mut h, OsPaintSignalSource::MacosSetNeedsDisplay);
        record_os_paint_consumption(&mut h, OsPaintSignalSource::WaylandFrameCallback);
        assert_eq!(
            h.os_paint_consumptions.get("macos_set_needs_display"),
            Some(&2)
        );
        assert_eq!(
            h.os_paint_consumptions.get("wayland_frame_callback"),
            Some(&1)
        );
    }

    #[test]
    fn record_force_paint_counters() {
        let mut h = RedrawDecisionHealth::baseline();
        record_force_paint(&mut h, ForcePaintSignal::Bel);
        record_force_paint(&mut h, ForcePaintSignal::AtUpdatePending);
        assert_eq!(h.force_paint_counters.get("bel"), Some(&1));
        assert_eq!(h.force_paint_counters.get("at_update_pending"), Some(&1));
        assert_eq!(
            h.force_paint_counters.get("cosmetic_defer_outstanding"),
            None
        );
    }

    #[test]
    fn record_paint_signal_counters_saturate() {
        let mut h = RedrawDecisionHealth::baseline();
        h.force_paint_counters
            .insert("drag_resize".to_string(), u64::MAX);
        h.os_paint_consumptions
            .insert("wayland_frame_callback".to_string(), u64::MAX);

        record_force_paint(&mut h, ForcePaintSignal::DragResize);
        record_os_paint_consumption(&mut h, OsPaintSignalSource::WaylandFrameCallback);

        assert_eq!(h.force_paint_counters.get("drag_resize"), Some(&u64::MAX));
        assert_eq!(
            h.os_paint_consumptions.get("wayland_frame_callback"),
            Some(&u64::MAX)
        );
    }

    // ------------------------------------------------------------------------
    // Bench corpus + acceptance
    // ------------------------------------------------------------------------

    #[test]
    fn bench_scenario_corpus_has_three() {
        assert_eq!(bench_scenario_corpus().len(), 3);
    }

    #[test]
    fn idle_10s_acceptance_is_99_pct() {
        assert!(matches!(
            IdlePaintSkipBenchScenario::Idle10s12PaneFleet.acceptance(),
            IdlePaintSkipAcceptance::SkipRatePctMin { min: 99.0 }
        ));
    }

    #[test]
    fn typing_cadence_acceptance_is_40_pct() {
        assert!(matches!(
            IdlePaintSkipBenchScenario::TypingCadence1Hz.acceptance(),
            IdlePaintSkipAcceptance::SkipRatePctMin { min: 40.0 }
        ));
    }

    #[test]
    fn force_paint_every_frame_acceptance_is_zero_skips() {
        assert!(matches!(
            IdlePaintSkipBenchScenario::ForcePaintEveryFrame.acceptance(),
            IdlePaintSkipAcceptance::EveryEvaluationPaints
        ));
    }

    #[test]
    fn idle_bench_passes_at_99pct() {
        let h = RedrawDecisionHealth {
            evaluations_total: 600, // 10s × 60Hz
            paints_total: 6,
            skips_total: 594,
            ..RedrawDecisionHealth::baseline()
        };
        let r =
            IdlePaintSkipBenchResult::evaluate(IdlePaintSkipBenchScenario::Idle10s12PaneFleet, h);
        assert!(r.passed);
    }

    #[test]
    fn idle_bench_fails_below_99pct() {
        let h = RedrawDecisionHealth {
            evaluations_total: 600,
            paints_total: 12,
            skips_total: 588,
            ..RedrawDecisionHealth::baseline()
        };
        let r =
            IdlePaintSkipBenchResult::evaluate(IdlePaintSkipBenchScenario::Idle10s12PaneFleet, h);
        assert!(!r.passed);
    }

    #[test]
    fn force_paint_every_frame_bench_passes_when_no_skips() {
        let h = RedrawDecisionHealth {
            evaluations_total: 60,
            paints_total: 60,
            skips_total: 0,
            ..RedrawDecisionHealth::baseline()
        };
        let r =
            IdlePaintSkipBenchResult::evaluate(IdlePaintSkipBenchScenario::ForcePaintEveryFrame, h);
        assert!(r.passed);
    }

    #[test]
    fn force_paint_every_frame_bench_fails_with_any_skip() {
        let h = RedrawDecisionHealth {
            evaluations_total: 60,
            paints_total: 59,
            skips_total: 1,
            ..RedrawDecisionHealth::baseline()
        };
        let r =
            IdlePaintSkipBenchResult::evaluate(IdlePaintSkipBenchScenario::ForcePaintEveryFrame, h);
        assert!(!r.passed);
    }

    // ------------------------------------------------------------------------
    // Snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn snapshot_record_replaces_on_dup() {
        let mut s = IdlePaintSkipBenchSnapshot::new();
        s.record(IdlePaintSkipBenchResult::evaluate(
            IdlePaintSkipBenchScenario::Idle10s12PaneFleet,
            RedrawDecisionHealth::baseline(),
        ));
        s.record(IdlePaintSkipBenchResult::evaluate(
            IdlePaintSkipBenchScenario::Idle10s12PaneFleet,
            RedrawDecisionHealth::baseline(),
        ));
        assert_eq!(s.results.len(), 1);
    }

    #[test]
    fn snapshot_all_pass_requires_all_three() {
        let mut s = IdlePaintSkipBenchSnapshot::new();
        let healthy = RedrawDecisionHealth {
            evaluations_total: 100,
            paints_total: 1,
            skips_total: 99,
            ..RedrawDecisionHealth::baseline()
        };
        s.record(IdlePaintSkipBenchResult::evaluate(
            IdlePaintSkipBenchScenario::Idle10s12PaneFleet,
            healthy.clone(),
        ));
        s.record(IdlePaintSkipBenchResult::evaluate(
            IdlePaintSkipBenchScenario::TypingCadence1Hz,
            healthy,
        ));
        // Missing ForcePaintEveryFrame.
        assert!(!s.all_pass());
    }

    #[test]
    fn serde_roundtrip() {
        let s = IdlePaintSkipBenchSnapshot::new();
        let json = serde_json::to_string(&s).unwrap();
        let parsed: IdlePaintSkipBenchSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    // ------------------------------------------------------------------------
    // Integration shape — simulate the bead's headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn simulated_idle_10s_at_60hz_meets_idle_rq() {
        // 10s × 60Hz = 600 frames. On a 12-pane fleet at
        // idle, the predicate skips every frame except for
        // periodic cursor blinks (~1Hz) and any incidental
        // signals. Conservative: 6 paints, 594 skips = 99%.
        let mut h = RedrawDecisionHealth::baseline();
        for frame in 0..600 {
            // Paint every 100th frame — 6 total, 594 skips.
            let decision = if frame % 100 == 0 {
                DecisionRecord::Paint {
                    reason_slugs: vec!["cursor_blink".to_string()],
                }
            } else {
                DecisionRecord::Skip
            };
            fold_decision(&mut h, &decision);
        }
        assert_eq!(h.evaluations_total, 600);
        assert_eq!(h.paints_total, 6);
        assert_eq!(h.skips_total, 594);
        assert!(h.meets_idle_skip_rq());
        let r =
            IdlePaintSkipBenchResult::evaluate(IdlePaintSkipBenchScenario::Idle10s12PaneFleet, h);
        assert!(r.passed);
    }
}
