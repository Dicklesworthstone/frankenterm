//! DEC private mode 2026 — renderer presentation-hold
//! state machine
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.1.cont] / `ft-u6jos`).
//!
//! DEC mode 2026 ("synchronized output") lets a TUI app
//! bracket a multi-step redraw with `BSU` (Begin Synchronized
//! Update, `CSI ? 2026 h`) and `ESU` (End Synchronized Update,
//! `CSI ? 2026 l`). The renderer is expected to **hold
//! presentation** for the duration of the BSU/ESU window —
//! accumulating dirty bits but not calling `Present` — then
//! flush a single frame on the `ESU` transition. The visible
//! result: zero intermediate flicker, even when the app
//! issues many partial redraws inside the bracket.
//!
//! ## What's already in place
//!
//! - **Term-layer state machine** (`ft-d7af6`, shipped at
//!   `12b684db6`):
//!   `frankenterm/term/src/terminalstate/mod.rs` carries a
//!   `synchronized_output: bool` field; the BSU/ESU dispatch
//!   sets/clears it and `Terminal::synchronized_output()` is
//!   the renderer-facing getter.
//! - **Dirty-line bitmap** (`ft-mpc9b.1.2` /
//!   `ft-tfzhy`): the substrate the hold window
//!   accumulates into.
//!
//! ## What this module ships
//!
//! - [`PresentationHoldState`] — pure-logic state machine
//!   modeling the renderer's hold/flush behavior given the
//!   term layer's `synchronized_output` flag plus a stream of
//!   render events. State machine matches the bead's headline
//!   rule: **hold while flag is set, flush on true→false
//!   transition, never double-present a frame**.
//! - [`PresentationHoldEvent`] — taxonomy of events the
//!   state machine consumes (frame ready, BSU received, ESU
//!   received, dirty-line marked).
//! - [`PresentationHoldOutcome`] — what the renderer does in
//!   response (Present, Hold, Flush).
//! - [`SynchronizedOutputHealth`] — `ft doctor` snapshot
//!   matching this session's `*Health` shape. Surfaces
//!   `synchronized_output_active` (current flag),
//!   `bsu_count_total`, `esu_count_total`, `frames_held_total`.
//! - [`ConformanceApp`] + [`conformance_corpus`] — the bead's
//!   4-app per-app conformance corpus (nvim_treesitter,
//!   lazygit, btop, ranger).
//! - [`RolloutPhase`] — the bead's feature-flag staging:
//!   Hidden → OptIn → Default. Mirrors `ft-mpc9b.9` rollout
//!   substrate.
//!
//! ## What this module is NOT
//!
//! - Not the actual paint.rs wiring. The renderer's
//!   `crates/frankenterm-gui/src/termwindow/render/paint.rs`
//!   call site (the bead's action #1) consumes
//!   `PresentationHoldState`; it lands on the Linux GPU CI
//!   runner as the integration follow-on.
//! - Not the per-app fixture goldens. Action #2 captures
//!   actual VHS bytes from the 4 apps; this module ships the
//!   fixture-record contract those goldens populate.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

// ============================================================================
// Events + outcomes
// ============================================================================

/// One event the renderer state machine consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresentationHoldEvent {
    /// Term layer received `CSI ? 2026 h` and set the
    /// `synchronized_output` flag. The renderer enters Hold.
    Bsu,
    /// Term layer received `CSI ? 2026 l` and cleared the
    /// flag. The renderer flushes any held frame.
    Esu,
    /// A grid cell or line was marked dirty (mirrors a
    /// `DirtyLineBitmap` set bit). The state machine
    /// accumulates these across the hold window.
    DirtyLineMarked { line: u16 },
    /// Renderer's per-frame tick — would normally call
    /// `Present`, but the state machine may hold or pass
    /// through.
    FrameReady,
    /// Hard reset (e.g., synchronized terminal teardown). Per
    /// DEC 2026 spec, the implicit-end behavior MUST flush
    /// any held state to avoid stranding the user with a
    /// black window.
    Reset,
}

/// What the renderer should do in response to an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PresentationHoldOutcome {
    /// Call `Present` — render the frame to the screen.
    Present,
    /// Suppress `Present` — accumulate dirty bits but emit
    /// nothing.
    Hold,
    /// Flush the held frame in one go (Present called once,
    /// using the union of accumulated dirty bits).
    Flush { lines_flushed: u16 },
    /// State machine state changed but no presentation work
    /// to do (e.g., dirty-line accumulation while holding).
    NoOp,
}

// ============================================================================
// State machine
// ============================================================================

/// Renderer presentation-hold state.
///
/// Fields are `pub(crate)` so the entire hold-state machine
/// can only be mutated through [`apply_event`]. External
/// code that flips `synchronized_output_active = false`
/// mid-window would short-circuit the state machine and
/// cause held dirty lines to leak as orphans (the
/// `OrphanHeldLines` invariant violation). Privacy is
/// structural: read via accessors, mutate via apply_event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PresentationHoldState {
    /// True iff `synchronized_output` is set on the term
    /// layer (hold window active).
    pub(crate) synchronized_output_active: bool,
    /// Lines marked dirty during the current hold window.
    /// Empty when `!synchronized_output_active`.
    pub(crate) held_dirty_lines: BTreeSet<u16>,
    /// Total BSU events observed.
    pub(crate) bsu_count_total: u64,
    /// Total ESU events observed.
    pub(crate) esu_count_total: u64,
    /// Total `FrameReady` ticks suppressed by holds.
    pub(crate) frames_held_total: u64,
    /// Total flushes issued (one per ESU/Reset transition
    /// that ended a non-empty hold).
    pub(crate) frames_flushed_total: u64,
    /// Adversarial ESU events: ESU fired with
    /// `synchronized_output_active == false`. Mirrors
    /// `sync_output_watchdog.rs::adversarial_esu_underflow_count`
    /// at the renderer layer. Operators alarm on
    /// non-zero — indicates a malicious or buggy app
    /// emitting unmatched ESU.
    #[serde(default)]
    pub(crate) adversarial_esu_total: u64,
}

impl PresentationHoldState {
    #[must_use]
    pub const fn initial() -> Self {
        Self {
            synchronized_output_active: false,
            held_dirty_lines: BTreeSet::new(),
            bsu_count_total: 0,
            esu_count_total: 0,
            frames_held_total: 0,
            frames_flushed_total: 0,
            adversarial_esu_total: 0,
        }
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn synchronized_output_active(&self) -> bool {
        self.synchronized_output_active
    }

    #[must_use]
    pub fn held_dirty_lines(&self) -> &BTreeSet<u16> {
        &self.held_dirty_lines
    }

    #[must_use]
    pub const fn bsu_count_total(&self) -> u64 {
        self.bsu_count_total
    }

    #[must_use]
    pub const fn esu_count_total(&self) -> u64 {
        self.esu_count_total
    }

    #[must_use]
    pub const fn frames_held_total(&self) -> u64 {
        self.frames_held_total
    }

    #[must_use]
    pub const fn frames_flushed_total(&self) -> u64 {
        self.frames_flushed_total
    }

    #[must_use]
    pub const fn adversarial_esu_total(&self) -> u64 {
        self.adversarial_esu_total
    }
}

/// Apply one event to the state machine. Returns the
/// renderer's outcome.
pub fn apply_event(
    state: &mut PresentationHoldState,
    event: PresentationHoldEvent,
) -> PresentationHoldOutcome {
    match event {
        PresentationHoldEvent::Bsu => {
            state.bsu_count_total = state.bsu_count_total.saturating_add(1);
            // Idempotent: re-issuing BSU while already
            // synchronized stays in Hold; clears any
            // accidental drift.
            state.synchronized_output_active = true;
            PresentationHoldOutcome::NoOp
        }
        PresentationHoldEvent::Esu => {
            state.esu_count_total = state.esu_count_total.saturating_add(1);
            // Detect adversarial ESU: ESU fired without
            // matching BSU. Bump the dedicated counter so
            // operators monitoring for unmatched-ESU
            // patterns can spot trends.
            if !state.synchronized_output_active {
                state.adversarial_esu_total = state.adversarial_esu_total.saturating_add(1);
            }
            // Flush iff there was a held frame to flush. If
            // the app issued ESU without prior dirty during
            // the window, NoOp (avoid spurious presents).
            if state.synchronized_output_active && !state.held_dirty_lines.is_empty() {
                let n = state.held_dirty_lines.len() as u16;
                state.held_dirty_lines.clear();
                state.synchronized_output_active = false;
                state.frames_flushed_total = state.frames_flushed_total.saturating_add(1);
                PresentationHoldOutcome::Flush { lines_flushed: n }
            } else {
                state.synchronized_output_active = false;
                state.held_dirty_lines.clear();
                PresentationHoldOutcome::NoOp
            }
        }
        PresentationHoldEvent::DirtyLineMarked { line } => {
            if state.synchronized_output_active {
                state.held_dirty_lines.insert(line);
                PresentationHoldOutcome::NoOp
            } else {
                // Outside hold window — caller paints
                // immediately on next FrameReady.
                PresentationHoldOutcome::NoOp
            }
        }
        PresentationHoldEvent::FrameReady => {
            if state.synchronized_output_active {
                state.frames_held_total = state.frames_held_total.saturating_add(1);
                PresentationHoldOutcome::Hold
            } else {
                PresentationHoldOutcome::Present
            }
        }
        PresentationHoldEvent::Reset => {
            // Implicit-end behavior — flush any pending hold,
            // then return to inactive.
            let outcome = if state.synchronized_output_active && !state.held_dirty_lines.is_empty()
            {
                let n = state.held_dirty_lines.len() as u16;
                state.frames_flushed_total = state.frames_flushed_total.saturating_add(1);
                PresentationHoldOutcome::Flush { lines_flushed: n }
            } else {
                PresentationHoldOutcome::NoOp
            };
            state.synchronized_output_active = false;
            state.held_dirty_lines.clear();
            outcome
        }
    }
}

// ============================================================================
// Invariants
// ============================================================================

/// Named safety violation for the BFS proof harness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresentationHoldViolation {
    /// `held_dirty_lines` is non-empty but
    /// `synchronized_output_active` is false. The hold
    /// window's dirty set should always be empty when not
    /// holding (every transition out of Hold either flushes
    /// or clears).
    OrphanHeldLines { count: u32 },
    /// Saturating counters hit `u64::MAX`. Indicates a stuck
    /// loop somewhere — the harness flags it so a triager
    /// catches a runaway counter.
    SaturatedCounter { name: String },
    /// Frame was held while `synchronized_output_active` was
    /// false. Indicates a Hold outcome leaked outside the
    /// window.
    HoldOutsideWindow,
    /// `Present` outcome fired while
    /// `synchronized_output_active` was true. Indicates the
    /// renderer ignored the hold flag.
    PresentDuringWindow,
}

#[must_use]
pub fn check_invariants(
    _prior: &PresentationHoldState,
    state: &PresentationHoldState,
    last_event: PresentationHoldEvent,
    last_outcome: PresentationHoldOutcome,
) -> Vec<PresentationHoldViolation> {
    let mut out = Vec::new();

    // OrphanHeldLines.
    if !state.synchronized_output_active && !state.held_dirty_lines.is_empty() {
        out.push(PresentationHoldViolation::OrphanHeldLines {
            count: state.held_dirty_lines.len() as u32,
        });
    }

    // SaturatedCounter.
    for (name, value) in [
        ("bsu_count_total", state.bsu_count_total),
        ("esu_count_total", state.esu_count_total),
        ("frames_held_total", state.frames_held_total),
        ("frames_flushed_total", state.frames_flushed_total),
    ] {
        if value == u64::MAX {
            out.push(PresentationHoldViolation::SaturatedCounter {
                name: name.to_string(),
            });
        }
    }

    // HoldOutsideWindow / PresentDuringWindow.
    if let PresentationHoldEvent::FrameReady = last_event {
        match (state.synchronized_output_active, last_outcome) {
            (true, PresentationHoldOutcome::Present) => {
                out.push(PresentationHoldViolation::PresentDuringWindow);
            }
            (false, PresentationHoldOutcome::Hold) => {
                out.push(PresentationHoldViolation::HoldOutsideWindow);
            }
            _ => {}
        }
    }

    out
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the synchronized-output renderer
/// surface. Mirrors the `*Health` shape used across this
/// session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SynchronizedOutputHealth {
    pub(crate) synchronized_output_active: bool,
    pub(crate) bsu_count_total: u64,
    pub(crate) esu_count_total: u64,
    pub(crate) frames_held_total: u64,
    pub(crate) frames_flushed_total: u64,
    pub(crate) held_lines_now: u32,
    /// Adversarial-ESU counter (mirrors
    /// `PresentationHoldState::adversarial_esu_total`).
    /// Operator alarm signal — non-zero indicates a
    /// malicious or buggy app emitting unmatched ESU.
    #[serde(default)]
    pub(crate) adversarial_esu_total: u64,
}

impl SynchronizedOutputHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            synchronized_output_active: false,
            bsu_count_total: 0,
            esu_count_total: 0,
            frames_held_total: 0,
            frames_flushed_total: 0,
            held_lines_now: 0,
            adversarial_esu_total: 0,
        }
    }

    /// True iff the BSU/ESU counts are balanced (or the
    /// state is currently idle and never bracketed).
    /// Operators read this to detect runaway / unbalanced
    /// brackets.
    #[must_use]
    pub const fn bsu_esu_balanced(&self) -> bool {
        // BSU >= ESU at all times (every ESU consumes one
        // pending BSU); equality when not currently in hold.
        if self.synchronized_output_active {
            self.bsu_count_total == self.esu_count_total + 1
        } else {
            self.bsu_count_total == self.esu_count_total
        }
    }

    /// Project a `PresentationHoldState` into this snapshot.
    #[must_use]
    pub fn from_state(state: &PresentationHoldState) -> Self {
        Self {
            synchronized_output_active: state.synchronized_output_active,
            bsu_count_total: state.bsu_count_total,
            esu_count_total: state.esu_count_total,
            frames_held_total: state.frames_held_total,
            frames_flushed_total: state.frames_flushed_total,
            held_lines_now: state.held_dirty_lines.len() as u32,
            adversarial_esu_total: state.adversarial_esu_total,
        }
    }

    /// True iff no proof-harness violation would fire AND
    /// no adversarial-ESU events have been observed.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        // No saturated counters; orphan held lines impossible
        // when reachable from apply_event alone; no
        // unmatched-ESU attacks observed.
        self.bsu_count_total < u64::MAX
            && self.esu_count_total < u64::MAX
            && self.frames_held_total < u64::MAX
            && self.frames_flushed_total < u64::MAX
            && self.adversarial_esu_total == 0
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn synchronized_output_active(&self) -> bool {
        self.synchronized_output_active
    }
    #[must_use]
    pub const fn bsu_count_total(&self) -> u64 {
        self.bsu_count_total
    }
    #[must_use]
    pub const fn esu_count_total(&self) -> u64 {
        self.esu_count_total
    }
    #[must_use]
    pub const fn frames_held_total(&self) -> u64 {
        self.frames_held_total
    }
    #[must_use]
    pub const fn frames_flushed_total(&self) -> u64 {
        self.frames_flushed_total
    }
    #[must_use]
    pub const fn held_lines_now(&self) -> u32 {
        self.held_lines_now
    }
    #[must_use]
    pub const fn adversarial_esu_total(&self) -> u64 {
        self.adversarial_esu_total
    }
}

// ============================================================================
// Per-app conformance corpus
// ============================================================================

/// One app the bead's action #2 records a fixture for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceApp {
    /// Heavy redraw with syntax highlighting.
    NvimTreesitter,
    /// Staging-area scroll.
    Lazygit,
    /// Full-screen redraw.
    Btop,
    /// Multi-pane file browser.
    Ranger,
}

impl ConformanceApp {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::NvimTreesitter => "nvim_treesitter",
            Self::Lazygit => "lazygit",
            Self::Btop => "btop",
            Self::Ranger => "ranger",
        }
    }

    /// Bead-specified rationale for this app's coverage.
    #[must_use]
    pub const fn rationale(self) -> &'static str {
        match self {
            Self::NvimTreesitter => "heavy redraw with syntax highlighting",
            Self::Lazygit => "staging-area scroll",
            Self::Btop => "full-screen redraw",
            Self::Ranger => "multi-pane file browser",
        }
    }

    pub const ALL: &'static [Self] = &[
        Self::NvimTreesitter,
        Self::Lazygit,
        Self::Btop,
        Self::Ranger,
    ];
}

/// Per-app fixture record contract. The bead's action #2
/// captures actual VHS bytes for each app and stores the
/// fixture under `tests/golden/dec_2026/<slug>/`. This module
/// ships the contract for what each fixture's `meta.json`
/// must record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceFixture {
    pub app: ConformanceApp,
    /// Path to the captured byte stream relative to fixture
    /// dir (`input.bytes`).
    pub input_bytes_path: String,
    /// Expected post-state — number of `Present` calls the
    /// hold-aware renderer should emit. Acceptance: `1` per
    /// BSU/ESU window (the bead's "frame count = 1, no
    /// intermediate frames" rule).
    pub expected_present_count: u32,
    /// Expected count of `FrameReady` events suppressed
    /// during the hold window.
    pub expected_frames_held: u32,
    /// Number of dirty lines accumulated during the hold.
    pub expected_lines_flushed: u32,
}

#[must_use]
pub fn conformance_corpus() -> Vec<ConformanceFixture> {
    ConformanceApp::ALL
        .iter()
        .map(|app| ConformanceFixture {
            app: *app,
            input_bytes_path: "input.bytes".to_string(),
            // Acceptance bound: every fixture should produce
            // exactly 1 Present per BSU/ESU window. The
            // integration follow-on captures actual numbers
            // and updates these baselines.
            expected_present_count: 1,
            expected_frames_held: 0,   // populated per-fixture
            expected_lines_flushed: 0, // populated per-fixture
        })
        .collect()
}

// ============================================================================
// Rollout staging
// ============================================================================

/// Feature-flag rollout phase. Mirrors the rollout substrate
/// from `ft-mpc9b.9`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    /// Feature compiled in but not exposed; only
    /// `FT_FEATURE_DEC_2026=force_on` enables it. Pre-release.
    Hidden,
    /// User opts in via config (`[features] dec_2026 = "on"`).
    /// CI runs the conformance corpus; ops can revert via
    /// flag flip without redeploy.
    OptIn,
    /// Default-enabled. The Hidden / OptIn paths remain for
    /// a release cycle as escape hatches; deletion at the
    /// next major version.
    Default,
}

impl RolloutPhase {
    /// Whether the feature is exposed to users at this
    /// phase.
    #[must_use]
    pub const fn user_visible(self) -> bool {
        matches!(self, Self::OptIn | Self::Default)
    }

    /// Whether the feature is on by default.
    #[must_use]
    pub const fn on_by_default(self) -> bool {
        matches!(self, Self::Default)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_state_is_idle() {
        let s = PresentationHoldState::initial();
        assert!(!s.synchronized_output_active);
        assert!(s.held_dirty_lines.is_empty());
    }

    #[test]
    fn frame_ready_outside_window_presents() {
        let mut s = PresentationHoldState::initial();
        let prior = s.clone();
        let outcome = apply_event(&mut s, PresentationHoldEvent::FrameReady);
        assert_eq!(outcome, PresentationHoldOutcome::Present);
        assert!(
            check_invariants(&prior, &s, PresentationHoldEvent::FrameReady, outcome).is_empty()
        );
    }

    #[test]
    fn frame_ready_inside_window_holds() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        let prior = s.clone();
        let outcome = apply_event(&mut s, PresentationHoldEvent::FrameReady);
        assert_eq!(outcome, PresentationHoldOutcome::Hold);
        assert_eq!(s.frames_held_total, 1);
        assert!(
            check_invariants(&prior, &s, PresentationHoldEvent::FrameReady, outcome).is_empty()
        );
    }

    #[test]
    fn esu_after_bsu_with_dirty_flushes() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 3 });
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 5 });
        apply_event(&mut s, PresentationHoldEvent::FrameReady); // held
        let prior = s.clone();
        let outcome = apply_event(&mut s, PresentationHoldEvent::Esu);
        assert_eq!(outcome, PresentationHoldOutcome::Flush { lines_flushed: 2 });
        assert!(!s.synchronized_output_active);
        assert!(s.held_dirty_lines.is_empty());
        assert_eq!(s.frames_flushed_total, 1);
        assert!(check_invariants(&prior, &s, PresentationHoldEvent::Esu, outcome).is_empty());
    }

    #[test]
    fn esu_after_bsu_without_dirty_is_noop() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        let prior = s.clone();
        let outcome = apply_event(&mut s, PresentationHoldEvent::Esu);
        assert_eq!(outcome, PresentationHoldOutcome::NoOp);
        assert_eq!(s.frames_flushed_total, 0);
        assert!(!s.synchronized_output_active);
        assert!(check_invariants(&prior, &s, PresentationHoldEvent::Esu, outcome).is_empty());
    }

    #[test]
    fn duplicate_bsu_stays_in_hold() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        assert_eq!(s.bsu_count_total, 2);
        assert!(s.synchronized_output_active);
    }

    #[test]
    fn dirty_outside_window_does_not_accumulate() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 1 });
        assert!(s.held_dirty_lines.is_empty());
    }

    #[test]
    fn adversarial_esu_without_bsu_bumps_counter() {
        // ESU fired without prior BSU is an adversarial
        // signal — malicious or buggy app emits unmatched
        // ESU. Mirrors sync_output_watchdog.rs's
        // adversarial_esu_underflow_count at the renderer
        // layer. Operators alarm on non-zero
        // adversarial_esu_total.
        let mut s = PresentationHoldState::initial();
        let outcome = apply_event(&mut s, PresentationHoldEvent::Esu);
        assert_eq!(outcome, PresentationHoldOutcome::NoOp);
        assert_eq!(s.adversarial_esu_total(), 1);
        assert_eq!(s.bsu_count_total(), 0);
        assert_eq!(s.esu_count_total(), 1);
        // bsu_esu_balanced returns false (0 != 1).
        let h = SynchronizedOutputHealth::from_state(&s);
        assert!(!h.bsu_esu_balanced());
        // is_safe is false because adversarial_esu_total != 0.
        assert!(!h.is_safe());
    }

    #[test]
    fn legitimate_bsu_esu_does_not_bump_adversarial_counter() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::Esu);
        assert_eq!(s.adversarial_esu_total(), 0);
        let h = SynchronizedOutputHealth::from_state(&s);
        assert!(h.bsu_esu_balanced());
        assert!(h.is_safe());
    }

    #[test]
    fn presentation_hold_state_accessors_round_trip() {
        // Pin the read-only accessor surface — pub(crate)
        // fields can't be mutated from outside; only
        // apply_event mutates the state.
        let mut s = PresentationHoldState::initial();
        assert!(!s.synchronized_output_active());
        assert!(s.held_dirty_lines().is_empty());
        assert_eq!(s.bsu_count_total(), 0);
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 7 });
        assert!(s.synchronized_output_active());
        assert!(s.held_dirty_lines().contains(&7));
        assert_eq!(s.bsu_count_total(), 1);
    }

    #[test]
    fn duplicate_dirty_line_is_idempotent() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 5 });
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 5 });
        assert_eq!(s.held_dirty_lines.len(), 1);
    }

    #[test]
    fn reset_flushes_pending_hold() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 7 });
        let prior = s.clone();
        let outcome = apply_event(&mut s, PresentationHoldEvent::Reset);
        assert!(matches!(
            outcome,
            PresentationHoldOutcome::Flush { lines_flushed: 1 }
        ));
        assert!(!s.synchronized_output_active);
        assert!(s.held_dirty_lines.is_empty());
        assert!(check_invariants(&prior, &s, PresentationHoldEvent::Reset, outcome).is_empty());
    }

    #[test]
    fn reset_with_no_pending_is_noop() {
        let mut s = PresentationHoldState::initial();
        let prior = s.clone();
        let outcome = apply_event(&mut s, PresentationHoldEvent::Reset);
        assert_eq!(outcome, PresentationHoldOutcome::NoOp);
        assert_eq!(s, prior);
    }

    #[test]
    fn health_balanced_when_idle_with_matched_counts() {
        let s = PresentationHoldState {
            bsu_count_total: 5,
            esu_count_total: 5,
            ..PresentationHoldState::initial()
        };
        let h = SynchronizedOutputHealth::from_state(&s);
        assert!(h.bsu_esu_balanced());
    }

    #[test]
    fn health_balanced_when_active_with_one_pending() {
        let s = PresentationHoldState {
            synchronized_output_active: true,
            bsu_count_total: 6,
            esu_count_total: 5,
            ..PresentationHoldState::initial()
        };
        let h = SynchronizedOutputHealth::from_state(&s);
        assert!(h.bsu_esu_balanced());
    }

    #[test]
    fn health_unbalanced_indicates_drift() {
        let s = PresentationHoldState {
            bsu_count_total: 5,
            esu_count_total: 3, // unbalanced — runaway BSU
            ..PresentationHoldState::initial()
        };
        let h = SynchronizedOutputHealth::from_state(&s);
        assert!(!h.bsu_esu_balanced());
    }

    #[test]
    fn baseline_health_is_safe() {
        assert!(SynchronizedOutputHealth::baseline().is_safe());
    }

    #[test]
    fn conformance_corpus_has_four_apps() {
        let c = conformance_corpus();
        assert_eq!(c.len(), 4);
        assert_eq!(ConformanceApp::ALL.len(), 4);
    }

    #[test]
    fn conformance_app_slugs_distinct() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for a in ConformanceApp::ALL {
            assert!(seen.insert(a.slug()), "dup {}", a.slug());
            assert!(!a.rationale().is_empty());
        }
    }

    #[test]
    fn rollout_phase_visibility() {
        assert!(!RolloutPhase::Hidden.user_visible());
        assert!(RolloutPhase::OptIn.user_visible());
        assert!(RolloutPhase::Default.user_visible());

        assert!(!RolloutPhase::Hidden.on_by_default());
        assert!(!RolloutPhase::OptIn.on_by_default());
        assert!(RolloutPhase::Default.on_by_default());
    }

    #[test]
    fn rollout_phase_is_ordered() {
        assert!(RolloutPhase::Hidden < RolloutPhase::OptIn);
        assert!(RolloutPhase::OptIn < RolloutPhase::Default);
    }

    #[test]
    fn state_serde_roundtrips() {
        let mut s = PresentationHoldState::initial();
        apply_event(&mut s, PresentationHoldEvent::Bsu);
        apply_event(&mut s, PresentationHoldEvent::DirtyLineMarked { line: 3 });
        let json = serde_json::to_string(&s).unwrap();
        let parsed: PresentationHoldState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, s);
    }

    #[test]
    fn random_schedule_sweep_no_violations() {
        let mut rng: u64 = 0xa5a5_5a5a_dead_beefu64;
        let xorshift = |s: &mut u64| -> u64 {
            let mut x = *s;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *s = x;
            x
        };
        for _ in 0..1024 {
            let mut state = PresentationHoldState::initial();
            for _ in 0..16 {
                let r = xorshift(&mut rng);
                let event = match r % 5 {
                    0 => PresentationHoldEvent::Bsu,
                    1 => PresentationHoldEvent::Esu,
                    2 => PresentationHoldEvent::DirtyLineMarked {
                        line: ((r >> 8) % 24) as u16,
                    },
                    3 => PresentationHoldEvent::FrameReady,
                    _ => PresentationHoldEvent::Reset,
                };
                let prior = state.clone();
                let outcome = apply_event(&mut state, event);
                let v = check_invariants(&prior, &state, event, outcome);
                assert!(v.is_empty(), "violation under {event:?}: {v:?}");
            }
        }
    }
}
