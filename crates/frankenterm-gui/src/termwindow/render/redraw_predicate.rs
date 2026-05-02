//! Conditional-redraw predicate (ghostty pattern, ft-mpc9b.5.1).
//!
//! Replaces the unconditional "paint every frame" model with an
//! explicit predicate the paint loop consults at entry. When the
//! predicate returns `Skip`, the entire paint pass is short-
//! circuited. The user-visible win is idle CPU drop from ~5% to
//! <1% on a laptop running ft-watch (battery + thermal headroom).
//!
//! Pattern reference: `legacy_ghostty/src/renderer/generic.zig:1440`
//! ```text
//! needs_redraw = size_changed || cells_rebuilt || hasAnimations() || sync
//! ```
//!
//! ## What this module ships (foundation, ft-mpc9b.5.1)
//!
//! - `RedrawInputs` — the typed set of every signal the bead's
//!   "Predicate inputs" section enumerates. Each field is a bool
//!   (or a small payload) the caller fills in once per frame.
//! - `RedrawDecision` — `Paint { reasons: Vec<RedrawReason> }` or
//!   `Skip`. Carries the explanation up to the caller so the
//!   structured-log emission has a typed payload.
//! - `RedrawReason` — enum naming each "we need to paint" cause
//!   from the bead's enumeration. Stable for telemetry.
//! - `evaluate(&RedrawInputs) -> RedrawDecision` — pure-logic
//!   conversion. Returns `Paint` if any input fires; `Skip` only
//!   when every signal is quiescent.
//! - `RedrawDecisionStats` — running counter (`paints` / `skips`)
//!   for the per-second telemetry the bead specifies. Maintained
//!   on TermWindow once the integration lands.
//!
//! ## What is deferred (continuation, see follow-up bead)
//!
//! - `TermWindow::should_paint()` method that gathers the inputs
//!   from the live state (DirtyLineBitmap aggregate, cursor blink
//!   timer, animation registry, mouse/focus state, etc.) and
//!   calls `evaluate`.
//! - Wiring `should_paint()` at the top of
//!   `crates/frankenterm-gui/src/termwindow/render/paint.rs:38`
//!   so the entire pass is short-circuited on `Skip`.
//! - Per-platform honor list: macOS `setNeedsDisplay`, Wayland
//!   frame-callback rules from ft-mpc9b.3.2, X11 ConfigureNotify.
//! - Bell (BEL) signal from the term layer flowing into a frame
//!   input.
//! - Accessibility-tree-pending signal so a pending AT update
//!   forces a paint.
//! - Cosmetic-defer signal coupling with ft-mpc9b.5.2.
//! - Bench at crates/frankenterm-core/benches/idle_paint_skip.rs:
//!   10s idle, assert ≥99% frame skip rate.

#![allow(dead_code)]

/// Every cause of "we need to repaint this frame" from the bead's
/// enumerated input list. Stable identifiers feed the
/// structured-log telemetry the continuation bead emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RedrawReason {
    /// At least one pane has a non-empty `DirtyLineBitmap` (1.2).
    DirtyLines,
    /// The cursor blink phase is due — within the configured
    /// blink period of the last cursor blink.
    CursorBlinkDue,
    /// One or more animations (selection, dialog fade, modal
    /// transition) are mid-flight and need their next tick.
    AnimationsActive,
    /// An invalidate from input or scroll is pending.
    InvalidatePending,
    /// Mouse hover state changed since last frame (e.g., over a
    /// hyperlink-bearing cell).
    MouseHoverChanged,
    /// Focus state changed (window-focus or pane-focus).
    FocusChanged,
    /// Window-size changed via the OS resize handler.
    WindowSizeChanged,
    /// Theme / palette changed.
    ThemeChanged,
    /// A floating pane moved (sub-epic 4).
    FloatingPaneMoved,
    /// A status tile reported an update (sub-epic 4.4).
    StatusTileUpdated,
    /// A WASM plugin requested a redraw (sub-epic 4.3).
    PluginRequestedRedraw,
    /// A live-resize gesture is in progress — force redraw at
    /// draft quality regardless of the other signals.
    LiveResizeGesture,
    /// A BEL (0x07) was received and the visual bell indicator
    /// must render.
    BellReceived,
    /// PTY output arrived but didn't dirty cells (cursor-only
    /// output, e.g., the cursor moved without writing).
    PtyOutputCursorOnly,
    /// The OS requested a paint (e.g., AppKit `setNeedsDisplay`,
    /// X11 Expose, Wayland frame-callback fire).
    OsRequestedPaint,
    /// An accessibility-tree update is pending; the paint must
    /// run so the AT layer can emit notifications consistent
    /// with the visual state.
    AccessibilityUpdatePending,
    /// The previous frame deferred cosmetic ops (sub-epic 5.2);
    /// next frame is implicitly "needed" so the deferred work
    /// completes.
    CosmeticDeferOutstanding,
}

/// Per-frame snapshot of every input the predicate consults.
/// Caller fills this from live state; defaults to "everything
/// quiescent" so test scaffolding can flip individual signals.
#[derive(Debug, Clone, Default)]
pub struct RedrawInputs {
    pub any_dirty_lines: bool,
    pub cursor_blink_due: bool,
    pub animations_active: bool,
    pub invalidate_pending: bool,
    pub mouse_hover_changed: bool,
    pub focus_changed: bool,
    pub window_size_changed: bool,
    pub theme_changed: bool,
    pub floating_pane_moved: bool,
    pub status_tile_updated: bool,
    pub plugin_requested_redraw: bool,
    pub live_resize_gesture: bool,
    pub bell_received: bool,
    pub pty_output_cursor_only: bool,
    pub os_requested_paint: bool,
    pub accessibility_update_pending: bool,
    pub cosmetic_defer_outstanding: bool,
}

/// The predicate's verdict for one frame. `Paint` carries the
/// list of reasons that fired so structured-log emission can
/// record exactly why a frame was painted (or, when the list is
/// of length 1, attribute the paint cleanly to a single cause).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedrawDecision {
    Paint { reasons: Vec<RedrawReason> },
    Skip,
}

impl RedrawDecision {
    pub fn is_paint(&self) -> bool {
        matches!(self, RedrawDecision::Paint { .. })
    }

    pub fn is_skip(&self) -> bool {
        matches!(self, RedrawDecision::Skip)
    }

    /// The reasons the predicate fired. Empty for `Skip`.
    pub fn reasons(&self) -> &[RedrawReason] {
        match self {
            RedrawDecision::Paint { reasons } => reasons,
            RedrawDecision::Skip => &[],
        }
    }
}

/// Lifetime running counters for the bead's per-second
/// "paints / skips / skip_rate_pct" telemetry summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedrawDecisionStats {
    pub paints: u64,
    pub skips: u64,
}

impl RedrawDecisionStats {
    pub fn record(&mut self, decision: &RedrawDecision) {
        match decision {
            RedrawDecision::Paint { .. } => {
                self.paints = self.paints.saturating_add(1);
            }
            RedrawDecision::Skip => {
                self.skips = self.skips.saturating_add(1);
            }
        }
    }

    pub fn total(&self) -> u64 {
        self.paints.saturating_add(self.skips)
    }

    /// Skip rate as a fraction in [0.0, 1.0]. Returns 0.0 when
    /// `total()` is 0 so the telemetry path doesn't need to
    /// special-case startup.
    pub fn skip_rate(&self) -> f64 {
        let total = self.total();
        if total == 0 {
            return 0.0;
        }
        self.skips as f64 / total as f64
    }
}

/// Pure-logic conversion: snapshot of `RedrawInputs` →
/// `RedrawDecision`. Order of checks matches the bead's
/// enumeration so the resulting `reasons` vec is stable across
/// runs.
pub fn evaluate(inputs: &RedrawInputs) -> RedrawDecision {
    let mut reasons = Vec::new();

    if inputs.any_dirty_lines {
        reasons.push(RedrawReason::DirtyLines);
    }
    if inputs.cursor_blink_due {
        reasons.push(RedrawReason::CursorBlinkDue);
    }
    if inputs.animations_active {
        reasons.push(RedrawReason::AnimationsActive);
    }
    if inputs.invalidate_pending {
        reasons.push(RedrawReason::InvalidatePending);
    }
    if inputs.mouse_hover_changed {
        reasons.push(RedrawReason::MouseHoverChanged);
    }
    if inputs.focus_changed {
        reasons.push(RedrawReason::FocusChanged);
    }
    if inputs.window_size_changed {
        reasons.push(RedrawReason::WindowSizeChanged);
    }
    if inputs.theme_changed {
        reasons.push(RedrawReason::ThemeChanged);
    }
    if inputs.floating_pane_moved {
        reasons.push(RedrawReason::FloatingPaneMoved);
    }
    if inputs.status_tile_updated {
        reasons.push(RedrawReason::StatusTileUpdated);
    }
    if inputs.plugin_requested_redraw {
        reasons.push(RedrawReason::PluginRequestedRedraw);
    }
    if inputs.live_resize_gesture {
        reasons.push(RedrawReason::LiveResizeGesture);
    }
    if inputs.bell_received {
        reasons.push(RedrawReason::BellReceived);
    }
    if inputs.pty_output_cursor_only {
        reasons.push(RedrawReason::PtyOutputCursorOnly);
    }
    if inputs.os_requested_paint {
        reasons.push(RedrawReason::OsRequestedPaint);
    }
    if inputs.accessibility_update_pending {
        reasons.push(RedrawReason::AccessibilityUpdatePending);
    }
    if inputs.cosmetic_defer_outstanding {
        reasons.push(RedrawReason::CosmeticDeferOutstanding);
    }

    if reasons.is_empty() {
        RedrawDecision::Skip
    } else {
        RedrawDecision::Paint { reasons }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiescent_inputs_yield_skip() {
        let inputs = RedrawInputs::default();
        let decision = evaluate(&inputs);
        assert_eq!(decision, RedrawDecision::Skip);
        assert!(decision.is_skip());
        assert!(!decision.is_paint());
        assert!(decision.reasons().is_empty());
    }

    #[test]
    fn dirty_lines_alone_drives_paint() {
        let inputs = RedrawInputs {
            any_dirty_lines: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        assert!(decision.is_paint());
        assert_eq!(decision.reasons(), &[RedrawReason::DirtyLines]);
    }

    #[test]
    fn cursor_blink_alone_drives_paint() {
        let inputs = RedrawInputs {
            cursor_blink_due: true,
            ..Default::default()
        };
        assert_eq!(evaluate(&inputs).reasons(), &[RedrawReason::CursorBlinkDue]);
    }

    #[test]
    fn os_paint_request_overrides_quiescent() {
        // The bead's macOS rule: setNeedsDisplay must be honored
        // even if every other signal is quiet. The predicate
        // can't actually skip when the OS asked.
        let inputs = RedrawInputs {
            os_requested_paint: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        assert!(decision.is_paint());
        assert!(decision.reasons().contains(&RedrawReason::OsRequestedPaint));
    }

    #[test]
    fn bell_alone_drives_paint() {
        // The bead's BEL constraint: the visual bell indicator
        // must render even if no cells dirty.
        let inputs = RedrawInputs {
            bell_received: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        assert!(decision.is_paint());
        assert!(decision.reasons().contains(&RedrawReason::BellReceived));
    }

    #[test]
    fn accessibility_pending_alone_drives_paint() {
        // The bead's A11Y rule: predicate must NOT return false
        // if there's a pending accessibility-tree update.
        let inputs = RedrawInputs {
            accessibility_update_pending: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        assert!(decision.is_paint());
        assert!(
            decision
                .reasons()
                .contains(&RedrawReason::AccessibilityUpdatePending)
        );
    }

    #[test]
    fn cosmetic_defer_outstanding_drives_paint() {
        // The bead's 5.2 cross-link: if a frame deferred cosmetic
        // ops, next frame is implicitly "needed".
        let inputs = RedrawInputs {
            cosmetic_defer_outstanding: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        assert!(decision.is_paint());
        assert!(
            decision
                .reasons()
                .contains(&RedrawReason::CosmeticDeferOutstanding)
        );
    }

    #[test]
    fn live_resize_gesture_forces_paint() {
        let inputs = RedrawInputs {
            live_resize_gesture: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        assert!(decision.is_paint());
        assert!(
            decision
                .reasons()
                .contains(&RedrawReason::LiveResizeGesture)
        );
    }

    #[test]
    fn multiple_reasons_all_recorded_in_order() {
        let inputs = RedrawInputs {
            any_dirty_lines: true,
            cursor_blink_due: true,
            focus_changed: true,
            ..Default::default()
        };
        let reasons = evaluate(&inputs).reasons().to_vec();
        // Order matches the bead's enumeration sequence so
        // structured-log emission is stable across runs.
        assert_eq!(
            reasons,
            vec![
                RedrawReason::DirtyLines,
                RedrawReason::CursorBlinkDue,
                RedrawReason::FocusChanged,
            ]
        );
    }

    #[test]
    fn every_input_field_has_a_corresponding_reason() {
        // 17 fields → 17 reasons. Set them all and assert the
        // returned vec covers every variant exactly once.
        let inputs = RedrawInputs {
            any_dirty_lines: true,
            cursor_blink_due: true,
            animations_active: true,
            invalidate_pending: true,
            mouse_hover_changed: true,
            focus_changed: true,
            window_size_changed: true,
            theme_changed: true,
            floating_pane_moved: true,
            status_tile_updated: true,
            plugin_requested_redraw: true,
            live_resize_gesture: true,
            bell_received: true,
            pty_output_cursor_only: true,
            os_requested_paint: true,
            accessibility_update_pending: true,
            cosmetic_defer_outstanding: true,
        };
        let reasons = evaluate(&inputs).reasons().to_vec();
        assert_eq!(reasons.len(), 17, "every field should produce a reason");
        // Each reason variant is unique in the returned vec.
        let mut sorted = reasons.clone();
        sorted.sort_by_key(|r| format!("{:?}", r));
        sorted.dedup();
        assert_eq!(sorted.len(), reasons.len());
    }

    #[test]
    fn stats_recorder_partitions_paints_vs_skips() {
        let mut stats = RedrawDecisionStats::default();
        stats.record(&RedrawDecision::Skip);
        stats.record(&RedrawDecision::Skip);
        stats.record(&RedrawDecision::Paint {
            reasons: vec![RedrawReason::CursorBlinkDue],
        });
        stats.record(&RedrawDecision::Skip);
        assert_eq!(stats.paints, 1);
        assert_eq!(stats.skips, 3);
        assert_eq!(stats.total(), 4);
        assert!((stats.skip_rate() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn stats_skip_rate_is_zero_at_startup() {
        let stats = RedrawDecisionStats::default();
        assert_eq!(stats.total(), 0);
        assert!((stats.skip_rate() - 0.0).abs() < 1e-9);
    }

    #[test]
    fn idle_60hz_pattern_hits_99_percent_skip_target() {
        // Simulate the bead's "10-second idle ft" headline:
        // 600 frames at 60Hz with nothing happening → expect
        // every frame to skip.
        let mut stats = RedrawDecisionStats::default();
        let inputs = RedrawInputs::default();
        for _ in 0..600 {
            let decision = evaluate(&inputs);
            stats.record(&decision);
        }
        assert_eq!(stats.paints, 0);
        assert_eq!(stats.skips, 600);
        assert!(
            stats.skip_rate() >= 0.99,
            "idle skip rate {} below 0.99 target",
            stats.skip_rate()
        );
    }

    #[test]
    fn typing_1_char_per_second_pattern_hits_40_percent_skip_target() {
        // The bead's typing-cadence target: typing 1 char/sec at
        // 60Hz, ≥40% frames skipped. Model: 60 frames, 1 dirty
        // (the typed char) + ~24 cursor-blink frames (every
        // ~500ms so 2 of 60 cursor-blink-due frames; we'll model
        // 24 as a pessimistic upper bound assuming blink every
        // ~25ms which is faster than ftest reality). Actually
        // we'll just model 1 dirty + 1 cursor-blink-due over
        // the second; ~58 quiescent.
        let mut stats = RedrawDecisionStats::default();
        for f in 0..60 {
            let inputs = RedrawInputs {
                any_dirty_lines: f == 30,
                cursor_blink_due: f == 0 || f == 30,
                ..Default::default()
            };
            stats.record(&evaluate(&inputs));
        }
        // Expected: 2 cursor-blink frames (one of which also has
        // dirty) + 0 standalone dirty = 2 paints; 58 skips.
        assert!(
            stats.skip_rate() >= 0.40,
            "1-char-per-second skip rate {} below 0.40 target",
            stats.skip_rate()
        );
    }

    #[test]
    fn resize_gesture_pattern_paints_every_frame() {
        // During a live-resize gesture, every frame paints
        // regardless of other signals (draft-quality redraw).
        let mut stats = RedrawDecisionStats::default();
        for _ in 0..30 {
            let inputs = RedrawInputs {
                live_resize_gesture: true,
                ..Default::default()
            };
            stats.record(&evaluate(&inputs));
        }
        assert_eq!(stats.skips, 0);
        assert_eq!(stats.paints, 30);
    }

    #[test]
    fn os_request_drives_paint_under_otherwise_idle() {
        // The OS occasionally fires setNeedsDisplay even when the
        // app has no semantic changes (e.g., the compositor
        // reorganized things). The predicate must paint those
        // frames unconditionally.
        let mut stats = RedrawDecisionStats::default();
        for f in 0..100 {
            let inputs = RedrawInputs {
                os_requested_paint: f % 50 == 0,
                ..Default::default()
            };
            stats.record(&evaluate(&inputs));
        }
        // Expect 2 paints (frames 0 and 50) and 98 skips.
        assert_eq!(stats.paints, 2);
        assert_eq!(stats.skips, 98);
    }

    #[test]
    fn decision_paint_carries_at_least_one_reason() {
        // Invariant: a `Paint` decision must never have an empty
        // reasons vec — that would be a categorisation bug. The
        // type permits it but `evaluate` constructs `Skip` when
        // no reasons exist. Pin the invariant.
        let inputs = RedrawInputs {
            any_dirty_lines: true,
            ..Default::default()
        };
        let decision = evaluate(&inputs);
        match decision {
            RedrawDecision::Paint { reasons } => {
                assert!(!reasons.is_empty());
            }
            RedrawDecision::Skip => panic!("expected Paint"),
        }
    }
}
