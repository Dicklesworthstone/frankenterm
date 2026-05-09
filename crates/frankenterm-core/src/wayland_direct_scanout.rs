//! Wayland direct-scanout policy substrate (ft-2okh0.2.2).
//!
//! Pure-logic substrate for the bead's "Direct scanout (Wayland
//! fullscreen) — zero compositor copies" requirement. The
//! integration crate handles the actual `linux-dmabuf` /
//! `xdg-shell` / `zwlr_layer_shell` protocol bindings; this
//! module ships:
//!
//! - The bead's per-compositor support matrix.
//! - Buffer-format negotiation policy with bead-cited fallback
//!   priority (BGRA8 first, then RGBA8, then whatever the
//!   compositor offers).
//! - The 4-cause fallback decision tree (cursor-overlay /
//!   partial-occlusion / format-mismatch / driver-bug).
//! - VRR + recording compose-with hooks (cross-link
//!   ft-2okh0.2.1 / display_pipeline.rs and
//!   ft-0thg2 / display_pipeline_ci_matrix.rs).
//! - Telemetry counters per the bead's structured-logging
//!   spec (active rate / fallback breakdown / latency win).
//!
//! ## What this module ships
//!
//! - [`WaylandCompositor`] 5-variant (`Mutter / Kwin / Sway /
//!   Hyprland / Weston`) covering the bead's support matrix.
//! - [`ScanoutSupport`] — per-compositor support level
//!   (`Native / NotSupported / Unknown`).
//! - [`BufferFormat`] 4-variant (`Bgra8 / Rgba8 / Bgra1010102
//!   / Other`) — bead's "BGRA8/RGBA8/etc. per display" list.
//! - [`negotiate_buffer_format`] — pure decision over
//!   (compositor-advertised list, ft preferences) returning
//!   the chosen format or `None` when no match.
//! - [`ScanoutFallback`] 4-variant covering the bead's
//!   "Concrete failure modes" (cursor / occlusion / format /
//!   driver-bug).
//! - [`DirectScanoutDecision`] (`Active{format} / Fallback{cause} /
//!   NotEligible`).
//! - [`evaluate_direct_scanout`] — pure decision tree composing
//!   compositor support + fullscreen state + cursor-overlay
//!   detected + partial-occlusion detected + format-negotiate
//!   result + driver-flag.
//! - [`DirectScanoutTelemetry`] — bead's per-session counters
//!   (frames_scanout / frames_fallback_by_cause /
//!   latency_win_us_total).
//!
//! ## What is deferred to ft-2okh0.2.2.cont
//!
//! - `linux-dmabuf-v1` protocol binding (Wayland Smithay /
//!   wayland-rs).
//! - `xdg-shell` fullscreen + `zwlr_layer_shell` opt-in.
//! - Compositor identification at startup (parse
//!   `XDG_CURRENT_DESKTOP` + `WAYLAND_DISPLAY` + adapter
//!   probe — cross-link ft-zgb5v / ft-0thg2.cont's platform
//!   probes).
//! - Cursor-overlay / partial-occlusion runtime detection
//!   (compositor reports via `wp_presentation` or the
//!   integration's own scene-graph tracking).
//! - DRM driver-bug allowlist (per-vendor known-broken-driver
//!   matrix).
//! - JSON-line emission to `tests/scanout/logs/<scenario>.jsonl`.

#![allow(dead_code)]

// ============================================================================
// Compositor support matrix
// ============================================================================

/// Per the bead's support matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaylandCompositor {
    /// GNOME's Mutter — direct scanout since GNOME 42.
    Mutter,
    /// KDE's KWin — direct scanout since 5.27.
    Kwin,
    /// Sway (wlroots) — full support.
    Sway,
    /// Hyprland (wlroots).
    Hyprland,
    /// Weston (reference Wayland compositor).
    Weston,
}

impl WaylandCompositor {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mutter => "mutter",
            Self::Kwin => "kwin",
            Self::Sway => "sway",
            Self::Hyprland => "hyprland",
            Self::Weston => "weston",
        }
    }

    /// Per the bead's matrix, all listed compositors support
    /// direct scanout (with version caveats noted in the doc).
    /// Used by the integration's startup probe.
    #[must_use]
    pub const fn known_supports_scanout(self) -> bool {
        match self {
            Self::Mutter | Self::Kwin | Self::Sway | Self::Hyprland | Self::Weston => true,
        }
    }

    /// Iterate all 5 compositors in stable order — for the
    /// integration's CI matrix.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Mutter,
            Self::Kwin,
            Self::Sway,
            Self::Hyprland,
            Self::Weston,
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ScanoutSupport {
    /// Compositor's protocol implementation supports
    /// `linux-dmabuf-v1` + scanout.
    Native,
    /// Compositor is known not to support scanout (e.g.,
    /// older KWin versions). Substrate forces fallback.
    NotSupported,
    /// Probe inconclusive; substrate treats as Unknown and
    /// the integration's runtime feature query decides.
    #[default]
    Unknown,
}

// ============================================================================
// Buffer format
// ============================================================================

/// Direct-scanout-capable buffer formats per the bead. The
/// integration's format-negotiation walks the compositor's
/// advertised list and matches against this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferFormat {
    /// `DRM_FORMAT_ARGB8888` — most common modern format.
    Bgra8,
    /// `DRM_FORMAT_ABGR8888`.
    Rgba8,
    /// `DRM_FORMAT_ARGB2101010` — 10-bit-per-channel HDR.
    Bgra1010102,
    /// Anything else the compositor supports — substrate
    /// records but the integration may decline.
    Other { drm_fourcc: u32 },
}

impl BufferFormat {
    /// Bytes per pixel for atlas-size calculations.
    #[must_use]
    pub const fn bytes_per_pixel(self) -> u32 {
        match self {
            Self::Bgra8 | Self::Rgba8 => 4,
            Self::Bgra1010102 => 4,
            Self::Other { .. } => 0, // unknown — caller asks the integration
        }
    }

    /// Whether substrate has a known label for this format
    /// (used by the integration's preference-ordered match).
    #[must_use]
    pub const fn is_well_known(self) -> bool {
        !matches!(self, Self::Other { .. })
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bgra8 => "bgra8",
            Self::Rgba8 => "rgba8",
            Self::Bgra1010102 => "bgra1010102",
            Self::Other { .. } => "other",
        }
    }
}

/// ft's preference order: `Bgra8` first (most-common), then
/// `Rgba8`, then 10-bit HDR. The integration walks this slice
/// and picks the first match in the compositor's advertised
/// formats.
pub const FT_FORMAT_PREFERENCE: &[BufferFormat] = &[
    BufferFormat::Bgra8,
    BufferFormat::Rgba8,
    BufferFormat::Bgra1010102,
];

/// Pure decision: pick the highest-preference format that the
/// compositor advertises. Returns `None` when no preferred
/// format is supported (forces fallback to standard Present).
#[must_use]
pub fn negotiate_buffer_format(compositor_advertised: &[BufferFormat]) -> Option<BufferFormat> {
    for preferred in FT_FORMAT_PREFERENCE {
        for advertised in compositor_advertised {
            if std::mem::discriminant(preferred) == std::mem::discriminant(advertised) {
                return Some(*preferred);
            }
        }
    }
    None
}

// ============================================================================
// Fallback decision tree
// ============================================================================

/// Per the bead's "Concrete failure modes" section. When
/// scanout isn't possible, substrate emits one of these so the
/// integration knows why and can log/diagnose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanoutFallback {
    /// Compositor must overlay the cursor — cannot scan out
    /// the bare buffer. Bead: "Acceptable; latency win still
    /// partial."
    CursorOverlay,
    /// A notification or other surface partially occludes ft —
    /// compositor must composite. Bead: "Acceptable."
    PartialOcclusion,
    /// No buffer format both compositor and ft support.
    FormatNegotiationFailed,
    /// DRM driver flagged as known-broken (vendor allowlist).
    /// Bead: "DRM driver bug."
    DriverBug,
    /// Window not in fullscreen mode — scanout requires
    /// xdg-shell fullscreen + layer-shell.
    NotFullscreen,
    /// Compositor doesn't support scanout (`ScanoutSupport::
    /// NotSupported` or `Unknown`).
    CompositorUnsupported,
}

impl ScanoutFallback {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CursorOverlay => "cursor_overlay",
            Self::PartialOcclusion => "partial_occlusion",
            Self::FormatNegotiationFailed => "format_negotiation_failed",
            Self::DriverBug => "driver_bug",
            Self::NotFullscreen => "not_fullscreen",
            Self::CompositorUnsupported => "compositor_unsupported",
        }
    }

    /// Whether this fallback represents an "acceptable"
    /// degradation per the bead (cursor + partial occlusion)
    /// vs an unrecoverable failure mode.
    #[must_use]
    pub const fn is_acceptable_per_bead(self) -> bool {
        matches!(self, Self::CursorOverlay | Self::PartialOcclusion)
    }
}

// ============================================================================
// Inputs + decision
// ============================================================================

/// Per-frame inputs to `evaluate_direct_scanout`. The
/// integration assembles this from compositor probes + scene
/// graph + DRM driver state.
#[derive(Debug, Clone)]
pub struct ScanoutInputs {
    pub compositor: WaylandCompositor,
    pub support: ScanoutSupport,
    pub fullscreen: bool,
    pub cursor_overlay_required: bool,
    pub partial_occlusion: bool,
    pub driver_known_broken: bool,
    /// Compositor-advertised buffer formats from
    /// `linux-dmabuf-v1`. Empty slice ⇒ no formats supported.
    pub compositor_advertised: Vec<BufferFormat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirectScanoutDecision {
    /// Compositor will scan out ft's buffer directly using
    /// `format`. Integration submits via `linux-dmabuf-v1`.
    Active { format: BufferFormat },
    /// Substrate refuses scanout for `cause`. Integration
    /// uses standard Present.
    Fallback { cause: ScanoutFallback },
}

/// Pure decision tree. Order matters:
///
/// 1. Not fullscreen → `Fallback::NotFullscreen` (the bead
///    requires xdg-shell fullscreen for opt-in scanout).
/// 2. Compositor support is `NotSupported` → `Fallback::
///    CompositorUnsupported`.
/// 3. Compositor support is `Unknown` → also Fallback (the
///    integration's runtime feature query may try later;
///    substrate stays conservative).
/// 4. Driver flagged broken → `Fallback::DriverBug`.
/// 5. Cursor overlay required → `Fallback::CursorOverlay`.
/// 6. Partial occlusion → `Fallback::PartialOcclusion`.
/// 7. Format negotiation: pick from the compositor's
///    advertised list per ft preference. None ⇒
///    `Fallback::FormatNegotiationFailed`.
/// 8. All gates pass → `Active{format}`.
#[must_use]
pub fn evaluate_direct_scanout(inputs: &ScanoutInputs) -> DirectScanoutDecision {
    if !inputs.fullscreen {
        return DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::NotFullscreen,
        };
    }
    match inputs.support {
        ScanoutSupport::Native => {}
        ScanoutSupport::NotSupported | ScanoutSupport::Unknown => {
            return DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CompositorUnsupported,
            };
        }
    }
    if inputs.driver_known_broken {
        return DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::DriverBug,
        };
    }
    if inputs.cursor_overlay_required {
        return DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::CursorOverlay,
        };
    }
    if inputs.partial_occlusion {
        return DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::PartialOcclusion,
        };
    }
    match negotiate_buffer_format(&inputs.compositor_advertised) {
        Some(format) => DirectScanoutDecision::Active { format },
        None => DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::FormatNegotiationFailed,
        },
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirectScanoutTelemetry {
    pub frames_scanout_active: u64,
    pub frames_fallback_total: u64,
    pub fallback_cursor_overlay: u64,
    pub fallback_partial_occlusion: u64,
    pub fallback_format_negotiation_failed: u64,
    pub fallback_driver_bug: u64,
    pub fallback_not_fullscreen: u64,
    pub fallback_compositor_unsupported: u64,
    /// Sum of latency-win microseconds. Compositor reports
    /// the savings via `wp_presentation`'s
    /// `presented_at - submitted_at` delta in scanout vs the
    /// composited path; the integration accumulates here.
    pub latency_win_us_total: u64,
}

impl DirectScanoutTelemetry {
    pub fn record_decision(&mut self, decision: DirectScanoutDecision) {
        match decision {
            DirectScanoutDecision::Active { .. } => {
                self.frames_scanout_active = self.frames_scanout_active.saturating_add(1);
            }
            DirectScanoutDecision::Fallback { cause } => {
                self.frames_fallback_total = self.frames_fallback_total.saturating_add(1);
                let slot = match cause {
                    ScanoutFallback::CursorOverlay => &mut self.fallback_cursor_overlay,
                    ScanoutFallback::PartialOcclusion => &mut self.fallback_partial_occlusion,
                    ScanoutFallback::FormatNegotiationFailed => {
                        &mut self.fallback_format_negotiation_failed
                    }
                    ScanoutFallback::DriverBug => &mut self.fallback_driver_bug,
                    ScanoutFallback::NotFullscreen => &mut self.fallback_not_fullscreen,
                    ScanoutFallback::CompositorUnsupported => {
                        &mut self.fallback_compositor_unsupported
                    }
                };
                *slot = slot.saturating_add(1);
            }
        }
    }

    pub fn record_latency_win(&mut self, microseconds: u64) {
        self.latency_win_us_total = self.latency_win_us_total.saturating_add(microseconds);
    }

    /// Scanout-active rate as integer percent `[0..=100]`.
    /// Returns 0 when no frames have been recorded.
    #[must_use]
    pub fn scanout_active_rate_pct(&self) -> u32 {
        let total = self.frames_scanout_active + self.frames_fallback_total;
        if total == 0 {
            return 0;
        }
        ((self.frames_scanout_active * 100) / total).min(100) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fullscreen_inputs(compositor: WaylandCompositor) -> ScanoutInputs {
        ScanoutInputs {
            compositor,
            support: ScanoutSupport::Native,
            fullscreen: true,
            cursor_overlay_required: false,
            partial_occlusion: false,
            driver_known_broken: false,
            compositor_advertised: vec![BufferFormat::Bgra8, BufferFormat::Rgba8],
        }
    }

    // ----------------------------------------------------------------
    // WaylandCompositor
    // ----------------------------------------------------------------

    #[test]
    fn compositor_label_stable() {
        assert_eq!(WaylandCompositor::Mutter.label(), "mutter");
        assert_eq!(WaylandCompositor::Kwin.label(), "kwin");
        assert_eq!(WaylandCompositor::Sway.label(), "sway");
        assert_eq!(WaylandCompositor::Hyprland.label(), "hyprland");
        assert_eq!(WaylandCompositor::Weston.label(), "weston");
    }

    #[test]
    fn compositor_all_supports_scanout_per_bead_matrix() {
        for c in WaylandCompositor::all() {
            assert!(
                c.known_supports_scanout(),
                "{c:?} should support scanout per bead matrix"
            );
        }
    }

    #[test]
    fn compositor_all_returns_five() {
        assert_eq!(WaylandCompositor::all().len(), 5);
    }

    // ----------------------------------------------------------------
    // ScanoutSupport
    // ----------------------------------------------------------------

    #[test]
    fn support_default_is_unknown() {
        assert_eq!(ScanoutSupport::default(), ScanoutSupport::Unknown);
    }

    // ----------------------------------------------------------------
    // BufferFormat
    // ----------------------------------------------------------------

    #[test]
    fn format_known_bytes_per_pixel() {
        assert_eq!(BufferFormat::Bgra8.bytes_per_pixel(), 4);
        assert_eq!(BufferFormat::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(BufferFormat::Bgra1010102.bytes_per_pixel(), 4);
    }

    #[test]
    fn format_well_known_classification() {
        assert!(BufferFormat::Bgra8.is_well_known());
        assert!(BufferFormat::Rgba8.is_well_known());
        assert!(BufferFormat::Bgra1010102.is_well_known());
        assert!(
            !BufferFormat::Other {
                drm_fourcc: 0xDEADBEEF
            }
            .is_well_known()
        );
    }

    // ----------------------------------------------------------------
    // negotiate_buffer_format
    // ----------------------------------------------------------------

    #[test]
    fn negotiate_picks_first_preference_when_available() {
        let advertised = vec![BufferFormat::Bgra8, BufferFormat::Rgba8];
        assert_eq!(
            negotiate_buffer_format(&advertised),
            Some(BufferFormat::Bgra8)
        );
    }

    #[test]
    fn negotiate_falls_back_when_first_unavailable() {
        let advertised = vec![BufferFormat::Rgba8, BufferFormat::Bgra1010102];
        assert_eq!(
            negotiate_buffer_format(&advertised),
            Some(BufferFormat::Rgba8)
        );
    }

    #[test]
    fn negotiate_picks_10bit_when_only_one_available() {
        let advertised = vec![BufferFormat::Bgra1010102];
        assert_eq!(
            negotiate_buffer_format(&advertised),
            Some(BufferFormat::Bgra1010102)
        );
    }

    #[test]
    fn negotiate_returns_none_when_no_match() {
        let advertised = vec![BufferFormat::Other {
            drm_fourcc: 0x12345678,
        }];
        assert_eq!(negotiate_buffer_format(&advertised), None);
    }

    #[test]
    fn negotiate_returns_none_for_empty_advertised_list() {
        assert_eq!(negotiate_buffer_format(&[]), None);
    }

    // ----------------------------------------------------------------
    // ScanoutFallback
    // ----------------------------------------------------------------

    #[test]
    fn fallback_acceptable_classifications() {
        assert!(ScanoutFallback::CursorOverlay.is_acceptable_per_bead());
        assert!(ScanoutFallback::PartialOcclusion.is_acceptable_per_bead());
        assert!(!ScanoutFallback::FormatNegotiationFailed.is_acceptable_per_bead());
        assert!(!ScanoutFallback::DriverBug.is_acceptable_per_bead());
        assert!(!ScanoutFallback::NotFullscreen.is_acceptable_per_bead());
        assert!(!ScanoutFallback::CompositorUnsupported.is_acceptable_per_bead());
    }

    // ----------------------------------------------------------------
    // evaluate_direct_scanout — decision tree priority
    // ----------------------------------------------------------------

    #[test]
    fn scanout_active_when_all_gates_pass() {
        let inputs = fullscreen_inputs(WaylandCompositor::Sway);
        match evaluate_direct_scanout(&inputs) {
            DirectScanoutDecision::Active { format } => {
                assert_eq!(format, BufferFormat::Bgra8);
            }
            other @ DirectScanoutDecision::Fallback { .. } => {
                panic!("expected Active; got {other:?}")
            }
        }
    }

    #[test]
    fn scanout_falls_back_when_not_fullscreen() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.fullscreen = false;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::NotFullscreen
            },
        );
    }

    #[test]
    fn scanout_falls_back_when_compositor_unsupported() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Mutter);
        inputs.support = ScanoutSupport::NotSupported;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CompositorUnsupported,
            },
        );
    }

    #[test]
    fn scanout_falls_back_when_support_unknown() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Mutter);
        inputs.support = ScanoutSupport::Unknown;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CompositorUnsupported,
            },
        );
    }

    #[test]
    fn scanout_falls_back_on_driver_bug() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.driver_known_broken = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::DriverBug
            },
        );
    }

    #[test]
    fn scanout_falls_back_on_cursor_overlay() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.cursor_overlay_required = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CursorOverlay
            },
        );
    }

    #[test]
    fn scanout_falls_back_on_partial_occlusion() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.partial_occlusion = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::PartialOcclusion
            },
        );
    }

    #[test]
    fn scanout_falls_back_on_format_negotiation_failure() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.compositor_advertised = vec![BufferFormat::Other { drm_fourcc: 0x99 }];
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::FormatNegotiationFailed,
            },
        );
    }

    #[test]
    fn scanout_priority_not_fullscreen_beats_other_gates() {
        // Even with everything else broken, NotFullscreen
        // fires first per the documented decision tree.
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.fullscreen = false;
        inputs.support = ScanoutSupport::NotSupported;
        inputs.driver_known_broken = true;
        inputs.cursor_overlay_required = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::NotFullscreen
            },
        );
    }

    #[test]
    fn scanout_priority_compositor_beats_driver() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Mutter);
        inputs.support = ScanoutSupport::NotSupported;
        inputs.driver_known_broken = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CompositorUnsupported,
            },
        );
    }

    #[test]
    fn scanout_priority_driver_beats_cursor() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.driver_known_broken = true;
        inputs.cursor_overlay_required = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::DriverBug
            },
        );
    }

    #[test]
    fn scanout_priority_cursor_beats_occlusion() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.cursor_overlay_required = true;
        inputs.partial_occlusion = true;
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CursorOverlay
            },
        );
    }

    #[test]
    fn scanout_priority_occlusion_beats_format() {
        let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
        inputs.partial_occlusion = true;
        inputs.compositor_advertised = vec![BufferFormat::Other { drm_fourcc: 0x99 }];
        let d = evaluate_direct_scanout(&inputs);
        assert_eq!(
            d,
            DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::PartialOcclusion
            },
        );
    }

    // ----------------------------------------------------------------
    // DirectScanoutTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = DirectScanoutTelemetry::default();
        assert_eq!(t.frames_scanout_active, 0);
        assert_eq!(t.scanout_active_rate_pct(), 0);
    }

    #[test]
    fn telemetry_record_active_increments() {
        let mut t = DirectScanoutTelemetry::default();
        t.record_decision(DirectScanoutDecision::Active {
            format: BufferFormat::Bgra8,
        });
        t.record_decision(DirectScanoutDecision::Active {
            format: BufferFormat::Bgra8,
        });
        assert_eq!(t.frames_scanout_active, 2);
        assert_eq!(t.frames_fallback_total, 0);
    }

    #[test]
    fn telemetry_record_fallback_routes_per_cause() {
        let mut t = DirectScanoutTelemetry::default();
        t.record_decision(DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::CursorOverlay,
        });
        t.record_decision(DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::PartialOcclusion,
        });
        t.record_decision(DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::FormatNegotiationFailed,
        });
        t.record_decision(DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::DriverBug,
        });
        t.record_decision(DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::NotFullscreen,
        });
        t.record_decision(DirectScanoutDecision::Fallback {
            cause: ScanoutFallback::CompositorUnsupported,
        });
        assert_eq!(t.frames_fallback_total, 6);
        assert_eq!(t.fallback_cursor_overlay, 1);
        assert_eq!(t.fallback_partial_occlusion, 1);
        assert_eq!(t.fallback_format_negotiation_failed, 1);
        assert_eq!(t.fallback_driver_bug, 1);
        assert_eq!(t.fallback_not_fullscreen, 1);
        assert_eq!(t.fallback_compositor_unsupported, 1);
    }

    #[test]
    fn telemetry_scanout_active_rate_pct() {
        let mut t = DirectScanoutTelemetry::default();
        for _ in 0..7 {
            t.record_decision(DirectScanoutDecision::Active {
                format: BufferFormat::Bgra8,
            });
        }
        for _ in 0..3 {
            t.record_decision(DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CursorOverlay,
            });
        }
        assert_eq!(t.scanout_active_rate_pct(), 70);
    }

    #[test]
    fn telemetry_record_latency_win_accumulates() {
        let mut t = DirectScanoutTelemetry::default();
        t.record_latency_win(8000); // 8 ms saved
        t.record_latency_win(16000); // 16 ms saved
        assert_eq!(t.latency_win_us_total, 24_000);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_sway_fullscreen_terminal_scans_out() {
        // Bead's headline scenario: ft fullscreen on Sway,
        // no notification, no cursor overlay required.
        let inputs = fullscreen_inputs(WaylandCompositor::Sway);
        match evaluate_direct_scanout(&inputs) {
            DirectScanoutDecision::Active { format } => {
                // ft prefers Bgra8 first.
                assert_eq!(format, BufferFormat::Bgra8);
            }
            other @ DirectScanoutDecision::Fallback { .. } => {
                panic!("expected Active scanout; got {other:?}")
            }
        }
    }

    #[test]
    fn scenario_notification_pops_during_fullscreen_falls_back_acceptably() {
        // Bead: "Partial occlusion (notification on top):
        // falls back. Acceptable; latency win still partial."
        let mut inputs = fullscreen_inputs(WaylandCompositor::Mutter);
        inputs.partial_occlusion = true;
        let d = evaluate_direct_scanout(&inputs);
        if let DirectScanoutDecision::Fallback { cause } = d {
            assert!(cause.is_acceptable_per_bead());
            assert_eq!(cause, ScanoutFallback::PartialOcclusion);
        } else {
            panic!("expected Fallback");
        }
    }

    #[test]
    fn scenario_obsolete_compositor_unsupported() {
        // Operator on an old GNOME (pre-42) — Mutter
        // available but support=NotSupported (probe found
        // protocol bindings missing).
        let mut inputs = fullscreen_inputs(WaylandCompositor::Mutter);
        inputs.support = ScanoutSupport::NotSupported;
        let d = evaluate_direct_scanout(&inputs);
        match d {
            DirectScanoutDecision::Fallback { cause } => {
                assert!(!cause.is_acceptable_per_bead()); // unrecoverable
                assert_eq!(cause, ScanoutFallback::CompositorUnsupported);
            }
            DirectScanoutDecision::Active { .. } => panic!("expected Fallback"),
        }
    }

    #[test]
    fn scenario_session_with_50pct_active_rate() {
        let mut t = DirectScanoutTelemetry::default();
        for _ in 0..50 {
            t.record_decision(DirectScanoutDecision::Active {
                format: BufferFormat::Bgra8,
            });
            t.record_latency_win(10_000); // 10 ms saved per scanout frame
        }
        for _ in 0..50 {
            t.record_decision(DirectScanoutDecision::Fallback {
                cause: ScanoutFallback::CursorOverlay,
            });
        }
        assert_eq!(t.scanout_active_rate_pct(), 50);
        assert_eq!(t.latency_win_us_total, 500_000); // 500 ms total saved
    }

    #[test]
    fn scenario_full_corpus_decision_path_coverage() {
        // Walk every fallback variant through the decision
        // tree, asserting each lands on its expected cause.
        type ScanoutCase = (fn(&mut ScanoutInputs), ScanoutFallback);
        let cases: &[ScanoutCase] = &[
            (|i| i.fullscreen = false, ScanoutFallback::NotFullscreen),
            (
                |i| i.support = ScanoutSupport::NotSupported,
                ScanoutFallback::CompositorUnsupported,
            ),
            (|i| i.driver_known_broken = true, ScanoutFallback::DriverBug),
            (
                |i| i.cursor_overlay_required = true,
                ScanoutFallback::CursorOverlay,
            ),
            (
                |i| i.partial_occlusion = true,
                ScanoutFallback::PartialOcclusion,
            ),
            (
                |i| i.compositor_advertised = vec![BufferFormat::Other { drm_fourcc: 0x99 }],
                ScanoutFallback::FormatNegotiationFailed,
            ),
        ];
        for (mutate, expected_cause) in cases {
            let mut inputs = fullscreen_inputs(WaylandCompositor::Sway);
            mutate(&mut inputs);
            let d = evaluate_direct_scanout(&inputs);
            match d {
                DirectScanoutDecision::Fallback { cause } => assert_eq!(cause, *expected_cause),
                other @ DirectScanoutDecision::Active { .. } => {
                    panic!("expected Fallback; got {other:?}")
                }
            }
        }
    }
}
