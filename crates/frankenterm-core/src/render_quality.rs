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
pub use frankenterm_core_audit_types::input_to_photon::{
    INPUT_TO_PHOTON_CLAIM_ID, INPUT_TO_PHOTON_SCHEMA_VERSION, INPUT_TO_PHOTON_WORKLOAD_CLASS,
    InputToPhotonEvidence, InputToPhotonStage, InputToPhotonStageTrace, InputToPhotonState,
    InputToPhotonTrace, MACOS_P95_TARGET_US, MAX_INSTRUMENTATION_OVERHEAD_PCT,
    WAYLAND_P95_TARGET_US, known_key_trace_from_stage_durations, summarize_input_to_photon_traces,
    target_p95_us_for_platform, unavailable_evidence,
};

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

/// JSON schema version for the renderer SLO doctor block.
pub const RENDERER_SLOS_DOCTOR_SCHEMA_VERSION: &str = "ft.renderer-slos.doctor.v1";
/// Read-only MCP resource URI for the input-to-photon SLO status.
pub const RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI: &str =
    "wa://perf/renderer-slo/input_to_photon";
/// Read-only MCP resource URI for the SSIM parity SLO status.
pub const RENDERER_SSIM_PARITY_MCP_RESOURCE_URI: &str = "wa://perf/renderer-slo/ssim_parity";
/// Current non-claiming status for the input-to-photon SLO substrate.
pub const RENDERER_INPUT_TO_PHOTON_STATUS: &str = "stage_telemetry_substrate_wired_pending_lab_run";
/// Current non-claiming status for the SSIM parity SLO substrate.
pub const RENDERER_SSIM_PARITY_STATUS: &str =
    "ssim_oracle_corpus_wired_pending_retained_release_run";
/// Current degradation state for the SSIM parity SLO substrate.
pub const RENDERER_SSIM_PARITY_CURRENT_DEGRADATION: &str = "backend-driver-divergence";
/// macOS p95 target from `docs/perf/resize-quality-slo.json`.
pub const RENDERER_INPUT_TO_PHOTON_MACOS_P95_TARGET_US: u64 = MACOS_P95_TARGET_US;
/// Wayland p95 target from `docs/perf/resize-quality-slo.json`.
pub const RENDERER_INPUT_TO_PHOTON_WAYLAND_P95_TARGET_US: u64 = WAYLAND_P95_TARGET_US;
/// Default SSIM floor from `frankenterm-gui::gpu_regression::Thresholds`.
pub const RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM: u32 = 990_000;
/// Default maximum per-channel pixel delta from `frankenterm-gui::gpu_regression::Thresholds`.
pub const RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF: u8 = 8;
/// Default changed-pixel fraction floor in parts per million.
pub const RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM: u32 = 1_000;

/// `ft doctor --json .renderer_slos` payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererSloDoctorReport {
    pub schema_version: String,
    pub input_to_photon: RendererInputToPhotonSloStatus,
    pub ssim_parity: RendererSsimParitySloStatus,
}

/// Operator-facing status for the input-to-photon renderer SLO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererInputToPhotonSloStatus {
    pub claim_id: String,
    pub status: String,
    pub target_p95_us_macos: u64,
    pub target_p95_us_wayland: u64,
    pub max_instrumentation_overhead_pct: u64,
    pub source_bench: String,
    pub structured_log_template: String,
    pub mcp_resource_uri: String,
    pub degradation_states: Vec<String>,
    pub pending_reason: String,
}

/// Operator-facing status for the SSIM parity renderer SLO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RendererSsimParitySloStatus {
    pub claim_id: String,
    pub status: String,
    pub current_degradation: String,
    pub reference_backend: String,
    pub subject_backend: String,
    pub corpus_path: String,
    pub default_min_ssim_ppm: u32,
    pub default_max_l_inf: u8,
    pub default_max_changed_pixel_fraction_ppm: u32,
    pub comparator_source: String,
    pub source_test: String,
    pub release_gate_script: String,
    pub topology_cross_check: String,
    pub mcp_resource_uri: String,
    pub degradation_states: Vec<String>,
    pub pending_reason: String,
}

/// Build the stable renderer SLO doctor block.
///
/// This surface is deliberately non-claiming until a retained target-run
/// publishes empirical p95/p99 rows from the renderer SLO bench.
#[must_use]
pub fn renderer_slos_doctor_report() -> RendererSloDoctorReport {
    RendererSloDoctorReport {
        schema_version: RENDERER_SLOS_DOCTOR_SCHEMA_VERSION.to_string(),
        input_to_photon: RendererInputToPhotonSloStatus {
            claim_id: "renderer.input_to_photon_p95".to_string(),
            status: RENDERER_INPUT_TO_PHOTON_STATUS.to_string(),
            target_p95_us_macos: RENDERER_INPUT_TO_PHOTON_MACOS_P95_TARGET_US,
            target_p95_us_wayland: RENDERER_INPUT_TO_PHOTON_WAYLAND_P95_TARGET_US,
            max_instrumentation_overhead_pct: 5,
            source_bench: "crates/frankenterm-gui/benches/renderer_slo/input_to_photon.rs"
                .to_string(),
            structured_log_template: "target/criterion/slo-input_to_photon_<platform>.jsonl"
                .to_string(),
            mcp_resource_uri: RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI.to_string(),
            degradation_states: vec![
                "instrumentation_unavailable".to_string(),
                "photon_detection_unavailable".to_string(),
                "instrumentation_overhead_exceeded".to_string(),
                "invalid_trace".to_string(),
            ],
            pending_reason: "deterministic known-key stage telemetry substrate is wired; retained target-run empirical p95/p99 remains pending"
                .to_string(),
        },
        ssim_parity: RendererSsimParitySloStatus {
            claim_id: "renderer.ssim_parity_floor".to_string(),
            status: RENDERER_SSIM_PARITY_STATUS.to_string(),
            current_degradation: RENDERER_SSIM_PARITY_CURRENT_DEGRADATION.to_string(),
            reference_backend: "ratatui_or_recorded_oracle".to_string(),
            subject_backend: "ftui_headless_renderer".to_string(),
            corpus_path: "tests/golden/gpu".to_string(),
            default_min_ssim_ppm: RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM,
            default_max_l_inf: RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF,
            default_max_changed_pixel_fraction_ppm:
                RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM,
            comparator_source: "crates/frankenterm-gui/src/gpu_regression.rs::compare_images"
                .to_string(),
            source_test: "crates/frankenterm-gui/tests/ssim_parity.rs".to_string(),
            release_gate_script: "tests/e2e/test_ssim_parity_release_gate.sh".to_string(),
            topology_cross_check: "docs/attestations/tui/topology-parity.json".to_string(),
            mcp_resource_uri: RENDERER_SSIM_PARITY_MCP_RESOURCE_URI.to_string(),
            degradation_states: vec![
                "oracle-unavailable".to_string(),
                "backend-driver-divergence".to_string(),
                "dimension_mismatch".to_string(),
                "metric_threshold_exceeded".to_string(),
                "topology_cross_check_required".to_string(),
            ],
            pending_reason: "backend-driver oracle reaches ratatui and ftui with matched state and currently reports divergence; retained clean ratatui-vs-ftui release run remains pending"
                .to_string(),
        },
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
        assert!(h.draft_ratio().abs() <= f64::EPSILON);
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
    fn renderer_slo_doctor_report_exposes_input_to_photon_contract() {
        let report = renderer_slos_doctor_report();
        assert_eq!(report.schema_version, RENDERER_SLOS_DOCTOR_SCHEMA_VERSION);
        assert_eq!(
            report.input_to_photon.status,
            RENDERER_INPUT_TO_PHOTON_STATUS
        );
        assert_eq!(
            report.input_to_photon.mcp_resource_uri,
            RENDERER_INPUT_TO_PHOTON_MCP_RESOURCE_URI
        );
        assert_eq!(
            report.input_to_photon.target_p95_us_macos,
            RENDERER_INPUT_TO_PHOTON_MACOS_P95_TARGET_US
        );
        assert_eq!(
            report.input_to_photon.target_p95_us_wayland,
            RENDERER_INPUT_TO_PHOTON_WAYLAND_P95_TARGET_US
        );
        assert!(
            report
                .input_to_photon
                .degradation_states
                .contains(&"photon_detection_unavailable".to_string())
        );
    }

    #[test]
    fn renderer_slo_doctor_report_exposes_ssim_parity_contract() {
        let report = renderer_slos_doctor_report();
        assert_eq!(report.ssim_parity.status, RENDERER_SSIM_PARITY_STATUS);
        assert_eq!(
            report.ssim_parity.current_degradation,
            RENDERER_SSIM_PARITY_CURRENT_DEGRADATION
        );
        assert_eq!(
            report.ssim_parity.mcp_resource_uri,
            RENDERER_SSIM_PARITY_MCP_RESOURCE_URI
        );
        assert_eq!(
            report.ssim_parity.default_min_ssim_ppm,
            RENDERER_SSIM_PARITY_DEFAULT_MIN_SSIM_PPM
        );
        assert_eq!(
            report.ssim_parity.default_max_l_inf,
            RENDERER_SSIM_PARITY_DEFAULT_MAX_L_INF
        );
        assert_eq!(
            report.ssim_parity.default_max_changed_pixel_fraction_ppm,
            RENDERER_SSIM_PARITY_DEFAULT_MAX_CHANGED_PIXEL_FRACTION_PPM
        );
        assert_eq!(
            report.ssim_parity.topology_cross_check,
            "docs/attestations/tui/topology-parity.json"
        );
        assert!(
            report
                .ssim_parity
                .degradation_states
                .contains(&"oracle-unavailable".to_string())
        );
        assert!(
            report
                .ssim_parity
                .degradation_states
                .contains(&"backend-driver-divergence".to_string())
        );
    }

    #[test]
    fn input_to_photon_summary_reports_percentiles_and_target() {
        let traces = [
            known_key_trace_from_stage_durations(
                0,
                "a",
                "macos",
                [100, 200, 300, 400, 100],
                20,
                None,
                None,
            ),
            known_key_trace_from_stage_durations(
                1,
                "a",
                "macos",
                [200, 300, 400, 500, 200],
                25,
                None,
                None,
            ),
            known_key_trace_from_stage_durations(
                2,
                "a",
                "macos",
                [300, 400, 500, 600, 300],
                30,
                None,
                None,
            ),
        ];

        let evidence = summarize_input_to_photon_traces("macos", &traces);

        assert_eq!(evidence.state, InputToPhotonState::Measured);
        assert_eq!(evidence.sample_count, 3);
        assert_eq!(evidence.target_p95_us, MACOS_P95_TARGET_US);
        assert_eq!(evidence.p50_us, Some(1600));
        assert_eq!(evidence.p95_us, Some(2100));
        assert_eq!(evidence.p99_us, Some(2100));
        assert_eq!(evidence.within_target, Some(true));
        assert!(
            evidence
                .stage_breakdown_p50
                .contains_key("term_update_to_render_submit")
        );
    }

    #[test]
    fn excessive_instrumentation_overhead_degrades_input_to_photon_evidence() {
        let trace = known_key_trace_from_stage_durations(
            0,
            "a",
            "linux",
            [100, 100, 100, 100, 100],
            100,
            None,
            None,
        );

        let evidence = summarize_input_to_photon_traces("linux", &[trace]);

        assert_eq!(
            evidence.state,
            InputToPhotonState::InstrumentationOverheadExceeded
        );
        assert!(
            evidence
                .degradation_reason
                .as_deref()
                .unwrap_or_default()
                .contains("instrumentation overhead")
        );
        assert_eq!(evidence.within_target, None);
    }

    #[test]
    fn empty_input_to_photon_summary_is_degraded_not_measured() {
        let evidence = summarize_input_to_photon_traces("linux", &[]);

        assert_eq!(evidence.state, InputToPhotonState::InvalidTrace);
        assert_eq!(evidence.sample_count, 0);
        assert_eq!(evidence.p95_us, None);
        assert_eq!(evidence.within_target, None);
        assert!(
            evidence
                .degradation_reason
                .as_deref()
                .unwrap_or_default()
                .contains("no input-to-photon traces")
        );
    }

    #[test]
    fn input_to_photon_empirical_p99_agrees_with_lindley_bound() {
        use crate::network_calculus_bound::{
            ArrivalCurve, EmpiricalComparison, ServiceCurve, StageModel, TOLERANCE_PCT,
            pipeline_delay_bound,
        };

        let trace = known_key_trace_from_stage_durations(
            0,
            "a",
            "macos",
            [250, 400, 750, 600, 250],
            20,
            Some(1),
            Some("deterministic-test-adapter".to_string()),
        );
        let evidence = summarize_input_to_photon_traces("macos", std::slice::from_ref(&trace));
        let empirical_p99_ms = evidence.p99_us.expect("p99 present") as f64 / 1_000.0;
        let stages: Vec<StageModel> = trace
            .stages
            .windows(2)
            .map(|window| {
                let from = window[0].stage;
                let to = window[1].stage;
                let service_latency_ms = window[1].duration_us as f64 / 1_000.0;
                StageModel::new(
                    format!("{from}_to_{to}"),
                    ServiceCurve::new(1_000.0, service_latency_ms),
                )
            })
            .collect();
        let analytical_bound_ms = pipeline_delay_bound(ArrivalCurve::new(0.0, 1.0), &stages)
            .expect("stable input-to-photon service curve");
        let comparison = EmpiricalComparison {
            analytical_bound_ms,
            empirical_p99_ms,
        };

        assert!(
            comparison.within_tolerance(),
            "empirical p99 {empirical_p99_ms:.3}ms should stay within {TOLERANCE_PCT:.1}% of Lindley bound {analytical_bound_ms:.3}ms"
        );
        assert!(
            comparison.deviation_pct().unwrap_or(f64::INFINITY) <= 1.0,
            "deterministic known-key trace should be nearly exact"
        );
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
