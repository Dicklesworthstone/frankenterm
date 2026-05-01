//! Display-pipeline policy substrate (ft-4vxjb sub-bead of the
//! ft-2okh0.2 display-pipeline trifecta epic).
//!
//! Three pure-logic policies that drive the trifecta from the
//! integration bead's wiring layer:
//!
//! 1. **VRR negotiation** — given the detected platform + compositor,
//!    pick a Variable Refresh Rate mechanism (Wayland
//!    `wp_tearing_control_v1`, X11 `XPresent`, macOS `CADisplayLink`
//!    dynamic, Windows DWM, or fixed-rate fallback). Pure detection
//!    table; the integration layer separately performs the platform
//!    probe and feeds the result in here.
//!
//! 2. **Direct scanout eligibility** — given the window state +
//!    buffer format + compositor capability set, decide whether the
//!    compositor can scan our buffer out directly (zero compositor
//!    copies). This is a pure predicate; the integration layer
//!    follows the `Eligible` outcome by attaching a `linux-dmabuf`
//!    buffer and the `Blocked { reason }` outcome by falling back to
//!    standard `Present`.
//!
//! 3. **Force-present signal** — given the recording / accessibility
//!    state, decide whether the renderer must bypass any frame-dedup
//!    elision and Present this frame regardless. This guards the
//!    "recording compatibility" acceptance criterion in the bead.
//!
//! ## What is deferred to the integration bead
//!
//! - Platform probing (read `/sys/class/drm`, query the running
//!   compositor via env vars or `WAYLAND_DISPLAY`, CADisplayLink
//!   property reads, DwmFlush availability).
//! - Wayland protocol bindings for `wp_tearing_control_v1`,
//!   `linux-dmabuf-v1`, and `wlr-layer-shell` fullscreen.
//! - X11 `XPresent` extension wiring with `PresentOptionAsync`.
//! - macOS CADisplayLink `preferredFrameRateRange` setter calls.
//! - paint.rs Present-wiring (cross-link
//!   `frankenterm-gui/src/termwindow/render/frame_dedup.rs`).
//! - 24 h idle-battery bench (`benches/idle_battery_drain.rs`).
//! - Per-compositor CI matrix.

#![allow(dead_code)]

use std::time::Duration;

// ============================================================================
// VRR negotiation
// ============================================================================

/// Operating system family. The compositor enum disambiguates within
/// Linux (Wayland vs X11 windowing systems with their own zoo of
/// compositors).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VrrPlatform {
    Wayland,
    X11,
    MacOs,
    Windows,
}

/// Wayland compositor identity. The detection table picks
/// `wp_tearing_control_v1` for the four mainstream compositors that
/// support it and `FixedRate` fallback for `Other`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WaylandCompositor {
    Mutter,
    Kwin,
    Sway,
    Hyprland,
    Other,
}

/// X11 window manager identity. All major X11 WMs route VRR through
/// the `XPresent` extension when the underlying server supports it,
/// so this enum is purely informational for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum X11WindowManager {
    I3,
    Xfwm,
    Kwin,
    Mutter,
    Other,
}

/// The mechanism the renderer drives for variable-refresh-rate
/// presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VrrMechanism {
    /// Wayland `wp_tearing_control_v1` — opt-in tearing presentation
    /// hint accepted by Mutter / Kwin / Sway / Hyprland.
    WpTearingControlV1,
    /// X11 `Present` extension's `PresentOptionAsync` flag.
    XPresentAsync,
    /// macOS `CADisplayLink` with `preferredFrameRateRange`.
    CaDisplayLinkDynamic,
    /// Windows DWM `DwmFlush` + composition timing — best-effort VRR
    /// where the compositor exposes it.
    DwmComposition,
    /// No VRR — the renderer Presents at a fixed cadence and lets the
    /// compositor decide.
    FixedRate,
}

/// VRR detection result. The integration layer takes the mechanism
/// and dispatches to its platform-specific call site; if the
/// mechanism is `FixedRate` the dispatcher uses standard Present.
///
/// Fields are `pub(crate)` so external code can't construct a
/// VrrSupport claiming a mechanism the platform doesn't actually
/// support (e.g., `mechanism=WpTearingControlV1` on
/// `platform=MacOs`), which would lead the integration's
/// dispatcher to call non-existent platform APIs.
/// [`negotiate_vrr_support`] is the only legitimate constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VrrSupport {
    pub(crate) platform: VrrPlatform,
    pub(crate) mechanism: VrrMechanism,
    /// Reported back via `ft doctor` so operators can see *why*
    /// fallback was chosen on a given host.
    pub(crate) fallback_reason: Option<VrrFallbackReason>,
}

impl VrrSupport {
    #[must_use]
    pub const fn platform(self) -> VrrPlatform {
        self.platform
    }
    #[must_use]
    pub const fn mechanism(self) -> VrrMechanism {
        self.mechanism
    }
    #[must_use]
    pub const fn fallback_reason(self) -> Option<VrrFallbackReason> {
        self.fallback_reason
    }
}

/// Why VRR negotiation chose `FixedRate` instead of a real VRR
/// mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VrrFallbackReason {
    /// Compositor doesn't advertise the protocol (e.g. Wayland
    /// `Other` compositor with no `wp_tearing_control_v1`).
    CompositorUnsupported,
    /// X11 server too old / lacks the `Present` extension.
    PresentExtensionMissing,
    /// macOS pre-CADisplayLink dynamic-rate APIs (pre-iOS 15-style;
    /// this is mostly for safety on very old macOS builds).
    DisplayLinkApiMissing,
    /// Windows compositor disabled or DWM unavailable.
    DwmUnavailable,
}

/// Pure detection table. Given a platform and (Wayland-only) the
/// detected compositor, return the negotiated VRR mechanism. The
/// integration layer feeds in the probe results; this function is
/// total and side-effect-free.
#[must_use]
pub fn negotiate_vrr_support(
    platform: VrrPlatform,
    wayland_compositor: Option<WaylandCompositor>,
    x11_present_available: bool,
) -> VrrSupport {
    match platform {
        VrrPlatform::Wayland => match wayland_compositor {
            Some(
                WaylandCompositor::Mutter
                | WaylandCompositor::Kwin
                | WaylandCompositor::Sway
                | WaylandCompositor::Hyprland,
            ) => VrrSupport {
                platform,
                mechanism: VrrMechanism::WpTearingControlV1,
                fallback_reason: None,
            },
            Some(WaylandCompositor::Other) | None => VrrSupport {
                platform,
                mechanism: VrrMechanism::FixedRate,
                fallback_reason: Some(VrrFallbackReason::CompositorUnsupported),
            },
        },
        VrrPlatform::X11 => {
            if x11_present_available {
                VrrSupport {
                    platform,
                    mechanism: VrrMechanism::XPresentAsync,
                    fallback_reason: None,
                }
            } else {
                VrrSupport {
                    platform,
                    mechanism: VrrMechanism::FixedRate,
                    fallback_reason: Some(VrrFallbackReason::PresentExtensionMissing),
                }
            }
        }
        VrrPlatform::MacOs => VrrSupport {
            platform,
            mechanism: VrrMechanism::CaDisplayLinkDynamic,
            fallback_reason: None,
        },
        VrrPlatform::Windows => VrrSupport {
            platform,
            mechanism: VrrMechanism::DwmComposition,
            fallback_reason: None,
        },
    }
}

// ============================================================================
// Refresh-range negotiation
// ============================================================================

/// A display's advertised refresh capability. `min_hz` <= `max_hz`;
/// for a fixed-rate display they're equal.
///
/// Fields are `pub(crate)` because the `min_hz <= max_hz` and
/// non-zero invariants are enforced by [`Self::new`]. Direct
/// field write would let a maintainer set `max_hz = 0`, which
/// then panics downstream in [`frame_deadline`] (`1.0 / 0` →
/// `Duration::from_secs_f64(inf)` → panic). Read via
/// [`Self::min_hz`] / [`Self::max_hz`]; mutate via the
/// constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRange {
    pub(crate) min_hz: u32,
    pub(crate) max_hz: u32,
}

impl RefreshRange {
    /// Build a range, swapping if the caller passes them in the wrong
    /// order. Panics in debug if either value is zero — a 0 Hz
    /// refresh range is meaningless and almost certainly a probe bug
    /// the integration layer should surface as a fallback path.
    #[must_use]
    pub fn new(a: u32, b: u32) -> Self {
        debug_assert!(a > 0 && b > 0, "refresh range hz must be non-zero");
        let (min_hz, max_hz) = if a <= b { (a, b) } else { (b, a) };
        Self { min_hz, max_hz }
    }

    /// Read accessor for the lower bound.
    #[must_use]
    pub const fn min_hz(self) -> u32 {
        self.min_hz
    }

    /// Read accessor for the upper bound.
    #[must_use]
    pub const fn max_hz(self) -> u32 {
        self.max_hz
    }

    #[must_use]
    pub fn fixed(hz: u32) -> Self {
        Self::new(hz, hz)
    }

    #[must_use]
    pub fn is_fixed(&self) -> bool {
        self.min_hz == self.max_hz
    }

    /// Clamp a requested target into the display's capability window.
    #[must_use]
    pub fn clamp(&self, requested_hz: u32) -> u32 {
        requested_hz.clamp(self.min_hz, self.max_hz)
    }
}

/// Negotiate the renderer's per-frame refresh target given the
/// display's advertised range and the workload's preferred range.
/// Returns the intersection (which may collapse to a single Hz on a
/// fixed-rate display) or `Bypass` if the workload's preferred range
/// is entirely outside what the display supports — the integration
/// layer logs that as a configuration warning and proceeds at the
/// display's max.
#[must_use]
pub fn negotiate_refresh_range(
    display: RefreshRange,
    requested: RefreshRange,
) -> RefreshNegotiation {
    if requested.max_hz < display.min_hz || requested.min_hz > display.max_hz {
        return RefreshNegotiation::OutOfRange {
            display,
            requested,
            applied_max_hz: display.max_hz,
        };
    }
    let min_hz = requested.min_hz.max(display.min_hz);
    let max_hz = requested.max_hz.min(display.max_hz);
    RefreshNegotiation::Negotiated(RefreshRange { min_hz, max_hz })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshNegotiation {
    Negotiated(RefreshRange),
    /// Workload requested a range entirely disjoint from the display's
    /// capability. The integration layer should log + clamp; this
    /// variant carries the diagnostic data plus the clamped max so
    /// the caller can still drive a sane Present cadence.
    OutOfRange {
        display: RefreshRange,
        requested: RefreshRange,
        applied_max_hz: u32,
    },
}

impl RefreshNegotiation {
    #[must_use]
    pub fn applied_max_hz(&self) -> u32 {
        match self {
            Self::Negotiated(r) => r.max_hz,
            Self::OutOfRange { applied_max_hz, .. } => *applied_max_hz,
        }
    }
}

// ============================================================================
// Direct scanout eligibility
// ============================================================================

/// A renderer surface buffer format the integration layer wants the
/// compositor to scan out directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanoutBufferFormat {
    /// Linear ARGB8888 — the universal fallback, almost always
    /// scan-out-eligible.
    Argb8888,
    /// 10-bit-per-channel HDR-friendly format. Only some compositors
    /// (recent Mutter, Kwin Plasma 6+) handle this on scanout.
    Argb2101010,
    /// Compressed / tiled vendor-specific format. The integration
    /// layer must check the compositor's modifier list separately;
    /// this variant signals "ask, don't assume".
    VendorTiled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowMode {
    Fullscreen,
    Maximized,
    Windowed,
}

/// What the compositor advertises about scanout in the running
/// session. The integration layer fills these out from the Wayland
/// `linux-dmabuf-v1` modifier list and the `wp_viewporter`
/// capability set; on X11 / macOS we always use the `Unsupported`
/// path (direct scanout is a Wayland-only acceleration).
///
/// Fields are `pub(crate)` for consistency with other substrate
/// security/state surfaces; use the [`Self::unsupported`]
/// constructor + `with_*` builder API to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompositorScanoutCaps {
    pub(crate) supports_dmabuf: bool,
    pub(crate) supports_argb8888_scanout: bool,
    pub(crate) supports_10bit_scanout: bool,
    pub(crate) supports_vendor_modifiers: bool,
}

impl CompositorScanoutCaps {
    /// Conservative default: no scanout support. The integration
    /// layer flips fields on as it probes.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            supports_dmabuf: false,
            supports_argb8888_scanout: false,
            supports_10bit_scanout: false,
            supports_vendor_modifiers: false,
        }
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn supports_dmabuf(self) -> bool {
        self.supports_dmabuf
    }
    #[must_use]
    pub const fn supports_argb8888_scanout(self) -> bool {
        self.supports_argb8888_scanout
    }
    #[must_use]
    pub const fn supports_10bit_scanout(self) -> bool {
        self.supports_10bit_scanout
    }
    #[must_use]
    pub const fn supports_vendor_modifiers(self) -> bool {
        self.supports_vendor_modifiers
    }

    // ----- Builder API -----

    #[must_use]
    pub const fn with_dmabuf(mut self, supported: bool) -> Self {
        self.supports_dmabuf = supported;
        self
    }
    #[must_use]
    pub const fn with_argb8888_scanout(mut self, supported: bool) -> Self {
        self.supports_argb8888_scanout = supported;
        self
    }
    #[must_use]
    pub const fn with_10bit_scanout(mut self, supported: bool) -> Self {
        self.supports_10bit_scanout = supported;
        self
    }
    #[must_use]
    pub const fn with_vendor_modifiers(mut self, supported: bool) -> Self {
        self.supports_vendor_modifiers = supported;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanoutBlockReason {
    NotFullscreen,
    CompositorNoDmabuf,
    BufferFormatNotSupported,
    VendorModifierNotAdvertised,
    MultiPaneOverlayPresent,
    RecordingActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanoutEligibility {
    Eligible,
    Blocked(ScanoutBlockReason),
}

impl ScanoutEligibility {
    #[must_use]
    pub fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible)
    }

    #[must_use]
    pub fn block_reason(&self) -> Option<ScanoutBlockReason> {
        match self {
            Self::Blocked(r) => Some(*r),
            Self::Eligible => None,
        }
    }
}

/// Decide direct-scanout eligibility from the integration layer's
/// probe inputs. The order of checks mirrors the cheap-to-expensive
/// gradient — `NotFullscreen` is trivial, `MultiPaneOverlayPresent`
/// is observed from the GUI's pane tree, and `RecordingActive` is
/// the last check because cross-platform recording detection
/// (ScreenCaptureKit on macOS, PipeWire on Linux) is the most
/// expensive probe.
#[must_use]
pub fn evaluate_scanout(
    window_mode: WindowMode,
    buffer_format: ScanoutBufferFormat,
    caps: CompositorScanoutCaps,
    multi_pane_overlay_present: bool,
    recording_active: bool,
) -> ScanoutEligibility {
    if window_mode != WindowMode::Fullscreen {
        return ScanoutEligibility::Blocked(ScanoutBlockReason::NotFullscreen);
    }
    if !caps.supports_dmabuf {
        return ScanoutEligibility::Blocked(ScanoutBlockReason::CompositorNoDmabuf);
    }
    let format_ok = match buffer_format {
        ScanoutBufferFormat::Argb8888 => caps.supports_argb8888_scanout,
        ScanoutBufferFormat::Argb2101010 => caps.supports_10bit_scanout,
        ScanoutBufferFormat::VendorTiled => caps.supports_vendor_modifiers,
    };
    if !format_ok {
        let reason = if matches!(buffer_format, ScanoutBufferFormat::VendorTiled) {
            ScanoutBlockReason::VendorModifierNotAdvertised
        } else {
            ScanoutBlockReason::BufferFormatNotSupported
        };
        return ScanoutEligibility::Blocked(reason);
    }
    if multi_pane_overlay_present {
        return ScanoutEligibility::Blocked(ScanoutBlockReason::MultiPaneOverlayPresent);
    }
    if recording_active {
        return ScanoutEligibility::Blocked(ScanoutBlockReason::RecordingActive);
    }
    ScanoutEligibility::Eligible
}

// ============================================================================
// Force-present signal
// ============================================================================

/// Reason the renderer must bypass frame-dedup elision and Present
/// this frame regardless of dedup outcome. The integration layer's
/// paint loop checks `should_force_present` *before* the dedup
/// decision and short-circuits to Present when this is `Some`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForcePresentReason {
    /// macOS ScreenCaptureKit / Linux PipeWire reports an active
    /// screen-recording session. Dedup elision would create gaps in
    /// the recording timeline.
    ScreenRecordingActive,
    /// AT-SPI / NSAccessibility query is in flight; the assistive
    /// tech expects the latest visual state.
    A11yQueryInFlight,
    /// Operator pressed a manual flush key / sent `ft flush` — the
    /// debug surface needs a guaranteed Present.
    ManualFlush,
    /// First frame after a window resize / pane split — flush any
    /// dedup state from the prior layout.
    PostLayoutChange,
}

/// Pure predicate. Returns the *highest-priority* reason if any
/// applies; ties resolve toward operator intent (manual > a11y >
/// recording > layout) since the operator's a11y reader is the most
/// latency-sensitive observer of a missed Present.
#[must_use]
pub fn should_force_present(
    recording_active: bool,
    a11y_query_in_flight: bool,
    manual_flush_requested: bool,
    post_layout_change: bool,
) -> Option<ForcePresentReason> {
    if manual_flush_requested {
        return Some(ForcePresentReason::ManualFlush);
    }
    if a11y_query_in_flight {
        return Some(ForcePresentReason::A11yQueryInFlight);
    }
    if recording_active {
        return Some(ForcePresentReason::ScreenRecordingActive);
    }
    if post_layout_change {
        return Some(ForcePresentReason::PostLayoutChange);
    }
    None
}

// ============================================================================
// Per-frame Present decision
// ============================================================================

/// The deadline budget the renderer has between frames. Computed
/// from `RefreshRange::max_hz` as `1 / max_hz` and exposed for the
/// integration's frame-budget allocator (cross-link the existing
/// `FrameBudget` type from ft-mpc9b.5.2).
///
/// Returns a fallback 60 Hz deadline (16.667 ms) when `max_hz` is
/// 0. The pub-field-private + constructor-validated [`RefreshRange`]
/// API makes max_hz=0 unreachable in normal flow, but a serde-
/// deserialized `RefreshRange` (or future code paths) could
/// produce an inconsistent value; the runtime guard prevents the
/// `1.0 / 0 = inf → Duration::from_secs_f64(inf) panic` failure
/// mode that would otherwise crash the render thread.
#[must_use]
pub fn frame_deadline(range: RefreshRange) -> Duration {
    if range.max_hz == 0 {
        // Fallback: 60 Hz (≈ 16.667 ms) — same default the
        // integration layer uses for "VRR negotiation
        // failed, drive at fixed rate".
        return Duration::from_micros(16_667);
    }
    Duration::from_secs_f64(1.0 / f64::from(range.max_hz))
}

/// What the renderer should do for this frame. The integration layer
/// composes this with the existing `FrameDeduplicator` (force-present
/// short-circuits the dedup) and the platform-specific Present call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentAction {
    /// Present immediately via the negotiated VRR mechanism (or
    /// fixed-rate Present when the mechanism is `FixedRate`).
    PresentNow {
        mechanism: VrrMechanism,
        scanout: ScanoutEligibility,
    },
    /// Force-present overriding any dedup state (records, a11y,
    /// manual flush, post-layout).
    ForcePresent {
        mechanism: VrrMechanism,
        reason: ForcePresentReason,
    },
    /// Yield to the next frame-pacing tick — used when the workload
    /// has nothing to draw and dedup would have elided this frame
    /// anyway. The integration layer drops back to its idle path.
    Skip,
}

/// Compose VRR mechanism, scanout eligibility, dedup outcome, and
/// force-present signal into a single per-frame action. Pure-logic;
/// the integration layer wires this directly to the Present call.
#[must_use]
pub fn decide_present(
    vrr: VrrSupport,
    scanout: ScanoutEligibility,
    dedup_says_skip: bool,
    force_present: Option<ForcePresentReason>,
) -> PresentAction {
    if let Some(reason) = force_present {
        return PresentAction::ForcePresent {
            mechanism: vrr.mechanism,
            reason,
        };
    }
    if dedup_says_skip {
        return PresentAction::Skip;
    }
    PresentAction::PresentNow {
        mechanism: vrr.mechanism,
        scanout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // VRR detection table
    // ----------------------------------------------------------------

    #[test]
    fn wayland_mainstream_compositors_get_tearing_control() {
        for compositor in [
            WaylandCompositor::Mutter,
            WaylandCompositor::Kwin,
            WaylandCompositor::Sway,
            WaylandCompositor::Hyprland,
        ] {
            let vrr = negotiate_vrr_support(VrrPlatform::Wayland, Some(compositor), false);
            assert_eq!(
                vrr.mechanism,
                VrrMechanism::WpTearingControlV1,
                "compositor {compositor:?} should negotiate WpTearingControlV1"
            );
            assert!(vrr.fallback_reason.is_none());
        }
    }

    #[test]
    fn wayland_other_compositor_falls_back_with_reason() {
        let vrr =
            negotiate_vrr_support(VrrPlatform::Wayland, Some(WaylandCompositor::Other), false);
        assert_eq!(vrr.mechanism, VrrMechanism::FixedRate);
        assert_eq!(
            vrr.fallback_reason,
            Some(VrrFallbackReason::CompositorUnsupported)
        );
    }

    #[test]
    fn wayland_no_compositor_detected_falls_back() {
        let vrr = negotiate_vrr_support(VrrPlatform::Wayland, None, false);
        assert_eq!(vrr.mechanism, VrrMechanism::FixedRate);
        assert_eq!(
            vrr.fallback_reason,
            Some(VrrFallbackReason::CompositorUnsupported)
        );
    }

    #[test]
    fn x11_with_present_extension_picks_xpresent_async() {
        let vrr = negotiate_vrr_support(VrrPlatform::X11, None, true);
        assert_eq!(vrr.mechanism, VrrMechanism::XPresentAsync);
        assert!(vrr.fallback_reason.is_none());
    }

    #[test]
    fn x11_without_present_extension_falls_back_with_reason() {
        let vrr = negotiate_vrr_support(VrrPlatform::X11, None, false);
        assert_eq!(vrr.mechanism, VrrMechanism::FixedRate);
        assert_eq!(
            vrr.fallback_reason,
            Some(VrrFallbackReason::PresentExtensionMissing)
        );
    }

    #[test]
    fn macos_always_picks_cadisplaylink_dynamic() {
        let vrr = negotiate_vrr_support(VrrPlatform::MacOs, None, false);
        assert_eq!(vrr.mechanism, VrrMechanism::CaDisplayLinkDynamic);
        assert!(vrr.fallback_reason.is_none());
    }

    #[test]
    fn windows_always_picks_dwm_composition() {
        let vrr = negotiate_vrr_support(VrrPlatform::Windows, None, false);
        assert_eq!(vrr.mechanism, VrrMechanism::DwmComposition);
        assert!(vrr.fallback_reason.is_none());
    }

    // ----------------------------------------------------------------
    // RefreshRange + negotiation
    // ----------------------------------------------------------------

    #[test]
    fn refresh_range_swaps_inverted_args() {
        let r = RefreshRange::new(120, 60);
        assert_eq!(r.min_hz, 60);
        assert_eq!(r.max_hz, 120);
    }

    #[test]
    fn refresh_range_fixed_is_fixed() {
        let r = RefreshRange::fixed(60);
        assert!(r.is_fixed());
        assert_eq!(r.min_hz, 60);
        assert_eq!(r.max_hz, 60);
    }

    #[test]
    fn refresh_range_clamps_into_window() {
        let r = RefreshRange::new(48, 144);
        assert_eq!(r.clamp(30), 48);
        assert_eq!(r.clamp(60), 60);
        assert_eq!(r.clamp(240), 144);
    }

    #[test]
    fn refresh_range_accessors_round_trip() {
        // Pin: pub(crate) fields read via accessors.
        let r = RefreshRange::new(48, 144);
        assert_eq!(r.min_hz(), 48);
        assert_eq!(r.max_hz(), 144);
    }

    #[test]
    fn frame_deadline_does_not_panic_on_zero_max_hz() {
        // CORRECTNESS REGRESSION: previously, a malformed
        // RefreshRange (e.g. via direct field write before
        // pub→pub(crate), or future serde-deserialization
        // path) with max_hz=0 caused frame_deadline to do
        // 1.0/0=inf → Duration::from_secs_f64(inf) PANIC,
        // crashing the render thread. Now we fall back to
        // 60 Hz.
        //
        // Construct the malformed value via raw field
        // initialization through the same module
        // (pub(crate) reachable here from in-crate tests).
        let bad = RefreshRange { min_hz: 0, max_hz: 0 };
        let deadline = frame_deadline(bad);
        // 60 Hz fallback = 16.667 ms.
        assert_eq!(deadline, Duration::from_micros(16_667));
    }

    #[test]
    fn vrr_support_accessors_round_trip() {
        let s = negotiate_vrr_support(VrrPlatform::Wayland, Some(WaylandCompositor::Mutter), false);
        assert_eq!(s.platform(), VrrPlatform::Wayland);
        assert_eq!(s.mechanism(), VrrMechanism::WpTearingControlV1);
        assert_eq!(s.fallback_reason(), None);
    }

    #[test]
    fn compositor_scanout_caps_builder_round_trip() {
        let caps = CompositorScanoutCaps::unsupported()
            .with_dmabuf(true)
            .with_argb8888_scanout(true)
            .with_10bit_scanout(false)
            .with_vendor_modifiers(false);
        assert!(caps.supports_dmabuf());
        assert!(caps.supports_argb8888_scanout());
        assert!(!caps.supports_10bit_scanout());
        assert!(!caps.supports_vendor_modifiers());
    }

    #[test]
    fn refresh_negotiation_intersects_overlapping_ranges() {
        let display = RefreshRange::new(48, 144);
        let requested = RefreshRange::new(60, 120);
        match negotiate_refresh_range(display, requested) {
            RefreshNegotiation::Negotiated(r) => {
                assert_eq!(r.min_hz, 60);
                assert_eq!(r.max_hz, 120);
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
    }

    #[test]
    fn refresh_negotiation_clips_to_display_when_request_is_wider() {
        let display = RefreshRange::new(60, 120);
        let requested = RefreshRange::new(30, 240);
        match negotiate_refresh_range(display, requested) {
            RefreshNegotiation::Negotiated(r) => {
                assert_eq!(r.min_hz, 60);
                assert_eq!(r.max_hz, 120);
            }
            other => panic!("expected Negotiated, got {other:?}"),
        }
    }

    #[test]
    fn refresh_negotiation_out_of_range_above() {
        let display = RefreshRange::new(48, 60);
        let requested = RefreshRange::new(120, 240);
        match negotiate_refresh_range(display, requested) {
            RefreshNegotiation::OutOfRange { applied_max_hz, .. } => assert_eq!(applied_max_hz, 60),
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn refresh_negotiation_out_of_range_below() {
        let display = RefreshRange::new(120, 240);
        let requested = RefreshRange::new(30, 60);
        match negotiate_refresh_range(display, requested) {
            RefreshNegotiation::OutOfRange { applied_max_hz, .. } => {
                assert_eq!(applied_max_hz, 240)
            }
            other => panic!("expected OutOfRange, got {other:?}"),
        }
    }

    #[test]
    fn refresh_negotiation_applied_max_works_either_variant() {
        let neg = negotiate_refresh_range(RefreshRange::fixed(60), RefreshRange::fixed(60));
        assert_eq!(neg.applied_max_hz(), 60);
        let oor = negotiate_refresh_range(RefreshRange::fixed(60), RefreshRange::fixed(120));
        assert_eq!(oor.applied_max_hz(), 60);
    }

    #[test]
    fn frame_deadline_inverts_max_hz() {
        let r = RefreshRange::fixed(60);
        let d = frame_deadline(r);
        // 1/60 ≈ 16.666 ms
        assert!(d.as_millis() == 16 || d.as_millis() == 17);
    }

    #[test]
    fn frame_deadline_120hz_is_half_of_60hz() {
        let d_60 = frame_deadline(RefreshRange::fixed(60));
        let d_120 = frame_deadline(RefreshRange::fixed(120));
        // Within rounding, 60 Hz deadline is ~2x 120 Hz deadline.
        assert!(d_60.as_secs_f64() / d_120.as_secs_f64() > 1.99);
        assert!(d_60.as_secs_f64() / d_120.as_secs_f64() < 2.01);
    }

    // ----------------------------------------------------------------
    // Direct scanout
    // ----------------------------------------------------------------

    fn full_caps() -> CompositorScanoutCaps {
        CompositorScanoutCaps {
            supports_dmabuf: true,
            supports_argb8888_scanout: true,
            supports_10bit_scanout: true,
            supports_vendor_modifiers: true,
        }
    }

    #[test]
    fn scanout_eligible_when_fullscreen_dmabuf_argb8888_no_overlay_no_recording() {
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            full_caps(),
            false,
            false,
        );
        assert_eq!(r, ScanoutEligibility::Eligible);
        assert!(r.is_eligible());
        assert_eq!(r.block_reason(), None);
    }

    #[test]
    fn scanout_blocked_when_not_fullscreen() {
        for mode in [WindowMode::Maximized, WindowMode::Windowed] {
            let r = evaluate_scanout(
                mode,
                ScanoutBufferFormat::Argb8888,
                full_caps(),
                false,
                false,
            );
            assert_eq!(
                r.block_reason(),
                Some(ScanoutBlockReason::NotFullscreen),
                "mode {mode:?} should block scanout"
            );
        }
    }

    #[test]
    fn scanout_blocked_when_compositor_no_dmabuf() {
        let caps = CompositorScanoutCaps::unsupported();
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            caps,
            false,
            false,
        );
        assert_eq!(
            r.block_reason(),
            Some(ScanoutBlockReason::CompositorNoDmabuf)
        );
    }

    #[test]
    fn scanout_blocked_when_format_unsupported_argb8888() {
        let caps = CompositorScanoutCaps {
            supports_dmabuf: true,
            supports_argb8888_scanout: false,
            supports_10bit_scanout: false,
            supports_vendor_modifiers: false,
        };
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            caps,
            false,
            false,
        );
        assert_eq!(
            r.block_reason(),
            Some(ScanoutBlockReason::BufferFormatNotSupported)
        );
    }

    #[test]
    fn scanout_blocked_when_format_unsupported_10bit() {
        let caps = CompositorScanoutCaps {
            supports_dmabuf: true,
            supports_argb8888_scanout: true,
            supports_10bit_scanout: false,
            supports_vendor_modifiers: false,
        };
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb2101010,
            caps,
            false,
            false,
        );
        assert_eq!(
            r.block_reason(),
            Some(ScanoutBlockReason::BufferFormatNotSupported)
        );
    }

    #[test]
    fn scanout_blocked_for_vendor_tiled_uses_specific_reason() {
        let caps = CompositorScanoutCaps {
            supports_dmabuf: true,
            supports_argb8888_scanout: true,
            supports_10bit_scanout: true,
            supports_vendor_modifiers: false,
        };
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::VendorTiled,
            caps,
            false,
            false,
        );
        assert_eq!(
            r.block_reason(),
            Some(ScanoutBlockReason::VendorModifierNotAdvertised),
            "vendor-tiled rejection should report the vendor-specific reason \
             so the integration layer can suggest a modifier list change"
        );
    }

    #[test]
    fn scanout_blocked_when_overlay_present() {
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            full_caps(),
            true,
            false,
        );
        assert_eq!(
            r.block_reason(),
            Some(ScanoutBlockReason::MultiPaneOverlayPresent)
        );
    }

    #[test]
    fn scanout_blocked_when_recording_active() {
        let r = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            full_caps(),
            false,
            true,
        );
        assert_eq!(r.block_reason(), Some(ScanoutBlockReason::RecordingActive));
    }

    #[test]
    fn scanout_check_order_not_fullscreen_dominates() {
        // Even with everything else broken, NotFullscreen is reported
        // first (cheapest check; surfaces the right operator hint).
        let r = evaluate_scanout(
            WindowMode::Windowed,
            ScanoutBufferFormat::VendorTiled,
            CompositorScanoutCaps::unsupported(),
            true,
            true,
        );
        assert_eq!(r.block_reason(), Some(ScanoutBlockReason::NotFullscreen));
    }

    // ----------------------------------------------------------------
    // Force-present
    // ----------------------------------------------------------------

    #[test]
    fn force_present_none_when_all_signals_clear() {
        assert_eq!(should_force_present(false, false, false, false), None);
    }

    #[test]
    fn force_present_recording_only() {
        assert_eq!(
            should_force_present(true, false, false, false),
            Some(ForcePresentReason::ScreenRecordingActive)
        );
    }

    #[test]
    fn force_present_a11y_only() {
        assert_eq!(
            should_force_present(false, true, false, false),
            Some(ForcePresentReason::A11yQueryInFlight)
        );
    }

    #[test]
    fn force_present_manual_only() {
        assert_eq!(
            should_force_present(false, false, true, false),
            Some(ForcePresentReason::ManualFlush)
        );
    }

    #[test]
    fn force_present_post_layout_only() {
        assert_eq!(
            should_force_present(false, false, false, true),
            Some(ForcePresentReason::PostLayoutChange)
        );
    }

    #[test]
    fn force_present_priority_manual_beats_all() {
        assert_eq!(
            should_force_present(true, true, true, true),
            Some(ForcePresentReason::ManualFlush)
        );
    }

    #[test]
    fn force_present_priority_a11y_beats_recording() {
        // Operator's a11y reader is more latency-sensitive than the
        // recording timeline.
        assert_eq!(
            should_force_present(true, true, false, false),
            Some(ForcePresentReason::A11yQueryInFlight)
        );
    }

    #[test]
    fn force_present_priority_recording_beats_layout() {
        assert_eq!(
            should_force_present(true, false, false, true),
            Some(ForcePresentReason::ScreenRecordingActive)
        );
    }

    // ----------------------------------------------------------------
    // decide_present composition
    // ----------------------------------------------------------------

    fn fixed_rate_vrr() -> VrrSupport {
        VrrSupport {
            platform: VrrPlatform::X11,
            mechanism: VrrMechanism::FixedRate,
            fallback_reason: Some(VrrFallbackReason::PresentExtensionMissing),
        }
    }

    fn wayland_vrr() -> VrrSupport {
        VrrSupport {
            platform: VrrPlatform::Wayland,
            mechanism: VrrMechanism::WpTearingControlV1,
            fallback_reason: None,
        }
    }

    #[test]
    fn decide_present_force_overrides_dedup() {
        let action = decide_present(
            wayland_vrr(),
            ScanoutEligibility::Eligible,
            true, // dedup says skip
            Some(ForcePresentReason::ScreenRecordingActive),
        );
        match action {
            PresentAction::ForcePresent { mechanism, reason } => {
                assert_eq!(mechanism, VrrMechanism::WpTearingControlV1);
                assert_eq!(reason, ForcePresentReason::ScreenRecordingActive);
            }
            other => panic!("expected ForcePresent, got {other:?}"),
        }
    }

    #[test]
    fn decide_present_skip_when_dedup_says_skip_and_no_force() {
        let action = decide_present(wayland_vrr(), ScanoutEligibility::Eligible, true, None);
        assert_eq!(action, PresentAction::Skip);
    }

    #[test]
    fn decide_present_present_now_when_dedup_says_render() {
        let action = decide_present(wayland_vrr(), ScanoutEligibility::Eligible, false, None);
        match action {
            PresentAction::PresentNow { mechanism, scanout } => {
                assert_eq!(mechanism, VrrMechanism::WpTearingControlV1);
                assert_eq!(scanout, ScanoutEligibility::Eligible);
            }
            other => panic!("expected PresentNow, got {other:?}"),
        }
    }

    #[test]
    fn decide_present_carries_blocked_scanout_into_present_now() {
        // PresentNow still fires when scanout is blocked — the
        // integration layer reads the block reason and falls back to
        // standard Present rather than the dmabuf path.
        let action = decide_present(
            wayland_vrr(),
            ScanoutEligibility::Blocked(ScanoutBlockReason::NotFullscreen),
            false,
            None,
        );
        match action {
            PresentAction::PresentNow { scanout, .. } => {
                assert_eq!(
                    scanout,
                    ScanoutEligibility::Blocked(ScanoutBlockReason::NotFullscreen)
                );
            }
            other => panic!("expected PresentNow, got {other:?}"),
        }
    }

    #[test]
    fn decide_present_fixed_rate_mechanism_propagates() {
        let action = decide_present(
            fixed_rate_vrr(),
            ScanoutEligibility::Blocked(ScanoutBlockReason::CompositorNoDmabuf),
            false,
            None,
        );
        match action {
            PresentAction::PresentNow { mechanism, .. } => {
                assert_eq!(mechanism, VrrMechanism::FixedRate);
            }
            other => panic!("expected PresentNow, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic scenarios from the bead
    // ----------------------------------------------------------------

    #[test]
    fn scenario_wayland_fullscreen_idle_typing_dedup_path() {
        // Sway compositor, fullscreen, dmabuf + argb8888, no overlay,
        // no recording, dedup says "this idle frame is identical so
        // skip" — final action should be Skip (which the integration
        // layer's idle-battery bench depends on).
        let vrr = negotiate_vrr_support(VrrPlatform::Wayland, Some(WaylandCompositor::Sway), false);
        let scanout = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            full_caps(),
            false,
            false,
        );
        let action = decide_present(vrr, scanout, true, None);
        assert_eq!(action, PresentAction::Skip);
    }

    #[test]
    fn scenario_macos_recording_active_forces_present() {
        // macOS, ScreenCaptureKit reports active recording — the
        // recording compatibility acceptance criterion in the bead
        // says we must NOT elide frames, even if dedup would have.
        let vrr = negotiate_vrr_support(VrrPlatform::MacOs, None, false);
        let scanout = ScanoutEligibility::Blocked(ScanoutBlockReason::CompositorNoDmabuf);
        let force = should_force_present(true, false, false, false);
        let action = decide_present(vrr, scanout, true, force);
        match action {
            PresentAction::ForcePresent { mechanism, reason } => {
                assert_eq!(mechanism, VrrMechanism::CaDisplayLinkDynamic);
                assert_eq!(reason, ForcePresentReason::ScreenRecordingActive);
            }
            other => panic!("expected ForcePresent, got {other:?}"),
        }
    }

    #[test]
    fn scenario_x11_no_present_extension_full_pipeline_works() {
        let vrr = negotiate_vrr_support(VrrPlatform::X11, None, false);
        // X11 doesn't do direct scanout in our stack, so always
        // blocked at the platform abstraction level — the integration
        // layer plumbs Unsupported caps in.
        let scanout = evaluate_scanout(
            WindowMode::Fullscreen,
            ScanoutBufferFormat::Argb8888,
            CompositorScanoutCaps::unsupported(),
            false,
            false,
        );
        assert_eq!(
            scanout.block_reason(),
            Some(ScanoutBlockReason::CompositorNoDmabuf)
        );
        let action = decide_present(vrr, scanout, false, None);
        match action {
            PresentAction::PresentNow { mechanism, scanout } => {
                assert_eq!(mechanism, VrrMechanism::FixedRate);
                assert!(!scanout.is_eligible());
            }
            other => panic!("expected PresentNow, got {other:?}"),
        }
    }
}
