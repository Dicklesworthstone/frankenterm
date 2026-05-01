//! Render-loop quality + draft-mode policy
//! ([BR-TERM-EMULATOR-UPLIFT.2.2] / `ft-mpc9b.2.2`).
//!
//! Sub-epic 2's draft-mode rendering layer. During an active
//! resize gesture, the renderer drops to a low-fidelity path —
//! bitmap glyphs only, no SDF / ligatures / italic synthesis /
//! subpixel AA / fancy underlines — so a 200-pane fleet hits 60
//! FPS even on integrated GPUs. On `ResizeEnd`, exactly one
//! full-quality frame paints (the **snap-back**), then the
//! renderer returns to its configured Standard or Fancy mode.
//!
//! ## What this module ships
//!
//! - [`RenderQuality`] — `Draft` / `Standard` / `Fancy`. The
//!   canonical home. The `ime_caret` module's forward-looking
//!   re-declaration from `ft-mpc9b.10.2` is now a re-export.
//! - [`DraftModeFeatureFlags`] — the table of what's disabled
//!   per quality. Per-feature predicates the renderer queries.
//! - [`DraftModeDriver`] — given a `LiveResizeState`, picks the
//!   appropriate `RenderQuality`. Encodes the bead's
//!   correctness rules: `Resizing → Draft`, `ResizeEnd →
//!   Standard (snap-back, exactly once)`, `Idle → configured
//!   default`.
//! - [`RenderQualityFrameEvent`] — per-frame JSONL row.
//! - [`RenderQualityHealth`] — counter snapshot for `ft doctor`,
//!   mirroring the shape of prior `*Health` types in this
//!   session.
//!
//! ## Critical invariants the bead pins (DO NOT BREAK)
//!
//! Three observable behaviors MUST be **independent** of
//! `RenderQuality`:
//!
//! 1. **A11Y tree updates fire regardless.** Cross-link
//!    `ft-mpc9b.10.1` (a11y_tree). A blind operator running
//!    VoiceOver / Orca / Narrator MUST receive announcements
//!    during a resize gesture even though the visual frame is
//!    in Draft.
//! 2. **Color profile honored regardless.** Cross-link
//!    `ft-mpc9b.10.3` (color_management). True-color cells
//!    render at the correct gamut even in Draft; only AA /
//!    decoration is dropped.
//! 3. **IME caret update fires regardless.** Cross-link
//!    `ft-mpc9b.10.2` (ime_caret). The composition window
//!    anchors to the caret cell across resize.
//!
//! All three are encoded as predicates on
//! [`DraftModeFeatureFlags`] that ALWAYS return `true`. The
//! regression fixture asserts these never silently flip.

use serde::{Deserialize, Serialize};

use crate::live_resize::LiveResizeState;

// ============================================================================
// Render quality enum
// ============================================================================

/// The closed list of render-quality modes.
///
/// `Draft` is the active-gesture path; `Standard` is the
/// steady-state default; `Fancy` enables shader effects (window-
/// effects, smoothing) for users who opt in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderQuality {
    /// Full-fidelity steady-state render.
    Standard,
    /// Shader-effects-on render (window-effects, smoothing).
    Fancy,
    /// Low-fidelity draft render used during resize gestures.
    Draft,
}

impl RenderQuality {
    /// Every quality in declaration order.
    pub const ALL: &'static [RenderQuality] = &[Self::Standard, Self::Fancy, Self::Draft];

    /// Filename slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Fancy => "fancy",
            Self::Draft => "draft",
        }
    }

    /// Whether this is the draft-mode quality.
    #[must_use]
    pub const fn is_draft(self) -> bool {
        matches!(self, Self::Draft)
    }

    /// Whether the IME caret-anchor update MUST dispatch in this
    /// quality. Always `true` per `ft-mpc9b.10.2`'s headline
    /// rule — the regression fixture pins this.
    #[must_use]
    pub const fn must_dispatch_caret_update(self) -> bool {
        true
    }
}

// ============================================================================
// Draft-mode feature flags
// ============================================================================

/// What renderer features are enabled at a given quality.
///
/// The bead enumerates 8 features that Draft disables. This
/// struct codifies them as a flat record; the renderer's
/// `glyphcache.rs` / `quad.rs` / shader uniforms query the
/// individual fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftModeFeatureFlags {
    /// SDF rendering (vs bitmap glyphs).
    pub sdf_glyphs: bool,
    /// HarfBuzz / OpenType ligature shaping.
    pub ligature_shaping: bool,
    /// Italic synthesis from non-italic font sources.
    pub italic_synthesis: bool,
    /// Subpixel anti-aliasing (vs full-pixel).
    pub subpixel_aa: bool,
    /// Fancy underline shapes (curly, dotted, double, …).
    /// Draft falls back to a single straight underline.
    pub fancy_underlines: bool,
    /// Pane border decorations beyond a single 1px line.
    pub pane_border_decorations: bool,
    /// Focus blur effect.
    pub focus_blur: bool,
    /// Background-image scaling (Draft uses 1×).
    pub background_image_scaling: bool,
    /// Whether the A11Y tree update MUST fire. Always `true`;
    /// flag exists so the renderer's gating code reads
    /// symmetrically across the bead's three independence rules.
    pub a11y_tree_update: bool,
    /// Whether color-profile application MUST run. Always `true`.
    pub color_profile: bool,
    /// Whether the IME caret-anchor update MUST fire. Always `true`.
    pub ime_caret_anchor: bool,
}

impl DraftModeFeatureFlags {
    /// Feature flags for `RenderQuality::Standard` — full
    /// fidelity, every feature on.
    #[must_use]
    pub const fn standard() -> Self {
        Self {
            sdf_glyphs: true,
            ligature_shaping: true,
            italic_synthesis: true,
            subpixel_aa: true,
            fancy_underlines: true,
            pane_border_decorations: true,
            focus_blur: false, // Off by default; Fancy turns it on.
            background_image_scaling: true,
            a11y_tree_update: true,
            color_profile: true,
            ime_caret_anchor: true,
        }
    }

    /// Feature flags for `RenderQuality::Fancy` — shader effects
    /// on top of Standard.
    #[must_use]
    pub const fn fancy() -> Self {
        let mut f = Self::standard();
        f.focus_blur = true;
        f
    }

    /// Feature flags for `RenderQuality::Draft` — the bead's
    /// load-bearing low-fidelity profile.
    ///
    /// Disables every cosmetic feature that's expensive on a
    /// 200-pane fleet during a drag, while preserving the three
    /// independence-rule features (a11y / color / ime).
    #[must_use]
    pub const fn draft() -> Self {
        Self {
            sdf_glyphs: false,
            ligature_shaping: false,
            italic_synthesis: false,
            subpixel_aa: false,
            fancy_underlines: false,
            pane_border_decorations: false,
            focus_blur: false,
            background_image_scaling: false,
            a11y_tree_update: true,
            color_profile: true,
            ime_caret_anchor: true,
        }
    }

    /// Construct flags for a given quality.
    #[must_use]
    pub const fn for_quality(quality: RenderQuality) -> Self {
        match quality {
            RenderQuality::Standard => Self::standard(),
            RenderQuality::Fancy => Self::fancy(),
            RenderQuality::Draft => Self::draft(),
        }
    }

    /// Number of cosmetic features enabled (the 8 bead features —
    /// excludes the 3 independence-rule features). Renderer's
    /// quad-budget allocator uses this to size per-frame work.
    #[must_use]
    pub const fn cosmetic_feature_count(&self) -> u32 {
        let bits = [
            self.sdf_glyphs,
            self.ligature_shaping,
            self.italic_synthesis,
            self.subpixel_aa,
            self.fancy_underlines,
            self.pane_border_decorations,
            self.focus_blur,
            self.background_image_scaling,
        ];
        let mut n = 0u32;
        let mut i = 0;
        while i < bits.len() {
            if bits[i] {
                n += 1;
            }
            i += 1;
        }
        n
    }
}

// ============================================================================
// Draft-mode driver
// ============================================================================

/// Configurable steady-state default. The integration layer's
/// config picks Standard (default) or Fancy (user opt-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteadyStateQuality {
    Standard,
    Fancy,
}

impl SteadyStateQuality {
    #[must_use]
    pub const fn as_render_quality(self) -> RenderQuality {
        match self {
            Self::Standard => RenderQuality::Standard,
            Self::Fancy => RenderQuality::Fancy,
        }
    }
}

/// Drives the renderer's quality picker from the live-resize
/// state machine's transitions. Per-tick: hand the current
/// `LiveResizeState`; the driver returns the
/// `RenderQuality` for THIS frame and tracks whether a
/// snap-back frame is owed.
#[derive(Debug, Clone)]
pub struct DraftModeDriver {
    steady_state: SteadyStateQuality,
    last_resize_state: LiveResizeState,
    /// Set when the resize state transitions away from Resizing
    /// or ResizeBegin. Cleared after the next call to `pick` so
    /// the snap-back fires exactly once.
    snap_back_owed: bool,
    /// Counters.
    draft_frames_total: u64,
    standard_frames_total: u64,
    fancy_frames_total: u64,
    snap_back_total: u64,
    quality_transitions_total: u64,
}

impl DraftModeDriver {
    /// New driver pinned to a steady-state default.
    #[must_use]
    pub fn new(steady_state: SteadyStateQuality) -> Self {
        Self {
            steady_state,
            last_resize_state: LiveResizeState::Idle,
            snap_back_owed: false,
            draft_frames_total: 0,
            standard_frames_total: 0,
            fancy_frames_total: 0,
            snap_back_total: 0,
            quality_transitions_total: 0,
        }
    }

    /// Pick the render quality for the current frame given the
    /// live-resize state.
    ///
    /// Rules per the bead:
    /// - `LiveResizeState::ResizeBegin | Resizing` → `Draft`.
    /// - `LiveResizeState::ResizeEnd` → `Standard` (the
    ///   snap-back; always Standard regardless of
    ///   `steady_state` so the user sees a guaranteed-correct
    ///   reference frame after the drag).
    /// - `LiveResizeState::Idle` → `steady_state` (Standard or
    ///   Fancy per config).
    ///
    /// Snap-back guarantee: exactly one `ResizeEnd` -> Standard
    /// frame fires per gesture, even if the integration polls
    /// `pick` multiple times while the live-resize state
    /// machine remains in `ResizeEnd` (which is unusual but
    /// possible during the auto-clear → Idle transition).
    pub fn pick(&mut self, resize_state: LiveResizeState) -> RenderQuality {
        let quality = match resize_state {
            LiveResizeState::ResizeBegin | LiveResizeState::Resizing => RenderQuality::Draft,
            LiveResizeState::ResizeEnd => {
                // Snap-back: one full-quality frame, always
                // Standard.
                if self.snap_back_owed
                    || matches!(
                        self.last_resize_state,
                        LiveResizeState::ResizeBegin | LiveResizeState::Resizing
                    )
                {
                    self.snap_back_total += 1;
                    self.snap_back_owed = false;
                }
                RenderQuality::Standard
            }
            LiveResizeState::Idle => {
                // If the prior state was a draft-mode state,
                // we missed the snap-back (the integration
                // layer skipped the ResizeEnd tick). Synthesize
                // it: this frame becomes the snap-back.
                if matches!(
                    self.last_resize_state,
                    LiveResizeState::ResizeBegin | LiveResizeState::Resizing
                ) {
                    self.snap_back_total += 1;
                    // Update bookkeeping so the NEXT Idle frame
                    // returns the steady-state instead of
                    // re-synthesizing a snap-back.
                    self.last_resize_state = LiveResizeState::Idle;
                    self.standard_frames_total += 1;
                    self.quality_transitions_total += 1;
                    return RenderQuality::Standard;
                }
                self.steady_state.as_render_quality()
            }
        };

        if quality != self.last_picked_quality_for_counters() {
            self.quality_transitions_total += 1;
        }

        // Update last-state AFTER the snap-back synthesis so the
        // next call sees ResizeEnd having already fired.
        self.last_resize_state = resize_state;

        match quality {
            RenderQuality::Draft => self.draft_frames_total += 1,
            RenderQuality::Standard => self.standard_frames_total += 1,
            RenderQuality::Fancy => self.fancy_frames_total += 1,
        }

        // Mark snap-back as owed when transitioning OUT of a
        // draft-mode state.
        if matches!(
            resize_state,
            LiveResizeState::Idle | LiveResizeState::ResizeEnd
        ) && matches!(
            self.last_resize_state,
            LiveResizeState::ResizeBegin | LiveResizeState::Resizing
        ) {
            self.snap_back_owed = true;
        }

        quality
    }

    fn last_picked_quality_for_counters(&self) -> RenderQuality {
        match self.last_resize_state {
            LiveResizeState::ResizeBegin | LiveResizeState::Resizing => RenderQuality::Draft,
            _ => self.steady_state.as_render_quality(),
        }
    }

    /// Cumulative health snapshot.
    #[must_use]
    pub fn health(&self) -> RenderQualityHealth {
        RenderQualityHealth {
            draft_frames_total: self.draft_frames_total,
            standard_frames_total: self.standard_frames_total,
            fancy_frames_total: self.fancy_frames_total,
            snap_back_total: self.snap_back_total,
            quality_transitions_total: self.quality_transitions_total,
        }
    }
}

// ============================================================================
// Per-frame event + health snapshot
// ============================================================================

/// One row of `tests/render_quality/logs/<scenario>.jsonl` per
/// the bead's structured-logging schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderQualityFrameEvent {
    pub ts_ms: u64,
    pub render_quality: RenderQuality,
    pub dirty_lines: u32,
    pub frame_time_us: u32,
    /// True iff this frame was the snap-back from a Draft
    /// gesture. Pinned in the regression fixture.
    pub is_snap_back: bool,
}

/// Cumulative health snapshot for `ft doctor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderQualityHealth {
    pub draft_frames_total: u64,
    pub standard_frames_total: u64,
    pub fancy_frames_total: u64,
    pub snap_back_total: u64,
    pub quality_transitions_total: u64,
}

impl RenderQualityHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            draft_frames_total: 0,
            standard_frames_total: 0,
            fancy_frames_total: 0,
            snap_back_total: 0,
            quality_transitions_total: 0,
        }
    }

    /// Fraction of frames spent in Draft. Steady-state typing
    /// reports 0.0; an active resize burst spikes it.
    #[must_use]
    pub fn draft_ratio(&self) -> f64 {
        let total = self.draft_frames_total + self.standard_frames_total + self.fancy_frames_total;
        if total == 0 {
            0.0
        } else {
            self.draft_frames_total as f64 / total as f64
        }
    }
}

/// Render a slice of frame events as JSONL.
#[must_use]
pub fn render_events_jsonl(events: &[RenderQualityFrameEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = serde_json::to_string(ev).expect("RenderQualityFrameEvent always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse JSONL back into events.
pub fn parse_events_jsonl(jsonl: &str) -> Result<Vec<RenderQualityFrameEvent>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_flags_have_full_fidelity() {
        let f = DraftModeFeatureFlags::standard();
        assert!(f.sdf_glyphs);
        assert!(f.ligature_shaping);
        assert!(f.italic_synthesis);
        assert!(f.subpixel_aa);
        assert!(f.fancy_underlines);
        assert!(f.pane_border_decorations);
        assert!(f.background_image_scaling);
    }

    #[test]
    fn fancy_flags_enable_focus_blur() {
        let f = DraftModeFeatureFlags::fancy();
        assert!(f.focus_blur);
    }

    #[test]
    fn draft_flags_disable_every_cosmetic() {
        let f = DraftModeFeatureFlags::draft();
        assert!(!f.sdf_glyphs);
        assert!(!f.ligature_shaping);
        assert!(!f.italic_synthesis);
        assert!(!f.subpixel_aa);
        assert!(!f.fancy_underlines);
        assert!(!f.pane_border_decorations);
        assert!(!f.focus_blur);
        assert!(!f.background_image_scaling);
    }

    #[test]
    fn independence_rules_hold_in_every_quality() {
        for q in RenderQuality::ALL {
            let f = DraftModeFeatureFlags::for_quality(*q);
            assert!(
                f.a11y_tree_update,
                "{q:?} silently disabled a11y tree updates — bead independence rule violated"
            );
            assert!(
                f.color_profile,
                "{q:?} silently disabled color profile — bead independence rule violated"
            );
            assert!(
                f.ime_caret_anchor,
                "{q:?} silently disabled IME caret anchor — bead independence rule violated"
            );
            assert!(q.must_dispatch_caret_update());
        }
    }

    #[test]
    fn cosmetic_feature_count_per_quality() {
        assert_eq!(
            DraftModeFeatureFlags::standard().cosmetic_feature_count(),
            7
        );
        assert_eq!(DraftModeFeatureFlags::fancy().cosmetic_feature_count(), 8);
        assert_eq!(DraftModeFeatureFlags::draft().cosmetic_feature_count(), 0);
    }

    #[test]
    fn driver_idle_returns_steady_state() {
        let mut d = DraftModeDriver::new(SteadyStateQuality::Standard);
        assert_eq!(d.pick(LiveResizeState::Idle), RenderQuality::Standard);
        let mut d = DraftModeDriver::new(SteadyStateQuality::Fancy);
        assert_eq!(d.pick(LiveResizeState::Idle), RenderQuality::Fancy);
    }

    #[test]
    fn driver_resizing_returns_draft() {
        let mut d = DraftModeDriver::new(SteadyStateQuality::Fancy);
        assert_eq!(d.pick(LiveResizeState::ResizeBegin), RenderQuality::Draft);
        assert_eq!(d.pick(LiveResizeState::Resizing), RenderQuality::Draft);
    }

    #[test]
    fn driver_resize_end_snaps_back_to_standard_not_steady_state() {
        let mut d = DraftModeDriver::new(SteadyStateQuality::Fancy);
        d.pick(LiveResizeState::ResizeBegin);
        d.pick(LiveResizeState::Resizing);
        // Snap-back is ALWAYS Standard, even if steady-state is
        // Fancy — the user sees a guaranteed-correct reference
        // frame.
        assert_eq!(d.pick(LiveResizeState::ResizeEnd), RenderQuality::Standard);
    }

    #[test]
    fn driver_snap_back_fires_exactly_once() {
        let mut d = DraftModeDriver::new(SteadyStateQuality::Standard);
        d.pick(LiveResizeState::ResizeBegin);
        d.pick(LiveResizeState::Resizing);
        d.pick(LiveResizeState::ResizeEnd);
        let h_after_snap_back = d.health();
        assert_eq!(h_after_snap_back.snap_back_total, 1);
        // Subsequent Idle frames must NOT re-trigger the snap-back.
        d.pick(LiveResizeState::Idle);
        d.pick(LiveResizeState::Idle);
        d.pick(LiveResizeState::Idle);
        assert_eq!(d.health().snap_back_total, 1);
    }

    #[test]
    fn driver_synthesizes_snap_back_when_resize_end_is_skipped() {
        // The integration layer might skip the ResizeEnd frame
        // (e.g., the GUI's render loop polls Idle directly
        // after Resizing → Idle auto-clear). In that case the
        // driver MUST synthesize the snap-back on the first Idle
        // frame.
        let mut d = DraftModeDriver::new(SteadyStateQuality::Fancy);
        d.pick(LiveResizeState::ResizeBegin);
        d.pick(LiveResizeState::Resizing);
        // Skip ResizeEnd — go directly to Idle.
        let snap = d.pick(LiveResizeState::Idle);
        assert_eq!(snap, RenderQuality::Standard, "synthesized snap-back");
        assert_eq!(d.health().snap_back_total, 1);
        // Subsequent Idle frames return Fancy.
        let idle_after = d.pick(LiveResizeState::Idle);
        assert_eq!(idle_after, RenderQuality::Fancy);
    }

    #[test]
    fn driver_health_counters_track_per_quality_frames() {
        let mut d = DraftModeDriver::new(SteadyStateQuality::Standard);
        d.pick(LiveResizeState::Idle);
        d.pick(LiveResizeState::ResizeBegin);
        d.pick(LiveResizeState::Resizing);
        d.pick(LiveResizeState::Resizing);
        d.pick(LiveResizeState::ResizeEnd);
        d.pick(LiveResizeState::Idle);
        let h = d.health();
        assert_eq!(h.draft_frames_total, 3);
        assert_eq!(h.standard_frames_total, 3); // 1 idle + 1 snap-back + 1 idle
        assert_eq!(h.snap_back_total, 1);
    }

    #[test]
    fn jsonl_event_roundtrip() {
        let events = vec![
            RenderQualityFrameEvent {
                ts_ms: 0,
                render_quality: RenderQuality::Standard,
                dirty_lines: 12,
                frame_time_us: 8_000,
                is_snap_back: false,
            },
            RenderQualityFrameEvent {
                ts_ms: 16,
                render_quality: RenderQuality::Draft,
                dirty_lines: 50,
                frame_time_us: 6_000,
                is_snap_back: false,
            },
            RenderQualityFrameEvent {
                ts_ms: 100,
                render_quality: RenderQuality::Standard,
                dirty_lines: 50,
                frame_time_us: 12_000,
                is_snap_back: true,
            },
        ];
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).unwrap();
        assert_eq!(parsed, events);
    }

    #[test]
    fn baseline_health_has_zero_draft_ratio() {
        let h = RenderQualityHealth::baseline();
        assert_eq!(h.draft_ratio(), 0.0);
    }

    #[test]
    fn draft_ratio_under_active_resize() {
        let h = RenderQualityHealth {
            draft_frames_total: 60,
            standard_frames_total: 40,
            fancy_frames_total: 0,
            snap_back_total: 1,
            quality_transitions_total: 2,
        };
        // 60 / 100 = 0.6
        assert!((h.draft_ratio() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn render_quality_slug_round_trip() {
        for q in RenderQuality::ALL {
            let json = serde_json::to_string(q).unwrap();
            let parsed: RenderQuality = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, *q);
        }
    }

    #[test]
    fn is_draft_predicate_only_matches_draft() {
        assert!(RenderQuality::Draft.is_draft());
        assert!(!RenderQuality::Standard.is_draft());
        assert!(!RenderQuality::Fancy.is_draft());
    }
}
