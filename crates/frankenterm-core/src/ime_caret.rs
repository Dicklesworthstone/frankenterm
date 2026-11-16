//! IME composition-window caret-anchor contract and regression
//! fixture infrastructure ([BR-TERM-EMULATOR-UPLIFT.A11Y.2] /
//! `ft-mpc9b.10.2`).
//!
//! IME (Input Method Editor) composition windows must track the
//! caret position. Pinyin / Japanese / Korean / Vietnamese users see
//! a floating composition window that anchors to where they're
//! typing. A renderer that drops or skips caret-position updates —
//! during draft-quality frames, live-resize gestures, or idle
//! wake-up — leaves the IME window stranded at a stale position.
//!
//! This module establishes the **contract** the per-platform IME
//! recorders must satisfy:
//!
//! - [`CaretAnchorRect`] — the window-relative caret rectangle the
//!   GUI computes and hands to `Window::set_text_cursor_position`.
//!   The platform layer (NSTextInputClient on macOS,
//!   `text-input-v3` on Wayland, XIM on X11) consumes it.
//! - [`compute_caret_anchor_rect`] — the **pure** caret-rectangle
//!   computation extracted from
//!   `crates/frankenterm-gui/src/termwindow/mod.rs::update_text_cursor`
//!   so the math is testable without a real GPU/window.
//! - [`RenderQuality`] — the closed list of render-quality modes from
//!   `ft-mpc9b.2.2`. The IME contract is "every quality MUST dispatch
//!   the caret update"; the regression fixture proves no quality
//!   silently elides it.
//! - [`ImeScenario`] — the closed list of caret-anchor scenarios
//!   (typing / draft-quality burst / live-resize / idle wake-up /
//!   focus change).
//! - [`ImeUpdate`] — one row of `tests/ime/logs/<platform>-<scenario>.jsonl`.
//! - [`should_dispatch_after_state_change`] — the predicate that
//!   answers "did anything observable to the IME change since the
//!   last dispatch?". The X11 / Wayland platform layers' current
//!   dedups are a strict subset of this predicate; see the
//!   accompanying audit doc for the gap analysis.
//!
//! See `docs/a11y/ime-caret-anchor.md` for the per-platform code-
//! citation audit and the closure plan.

use serde::{Deserialize, Serialize};

// ============================================================================
// Caret rectangle
// ============================================================================

/// The window-relative caret rectangle the GUI computes and hands to
/// `Window::set_text_cursor_position`. The platform layer composes
/// it with the window's screen position; ft-side correctness lives
/// entirely in this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CaretAnchorRect {
    /// Window-relative x in pixels.
    pub origin_x: i64,
    /// Window-relative y in pixels (origin = top-left of the window
    /// content area, NOT the screen).
    pub origin_y: i64,
    /// Caret cell width in pixels.
    pub width: u32,
    /// Caret cell height in pixels.
    pub height: u32,
}

impl CaretAnchorRect {
    /// Construct from raw window-relative coordinates. Negative
    /// width/height inputs are clamped to 0 — a malformed cell-size
    /// upstream MUST NOT crash the IME path.
    #[must_use]
    pub fn new(origin_x: i64, origin_y: i64, width: i64, height: i64) -> Self {
        Self {
            origin_x,
            origin_y,
            width: width.max(0) as u32,
            height: height.max(0) as u32,
        }
    }

    /// True iff the rect has non-zero extent. Zero-extent rects come
    /// from upstream when the cell metrics aren't initialized yet —
    /// the IME platform layer typically treats them as "no caret"
    /// and skips the update.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

// ============================================================================
// Pane geometry input
// ============================================================================

/// The minimal set of geometry values needed to compute a caret
/// anchor rect. Mirrors the inputs the GUI's
/// `update_text_cursor` reaches for, but extracted to a plain struct
/// so the regression fixture can exercise it without a `TermWindow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaretGeometry {
    /// Cursor cell column inside the pane (0-based).
    pub cursor_cell_col: i64,
    /// Cursor cell row inside the pane (0-based).
    pub cursor_cell_row: i64,
    /// Pane top in cells (the pane's `top` offset).
    pub pane_top_cell: i64,
    /// Pane left in cells.
    pub pane_left_cell: i64,
    /// `physical_top` from the pane's dimensions — the scroll offset.
    pub physical_top: i64,
    /// Cell width in pixels.
    pub cell_width_px: i64,
    /// Cell height in pixels.
    pub cell_height_px: i64,
    /// Tab-bar pixel height contributing to the y-offset.
    pub tab_bar_height_px: i64,
    /// Window padding (left).
    pub padding_left_px: i64,
    /// Window padding (top).
    pub padding_top_px: i64,
}

/// Pure caret-rect computation. Mirrors the math in
/// `frankenterm_gui::termwindow::TermWindow::update_text_cursor`.
///
/// Extracted here so the regression fixture can verify the
/// per-RenderQuality, per-resize-state invariants without spinning a
/// real GPU/window. The GUI delegates to this function so the math
/// has exactly one source of truth.
#[must_use]
pub fn compute_caret_anchor_rect(geom: CaretGeometry) -> CaretAnchorRect {
    let x_cell = geom.cursor_cell_col + geom.pane_left_cell;
    let x_px = x_cell.max(0).saturating_mul(geom.cell_width_px) + geom.padding_left_px;

    let y_cell = (geom.cursor_cell_row + geom.pane_top_cell - geom.physical_top).max(0);
    let y_px =
        y_cell.saturating_mul(geom.cell_height_px) + geom.tab_bar_height_px + geom.padding_top_px;

    CaretAnchorRect::new(x_px, y_px, geom.cell_width_px, geom.cell_height_px)
}

// ============================================================================
// Render quality (re-export from canonical home)
// ============================================================================
//
// `RenderQuality` originally landed here as a forward-looking
// declaration for `ft-mpc9b.2.2`. That bead has now landed and
// `crate::render_quality` is the canonical home with the full
// `DraftModeFeatureFlags` + `DraftModeDriver` policy. Re-export
// here so existing `frankenterm_core::ime_caret::RenderQuality`
// paths and golden JSONL files (which serialize the same
// snake_case strings) keep resolving unchanged.

pub use crate::render_quality::RenderQuality;

// ============================================================================
// IME platform
// ============================================================================

/// Identifies the IME framework a recorder targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImePlatform {
    /// macOS `NSTextInputClient`.
    MacosNsTextInput,
    /// Linux/Wayland `text-input-v3` protocol.
    WaylandTextInputV3,
    /// Linux/X11 XIM.
    X11Xim,
    /// Synthetic — the contract recorder.
    Synthetic,
}

impl ImePlatform {
    /// Filename slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::MacosNsTextInput => "macos",
            Self::WaylandTextInputV3 => "wayland",
            Self::X11Xim => "x11",
            Self::Synthetic => "synthetic",
        }
    }

    /// Whether a real platform integration is wired.
    #[must_use]
    pub const fn is_wired(self) -> bool {
        matches!(self, Self::Synthetic)
    }

    /// Stable golden-file path for `(platform, scenario)`.
    #[must_use]
    pub fn golden_filename(self, scenario: ImeScenario) -> String {
        format!("{}-{}.jsonl", self.slug(), scenario.slug())
    }
}

// ============================================================================
// Scenarios
// ============================================================================

/// The closed list of IME caret-anchor scenarios from the bead
/// description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImeScenario {
    /// Steady-state typing into the focused pane.
    Typing,
    /// Burst of frames at draft quality (gesture-driven).
    DraftQualityBurst,
    /// Live window-resize gesture.
    LiveResize,
    /// Wake from idle: no paint frames for >500ms, then composition
    /// resumes (the idle frame-rate dropdown from `ft-mpc9b.5.3`).
    IdleWakeup,
    /// Focus moves to a different pane mid-composition.
    FocusChange,
}

impl ImeScenario {
    /// Filename slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Typing => "typing",
            Self::DraftQualityBurst => "draft_quality_burst",
            Self::LiveResize => "live_resize",
            Self::IdleWakeup => "idle_wakeup",
            Self::FocusChange => "focus_change",
        }
    }

    /// Every scenario in declaration order.
    pub const ALL: &'static [ImeScenario] = &[
        Self::Typing,
        Self::DraftQualityBurst,
        Self::LiveResize,
        Self::IdleWakeup,
        Self::FocusChange,
    ];
}

// ============================================================================
// IME update event
// ============================================================================

/// One IME caret-anchor update — the unit of structured logging at
/// `tests/ime/logs/<platform>-<scenario>.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImeUpdate {
    /// Monotonic timestamp (ms since fixture start).
    pub ts_ms: u64,
    /// Scenario slug for filtering.
    pub scenario: String,
    /// Render quality at the time the update was dispatched.
    pub render_quality: RenderQuality,
    /// Caret rect dispatched to the platform IME.
    pub caret: CaretAnchorRect,
    /// Whether this update was actually dispatched (false = the
    /// dedup short-circuited; the regression fixture flags every
    /// false reading per scenario).
    pub dispatched: bool,
}

// ============================================================================
// Dispatch-fire predicate
//
// The platform-side dedups in X11 (`if self.last_cursor_position ==
// cursor`) and Wayland (`if self.text_cursor.map(|prior| prior !=
// rect)`) currently key on the **window-relative** caret rect alone.
// They miss state changes the IME cares about that don't move the
// caret cell:
//
//   - Window moved on screen (X11 XIM caches the screen position).
//   - Window resized (post-resize the cell rect can collide with the
//     pre-resize rect even though the surrounding layout shifted).
//   - Render quality flipped (Draft↔Standard transition is invisible
//     to the dedup).
//   - Idle wake-up (the IME may have lost client state).
//
// `should_dispatch_after_state_change` is the corrected predicate:
// dispatch when the cell rect differs OR any of the listed
// state-changes fired. Future platform fixes route through this.
// ============================================================================

/// State that, if changed since the last dispatch, requires a fresh
/// `set_text_cursor_position` call even when the cell-rect didn't
/// move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImeDispatchState {
    /// Window position on screen at the time of the last dispatch.
    pub window_screen_origin: (i32, i32),
    /// Window size at the time of the last dispatch.
    pub window_size: (u32, u32),
    /// Render quality at the time of the last dispatch.
    pub render_quality: RenderQuality,
    /// True iff the window was idle (no recent paint frames) at the
    /// moment of the last dispatch.
    pub was_idle: bool,
}

/// Returns true iff a fresh `set_text_cursor_position` MUST fire.
///
/// Strictly broader than the X11 / Wayland dedups: catches
/// window-move, resize, quality transition, and idle-wakeup deltas
/// that the existing dedups miss.
#[must_use]
pub fn should_dispatch_after_state_change(
    last_caret: Option<CaretAnchorRect>,
    next_caret: CaretAnchorRect,
    last_state: Option<ImeDispatchState>,
    next_state: ImeDispatchState,
) -> bool {
    // First update — always dispatch.
    let (Some(last_caret), Some(last_state)) = (last_caret, last_state) else {
        return true;
    };
    if last_caret != next_caret {
        return true;
    }
    if last_state.window_screen_origin != next_state.window_screen_origin {
        return true;
    }
    if last_state.window_size != next_state.window_size {
        return true;
    }
    if last_state.render_quality != next_state.render_quality {
        return true;
    }
    // Wake-from-idle: previous dispatch was while idle, but we're
    // now active.
    if last_state.was_idle && !next_state.was_idle {
        return true;
    }
    false
}

// ============================================================================
// JSONL log writer
// ============================================================================

/// Serialize a slice of [`ImeUpdate`] events as JSONL.
#[must_use]
pub fn render_updates_jsonl(updates: &[ImeUpdate]) -> String {
    let mut out = String::new();
    for u in updates {
        let line = serde_json::to_string(u).expect("ImeUpdate always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse a JSONL string back into a vector of updates.
pub fn parse_updates_jsonl(jsonl: &str) -> Result<Vec<ImeUpdate>, serde_json::Error> {
    let mut updates = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        updates.push(serde_json::from_str(trimmed)?);
    }
    Ok(updates)
}

// ============================================================================
// Contract scenario builder
// ============================================================================

/// Build the canonical `ImeUpdate` sequence for a scenario. Every
/// per-platform recorder must reproduce the same dispatch-or-elide
/// pattern; the bead's correctness rule is that EVERY update in the
/// canonical sequence has `dispatched = true`.
#[must_use]
pub fn contract_updates(scenario: ImeScenario) -> Vec<ImeUpdate> {
    let base_caret = CaretAnchorRect::new(80, 240, 8, 16);
    match scenario {
        ImeScenario::Typing => (0..3)
            .map(|i| ImeUpdate {
                ts_ms: i as u64 * 50,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                caret: CaretAnchorRect::new(80 + i * 8, 240, 8, 16),
                dispatched: true,
            })
            .collect(),
        ImeScenario::DraftQualityBurst => (0..4)
            .map(|i| ImeUpdate {
                ts_ms: i as u64 * 16,
                scenario: scenario.slug().to_string(),
                render_quality: if i == 0 || i == 3 {
                    RenderQuality::Standard
                } else {
                    RenderQuality::Draft
                },
                caret: base_caret,
                dispatched: true,
            })
            .collect(),
        ImeScenario::LiveResize => vec![
            ImeUpdate {
                ts_ms: 0,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                caret: base_caret,
                dispatched: true,
            },
            ImeUpdate {
                ts_ms: 16,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Draft,
                // Cell rect identical post-resize; this update MUST
                // still dispatch because the window-screen-origin
                // changed.
                caret: base_caret,
                dispatched: true,
            },
            ImeUpdate {
                ts_ms: 32,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                // Resize finished: cells reflowed → caret moved.
                caret: CaretAnchorRect::new(96, 240, 10, 18),
                dispatched: true,
            },
        ],
        ImeScenario::IdleWakeup => vec![
            ImeUpdate {
                ts_ms: 0,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                caret: base_caret,
                dispatched: true,
            },
            ImeUpdate {
                ts_ms: 1500,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                // Cell rect unchanged; dispatch MUST still fire on
                // wake to reset IME state.
                caret: base_caret,
                dispatched: true,
            },
        ],
        ImeScenario::FocusChange => vec![
            ImeUpdate {
                ts_ms: 0,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                caret: base_caret,
                dispatched: true,
            },
            ImeUpdate {
                ts_ms: 5,
                scenario: scenario.slug().to_string(),
                render_quality: RenderQuality::Standard,
                // Different pane → different caret rect.
                caret: CaretAnchorRect::new(40, 80, 8, 16),
                dispatched: true,
            },
        ],
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline_geometry() -> CaretGeometry {
        CaretGeometry {
            cursor_cell_col: 5,
            cursor_cell_row: 3,
            pane_top_cell: 0,
            pane_left_cell: 0,
            physical_top: 0,
            cell_width_px: 8,
            cell_height_px: 16,
            tab_bar_height_px: 24,
            padding_left_px: 4,
            padding_top_px: 2,
        }
    }

    #[test]
    fn caret_rect_baseline_math() {
        let r = compute_caret_anchor_rect(baseline_geometry());
        // x = (5 + 0) * 8 + 4 = 44
        // y = (3 + 0 - 0) * 16 + 24 + 2 = 74
        assert_eq!(r.origin_x, 44);
        assert_eq!(r.origin_y, 74);
        assert_eq!(r.width, 8);
        assert_eq!(r.height, 16);
        assert!(r.is_visible());
    }

    #[test]
    fn caret_rect_clamps_negative_cell_positions_to_zero() {
        let mut g = baseline_geometry();
        g.cursor_cell_row = -10;
        g.pane_top_cell = 0;
        g.physical_top = 0;
        let r = compute_caret_anchor_rect(g);
        // y_cell clamps to 0 → y = tab + padding only.
        assert_eq!(r.origin_y, 24 + 2);
    }

    #[test]
    fn caret_rect_handles_scroll_offset() {
        let mut g = baseline_geometry();
        g.physical_top = 100;
        g.cursor_cell_row = 105;
        let r = compute_caret_anchor_rect(g);
        // y_cell = (105 + 0 - 100) = 5 → 5 * 16 + 24 + 2 = 106
        assert_eq!(r.origin_y, 106);
    }

    #[test]
    fn caret_rect_zero_extent_is_invisible() {
        let mut g = baseline_geometry();
        g.cell_width_px = 0;
        g.cell_height_px = 0;
        let r = compute_caret_anchor_rect(g);
        assert!(!r.is_visible());
    }

    #[test]
    fn caret_rect_negative_cell_size_clamps_to_zero() {
        let mut g = baseline_geometry();
        g.cell_width_px = -42;
        g.cell_height_px = -1;
        let r = compute_caret_anchor_rect(g);
        // saturating_mul on negatives produces a negative pixel
        // value; what matters for the IME contract is that the
        // *width/height* fields stay non-negative. Verify clamp.
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
        assert!(!r.is_visible());
    }

    #[test]
    fn render_quality_must_always_dispatch() {
        for q in RenderQuality::ALL {
            assert!(
                q.must_dispatch_caret_update(),
                "{:?} reported as elide-allowed; bead correctness rule says every quality MUST dispatch",
                q
            );
        }
    }

    #[test]
    fn dispatch_fires_on_first_update() {
        let s = ImeDispatchState {
            window_screen_origin: (0, 0),
            window_size: (800, 600),
            render_quality: RenderQuality::Standard,
            was_idle: false,
        };
        let next = CaretAnchorRect::new(0, 0, 8, 16);
        assert!(should_dispatch_after_state_change(None, next, None, s));
    }

    #[test]
    fn dispatch_short_circuits_when_nothing_changed() {
        let s = ImeDispatchState {
            window_screen_origin: (10, 20),
            window_size: (800, 600),
            render_quality: RenderQuality::Standard,
            was_idle: false,
        };
        let r = CaretAnchorRect::new(40, 60, 8, 16);
        assert!(!should_dispatch_after_state_change(Some(r), r, Some(s), s));
    }

    #[test]
    fn dispatch_fires_on_window_move_even_if_caret_unchanged() {
        let r = CaretAnchorRect::new(40, 60, 8, 16);
        let mut prev = ImeDispatchState {
            window_screen_origin: (10, 20),
            window_size: (800, 600),
            render_quality: RenderQuality::Standard,
            was_idle: false,
        };
        let mut next = prev;
        next.window_screen_origin = (50, 80);
        assert!(should_dispatch_after_state_change(
            Some(r),
            r,
            Some(prev),
            next
        ));

        next = prev;
        next.window_size = (1024, 768);
        assert!(should_dispatch_after_state_change(
            Some(r),
            r,
            Some(prev),
            next
        ));

        next = prev;
        next.render_quality = RenderQuality::Draft;
        assert!(should_dispatch_after_state_change(
            Some(r),
            r,
            Some(prev),
            next
        ));

        prev.was_idle = true;
        next = prev;
        next.was_idle = false;
        assert!(should_dispatch_after_state_change(
            Some(r),
            r,
            Some(prev),
            next
        ));
    }

    #[test]
    fn ime_update_jsonl_roundtrips() {
        let updates = contract_updates(ImeScenario::LiveResize);
        let rendered = render_updates_jsonl(&updates);
        let parsed = parse_updates_jsonl(&rendered).expect("parse");
        assert_eq!(parsed, updates);
    }

    #[test]
    fn every_scenario_has_at_least_one_update_and_all_dispatched() {
        for scenario in ImeScenario::ALL {
            let updates = contract_updates(*scenario);
            assert!(!updates.is_empty(), "{:?} produced no updates", scenario);
            for u in &updates {
                assert!(
                    u.dispatched,
                    "{:?} produced an elided update; bead rule says every \
                     contract update MUST be dispatched: {u:?}",
                    scenario
                );
            }
        }
    }

    #[test]
    fn platform_metadata_is_stable() {
        assert_eq!(ImePlatform::MacosNsTextInput.slug(), "macos");
        assert_eq!(ImePlatform::WaylandTextInputV3.slug(), "wayland");
        assert_eq!(ImePlatform::X11Xim.slug(), "x11");
        assert_eq!(ImePlatform::Synthetic.slug(), "synthetic");
        assert!(!ImePlatform::MacosNsTextInput.is_wired());
        assert!(!ImePlatform::WaylandTextInputV3.is_wired());
        assert!(!ImePlatform::X11Xim.is_wired());
        assert!(ImePlatform::Synthetic.is_wired());
        assert_eq!(
            ImePlatform::MacosNsTextInput.golden_filename(ImeScenario::Typing),
            "macos-typing.jsonl"
        );
    }

    #[test]
    fn live_resize_scenario_keeps_dispatched_through_quality_transition() {
        let updates = contract_updates(ImeScenario::LiveResize);
        // The middle update is at Draft quality with cell-rect
        // unchanged from the prior Standard frame — the bug we're
        // pinning is "draft quality silently elided this dispatch".
        let middle = &updates[1];
        assert_eq!(middle.render_quality, RenderQuality::Draft);
        assert!(middle.dispatched);
    }
}
