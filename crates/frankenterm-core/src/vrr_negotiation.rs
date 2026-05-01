//! Variable Refresh Rate per-frame negotiation contract
//! ([BR-TERM-EMULATOR-UPLIFT-2.2.1] / `ft-2okh0.2.1`).
//!
//! Modern displays support VRR (FreeSync, G-Sync, ProMotion).
//! ft tells the compositor a desired refresh rate per frame:
//! idle → 30 Hz, typing → 60 Hz, live-resize → display max.
//! Per the bead's user-value claim: ~2× battery on M2
//! MacBook + smooth resize + no GPU work to refresh static
//! text at 120 Hz.
//!
//! ## What this module ships
//!
//! - [`VrrPlatformApi`] — per-platform negotiation API
//!   identity (Wayland presentation-time, X11 Present,
//!   macOS CADisplayLink, plus a `Unsupported` fallback).
//! - [`DisplayCapability`] — what the doctor probes at
//!   startup: `vrr_supported`, `min_rate_hz`, `max_rate_hz`.
//! - [`FrameRateRequestInputs`] — the per-frame inputs the
//!   decision tree consumes: `LiveResizeState`,
//!   `AnimationActive`, `BatteryState`, `IdleStreakFrames`,
//!   `RecordingActive`.
//! - [`decide_request`] — pure-logic decision tree that
//!   converts inputs into a `FrameRateRequest`. The decision
//!   policy mirrors the bead's stated rules.
//! - [`FrameRateRequest`] — what the integration sends to the
//!   per-platform API: target Hz with a `clamp_reason` if
//!   the value was clipped to the display floor / ceiling.
//! - [`NegotiationOutcome`] — what came back: `Honored`,
//!   `ClampedByCompositor`, `Failed`, `FellBackToFixedRate`.
//! - [`VrrHealth`] — `ft doctor` snapshot with negotiated-
//!   rate distribution + mismatch counter (the bead's
//!   "mismatched_negotiated_vs_actual_rate" telemetry).
//! - The `vrr_disabled_when_recording` flag (bead's "DO NOT
//!   BREAK" recording rule) projects into the decision tree.
//!
//! ## What this module is NOT
//!
//! - Not the per-platform API code itself. Wayland's
//!   `wp_tearing_control_v1` + `presentation-time` proxies,
//!   X11's `XPresentPixmap`, and macOS's
//!   `CADisplayLink.preferredFrameRateRange` live in the
//!   per-platform window crates. This module is the
//!   contract layer they project into.
//! - Not the doctor-side detection. Doctor probes via the
//!   per-platform API; this module ships the
//!   `DisplayCapability` shape the result projects into.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Per-platform API identity
// ============================================================================

/// Per-platform VRR negotiation API. Adding a platform
/// extends this enum; the per-frame negotiation code
/// dispatches on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VrrPlatformApi {
    /// `wp_tearing_control_v1` + `presentation-time` —
    /// covers mutter, kwin, sway, hyprland.
    WaylandPresentationTime,
    /// `XPresent` + Present extension — `XPresentPixmap`
    /// with `PresentOptionAsync`.
    X11Present,
    /// `CADisplayLink.preferredFrameRateRange` — adaptive
    /// within range (ProMotion).
    MacosCaDisplayLink,
    /// Compositor doesn't expose VRR; ft falls back to
    /// fixed-rate Present (no regression vs current
    /// behavior).
    Unsupported,
}

impl VrrPlatformApi {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::WaylandPresentationTime => "wayland_presentation_time",
            Self::X11Present => "x11_present",
            Self::MacosCaDisplayLink => "macos_ca_display_link",
            Self::Unsupported => "unsupported",
        }
    }

    /// Whether this API can carry a per-frame requested rate.
    #[must_use]
    pub const fn supports_per_frame_rate(self) -> bool {
        !matches!(self, Self::Unsupported)
    }

    pub const ALL: &'static [Self] = &[
        Self::WaylandPresentationTime,
        Self::X11Present,
        Self::MacosCaDisplayLink,
        Self::Unsupported,
    ];
}

// ============================================================================
// Display capability
// ============================================================================

/// What the doctor probes at startup. Operators read this
/// from `ft doctor` to confirm VRR is wired correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayCapability {
    pub api: VrrPlatformApi,
    /// True iff the connected display reports VRR support.
    pub vrr_supported: bool,
    /// Minimum supported refresh rate (Hz). Some panels
    /// floor at 48 Hz; the negotiator clamps below this.
    pub min_rate_hz: u16,
    /// Maximum supported refresh rate (Hz). 120 Hz on
    /// ProMotion / typical FreeSync; 60 Hz on legacy.
    pub max_rate_hz: u16,
}

impl DisplayCapability {
    /// Conservative default: no VRR, 60 Hz fixed.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            api: VrrPlatformApi::Unsupported,
            vrr_supported: false,
            min_rate_hz: 60,
            max_rate_hz: 60,
        }
    }

    /// Clamp a requested rate into the supported range.
    /// Returns the clamped rate + a flag telling whether
    /// clamping fired.
    #[must_use]
    pub fn clamp(self, requested_hz: u16) -> (u16, ClampReason) {
        if !self.vrr_supported {
            return (self.max_rate_hz, ClampReason::VrrUnsupported);
        }
        if requested_hz < self.min_rate_hz {
            (self.min_rate_hz, ClampReason::BelowDisplayFloor)
        } else if requested_hz > self.max_rate_hz {
            (self.max_rate_hz, ClampReason::AboveDisplayCeiling)
        } else {
            (requested_hz, ClampReason::Unclamped)
        }
    }
}

/// Why the requested rate was (or wasn't) clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClampReason {
    Unclamped,
    BelowDisplayFloor,
    AboveDisplayCeiling,
    VrrUnsupported,
}

// ============================================================================
// Decision-tree inputs
// ============================================================================

/// Per-frame inputs the decision tree consumes. The
/// integration gathers these from live state at the top of
/// every paint loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameRateRequestInputs {
    pub live_resize: LiveResizeState,
    /// True iff any of cursor blink, dialog fade, etc. is
    /// active.
    pub animation_active: bool,
    pub battery: BatteryState,
    /// Number of consecutive idle frames (no input event,
    /// no dirty lines). Used to drop the request rate
    /// after sustained idleness.
    pub idle_streak_frames: u32,
    /// True iff the user is recording the screen (the bead's
    /// `vrr_disabled_when_recording` rule fires when set).
    pub recording_active: bool,
    /// Just-arrived input event — the bead's "Idle wake-up:
    /// arrival of input event immediately bumps requested
    /// rate to display max for that frame" rule.
    pub input_event_arrived: bool,
}

impl FrameRateRequestInputs {
    /// Sensible idle baseline for tests.
    #[must_use]
    pub const fn idle_baseline() -> Self {
        Self {
            live_resize: LiveResizeState::Idle,
            animation_active: false,
            battery: BatteryState::Plugged,
            idle_streak_frames: 0,
            recording_active: false,
            input_event_arrived: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveResizeState {
    Idle,
    /// Resize gesture in flight — request display max for
    /// smoothness.
    Resizing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatteryState {
    /// Plugged into AC. No battery cap.
    Plugged,
    /// On battery, charge ≥ 20%. Cap at typing-cadence rate.
    BatteryNormal,
    /// On battery, charge < 20%. Cap at idle-floor rate.
    BatteryLow,
}

// ============================================================================
// Decision tree
// ============================================================================

/// Rate request the decision tree produces, before
/// per-display clamping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameRateRequest {
    pub target_hz: u16,
    pub reason: RequestReason,
}

/// Why this rate was chosen. Surfaced in telemetry so
/// operators can audit the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestReason {
    /// `vrr_disabled_when_recording = true` and recording is
    /// active — defer to a fixed rate.
    RecordingDisablesVrr,
    /// Live-resize gesture wants display max.
    LiveResizeMax,
    /// Just-arrived input event bumps to display max for
    /// this frame.
    InputWakeUp,
    /// Animation active (cursor blink, dialog fade) —
    /// 60 Hz minimum.
    AnimationFloor,
    /// Battery-low cap (charge < 20%).
    BatteryLowCap,
    /// Battery-normal cap (charge ≥ 20%, on battery).
    BatteryNormalCap,
    /// Long idle streak — drop to idle floor (30 Hz).
    IdleFloor,
    /// Default plugged-in idle rate.
    PluggedIdleDefault,
}

/// Compute the per-frame rate request from the inputs +
/// display capability. Pure-logic — same inputs always
/// produce the same request.
///
/// Decision priority (top wins; everything below is
/// clamped):
///
/// 1. Recording active + `vrr_disabled_when_recording` → fixed rate.
/// 2. Live-resize → display max.
/// 3. Input wake-up → display max for one frame.
/// 4. Animation active → max(60 Hz, prior decision).
/// 5. Battery state caps the result.
/// 6. Long idle streak → drop to 30 Hz floor.
/// 7. Default → 60 Hz when plugged + animations off.
#[must_use]
pub fn decide_request(
    inputs: FrameRateRequestInputs,
    display: DisplayCapability,
    vrr_disabled_when_recording: bool,
) -> FrameRateRequest {
    // Highest-priority overrides first.
    if inputs.recording_active && vrr_disabled_when_recording {
        let (hz, _) = display.clamp(display.max_rate_hz);
        return FrameRateRequest {
            target_hz: hz,
            reason: RequestReason::RecordingDisablesVrr,
        };
    }
    if matches!(inputs.live_resize, LiveResizeState::Resizing) {
        let (hz, _) = display.clamp(display.max_rate_hz);
        return FrameRateRequest {
            target_hz: hz,
            reason: RequestReason::LiveResizeMax,
        };
    }
    if inputs.input_event_arrived {
        let (hz, _) = display.clamp(display.max_rate_hz);
        return FrameRateRequest {
            target_hz: hz,
            reason: RequestReason::InputWakeUp,
        };
    }

    // Build a "candidate target" + initial reason. The
    // default-rate branch picks based on battery state so
    // a battery-driven request reports the battery cap as
    // its reason (we're not "Plugged-default" if we're on
    // battery).
    let mut candidate = if inputs.animation_active {
        60 // Animation needs at least 60 Hz for smooth blink.
    } else if inputs.idle_streak_frames >= IDLE_FLOOR_THRESHOLD_FRAMES {
        30 // Sustained idle drops to floor.
    } else {
        match inputs.battery {
            BatteryState::Plugged => 60,
            BatteryState::BatteryNormal => 60,
            BatteryState::BatteryLow => 30,
        }
    };

    let mut reason = if inputs.animation_active {
        RequestReason::AnimationFloor
    } else if inputs.idle_streak_frames >= IDLE_FLOOR_THRESHOLD_FRAMES {
        RequestReason::IdleFloor
    } else {
        match inputs.battery {
            BatteryState::Plugged => RequestReason::PluggedIdleDefault,
            BatteryState::BatteryNormal => RequestReason::BatteryNormalCap,
            BatteryState::BatteryLow => RequestReason::BatteryLowCap,
        }
    };

    // Apply battery cap. Only override `reason` when the
    // cap actually clips — otherwise a binding constraint
    // like AnimationFloor or IdleFloor is the real driver
    // and the doctor's reason_distribution should reflect
    // that.
    let battery_cap_hz: Option<u16> = match inputs.battery {
        BatteryState::Plugged => None,
        BatteryState::BatteryNormal => Some(60),
        BatteryState::BatteryLow => Some(30),
    };
    if let Some(cap) = battery_cap_hz {
        if candidate > cap {
            candidate = cap;
            reason = match inputs.battery {
                BatteryState::Plugged => unreachable!(),
                BatteryState::BatteryNormal => RequestReason::BatteryNormalCap,
                BatteryState::BatteryLow => RequestReason::BatteryLowCap,
            };
        }
    }

    let (hz, _) = display.clamp(candidate);
    FrameRateRequest {
        target_hz: hz,
        reason,
    }
}

/// Threshold beyond which the policy drops to the idle
/// floor (30 Hz). At 60 Hz, 120 frames ≈ 2s of nothing
/// happening.
pub const IDLE_FLOOR_THRESHOLD_FRAMES: u32 = 120;

// ============================================================================
// Negotiation outcome
// ============================================================================

/// What the per-platform API returned after submitting the
/// request. The integration emits one per frame; the doctor
/// folds the distribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum NegotiationOutcome {
    /// Compositor honored the requested rate.
    Honored { rate_hz: u16 },
    /// Compositor returned a different (typically clamped)
    /// rate. Counts as a mismatch in telemetry.
    ClampedByCompositor { requested_hz: u16, actual_hz: u16 },
    /// Negotiation failed mid-call (compositor restart,
    /// transient error). The bead's "retry once, then fall
    /// back" rule applies.
    Failed { rate_hz: u16 },
    /// VRR not supported / disabled — we sent a fixed-rate
    /// Present.
    FellBackToFixedRate { rate_hz: u16 },
}

impl NegotiationOutcome {
    #[must_use]
    pub const fn rate_hz(&self) -> u16 {
        match self {
            Self::Honored { rate_hz }
            | Self::Failed { rate_hz }
            | Self::FellBackToFixedRate { rate_hz } => *rate_hz,
            Self::ClampedByCompositor { actual_hz, .. } => *actual_hz,
        }
    }

    #[must_use]
    pub const fn is_mismatch(&self) -> bool {
        matches!(self, Self::ClampedByCompositor { .. } | Self::Failed { .. })
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the VRR negotiator. Mirrors this
/// session's `*Health` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VrrHealth {
    pub display: DisplayCapability,
    /// True iff at least one frame has been negotiated this
    /// session.
    pub vrr_active: bool,
    /// Total negotiations this session.
    pub negotiations_total: u64,
    /// Mismatched-vs-actual count (the bead's bug-class
    /// detector — high mismatch rate means policy + display
    /// disagree).
    pub mismatched_total: u64,
    /// Failed negotiations (compositor restart class).
    pub failed_total: u64,
    /// Per-bucket histogram of negotiated rate (Hz → count).
    pub rate_distribution: BTreeMap<u16, u64>,
    /// Per-reason histogram of why each rate was chosen.
    pub reason_distribution: BTreeMap<String, u64>,
}

impl VrrHealth {
    #[must_use]
    pub fn baseline(display: DisplayCapability) -> Self {
        Self {
            display,
            vrr_active: false,
            negotiations_total: 0,
            mismatched_total: 0,
            failed_total: 0,
            rate_distribution: BTreeMap::new(),
            reason_distribution: BTreeMap::new(),
        }
    }

    /// Mismatch rate. Returns 0.0 when no negotiations.
    #[must_use]
    pub fn mismatch_rate(&self) -> f64 {
        if self.negotiations_total == 0 {
            return 0.0;
        }
        self.mismatched_total as f64 / self.negotiations_total as f64
    }

    /// True iff the negotiator looks healthy: <= 5%
    /// mismatch rate AND no failed negotiations dominate.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.mismatch_rate() <= 0.05 && self.failed_total <= self.negotiations_total / 100
    }
}

/// Fold one negotiation outcome into the snapshot.
pub fn fold_outcome(
    health: &mut VrrHealth,
    request: FrameRateRequest,
    outcome: NegotiationOutcome,
) {
    health.negotiations_total = health.negotiations_total.saturating_add(1);
    health.vrr_active = true;
    let rate = outcome.rate_hz();
    *health.rate_distribution.entry(rate).or_insert(0) += 1;
    let reason_slug = format!("{:?}", request.reason);
    *health.reason_distribution.entry(reason_slug).or_insert(0) += 1;
    if outcome.is_mismatch() {
        health.mismatched_total = health.mismatched_total.saturating_add(1);
    }
    if matches!(outcome, NegotiationOutcome::Failed { .. }) {
        health.failed_total = health.failed_total.saturating_add(1);
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn promotion() -> DisplayCapability {
        DisplayCapability {
            api: VrrPlatformApi::MacosCaDisplayLink,
            vrr_supported: true,
            min_rate_hz: 24,
            max_rate_hz: 120,
        }
    }

    fn freesync_48_144() -> DisplayCapability {
        DisplayCapability {
            api: VrrPlatformApi::WaylandPresentationTime,
            vrr_supported: true,
            min_rate_hz: 48,
            max_rate_hz: 144,
        }
    }

    // ------------------------------------------------------------------------
    // VrrPlatformApi
    // ------------------------------------------------------------------------

    #[test]
    fn all_apis_have_distinct_slugs() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for a in VrrPlatformApi::ALL {
            assert!(seen.insert(a.slug()));
        }
    }

    #[test]
    fn supports_per_frame_rate_excludes_unsupported() {
        assert!(VrrPlatformApi::WaylandPresentationTime.supports_per_frame_rate());
        assert!(VrrPlatformApi::X11Present.supports_per_frame_rate());
        assert!(VrrPlatformApi::MacosCaDisplayLink.supports_per_frame_rate());
        assert!(!VrrPlatformApi::Unsupported.supports_per_frame_rate());
    }

    // ------------------------------------------------------------------------
    // DisplayCapability::clamp
    // ------------------------------------------------------------------------

    #[test]
    fn unclamped_in_range() {
        let d = promotion();
        let (hz, reason) = d.clamp(60);
        assert_eq!(hz, 60);
        assert_eq!(reason, ClampReason::Unclamped);
    }

    #[test]
    fn clamps_below_floor() {
        let d = freesync_48_144();
        let (hz, reason) = d.clamp(30);
        assert_eq!(hz, 48);
        assert_eq!(reason, ClampReason::BelowDisplayFloor);
    }

    #[test]
    fn clamps_above_ceiling() {
        let d = promotion();
        let (hz, reason) = d.clamp(240);
        assert_eq!(hz, 120);
        assert_eq!(reason, ClampReason::AboveDisplayCeiling);
    }

    #[test]
    fn unsupported_display_returns_max_with_unsupported_reason() {
        let d = DisplayCapability::unsupported();
        let (hz, reason) = d.clamp(60);
        assert_eq!(hz, 60);
        assert_eq!(reason, ClampReason::VrrUnsupported);
    }

    // ------------------------------------------------------------------------
    // decide_request — bead's stated rules
    // ------------------------------------------------------------------------

    #[test]
    fn live_resize_requests_display_max() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            live_resize: LiveResizeState::Resizing,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 120);
        assert_eq!(req.reason, RequestReason::LiveResizeMax);
    }

    #[test]
    fn input_wakeup_requests_display_max() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            input_event_arrived: true,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 120);
        assert_eq!(req.reason, RequestReason::InputWakeUp);
    }

    #[test]
    fn animation_floors_at_60_hz_when_otherwise_idle() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            animation_active: true,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 60);
        assert_eq!(req.reason, RequestReason::AnimationFloor);
    }

    #[test]
    fn long_idle_drops_to_floor() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            idle_streak_frames: IDLE_FLOOR_THRESHOLD_FRAMES + 1,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 30);
        assert_eq!(req.reason, RequestReason::IdleFloor);
    }

    #[test]
    fn battery_low_caps_at_30_hz() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            battery: BatteryState::BatteryLow,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 30);
        assert_eq!(req.reason, RequestReason::BatteryLowCap);
    }

    #[test]
    fn battery_normal_caps_at_60_hz() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            battery: BatteryState::BatteryNormal,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 60);
        assert_eq!(req.reason, RequestReason::BatteryNormalCap);
    }

    #[test]
    fn battery_normal_with_long_idle_reports_idle_floor_not_battery_cap() {
        // Regression: previously, BatteryNormal + long
        // idle reported BatteryNormalCap as the reason
        // even though the cap (60) didn't clip the
        // candidate (30 from IdleFloor). Doctor's
        // reason_distribution misattributed idle frames.
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            battery: BatteryState::BatteryNormal,
            idle_streak_frames: IDLE_FLOOR_THRESHOLD_FRAMES + 1,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 30);
        // The binding constraint is IdleFloor, not the
        // battery cap.
        assert_eq!(req.reason, RequestReason::IdleFloor);
    }

    #[test]
    fn battery_low_with_animation_reports_battery_cap() {
        // Battery cap actually fires when animation
        // requests 60 but BatteryLow caps at 30. Reason
        // = BatteryLowCap (the binding constraint).
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            battery: BatteryState::BatteryLow,
            animation_active: true,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 30);
        assert_eq!(req.reason, RequestReason::BatteryLowCap);
    }

    #[test]
    fn battery_low_with_long_idle_reports_idle_floor_or_battery_cap() {
        // Both candidates land on 30 — reason should
        // surface the dominant policy. Foundation slice:
        // IdleFloor wins (set first, cap doesn't clip
        // since 30 = 30 → not >).
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            battery: BatteryState::BatteryLow,
            idle_streak_frames: IDLE_FLOOR_THRESHOLD_FRAMES + 1,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 30);
        assert_eq!(req.reason, RequestReason::IdleFloor);
    }

    #[test]
    fn plugged_with_animation_does_not_attribute_to_battery() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            animation_active: true,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 60);
        assert_eq!(req.reason, RequestReason::AnimationFloor);
    }

    #[test]
    fn recording_overrides_when_flag_set() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            recording_active: true,
            // Even with live-resize, recording wins.
            live_resize: LiveResizeState::Resizing,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.reason, RequestReason::RecordingDisablesVrr);
    }

    #[test]
    fn recording_does_not_override_when_flag_off() {
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            recording_active: true,
            live_resize: LiveResizeState::Resizing,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, false);
        // LiveResize wins because vrr_disabled_when_recording is off.
        assert_eq!(req.reason, RequestReason::LiveResizeMax);
    }

    #[test]
    fn input_wakeup_beats_long_idle() {
        // Bead's "Idle wake-up" rule: input event bumps to
        // display max for that frame even if we were deep
        // in idle-floor territory.
        let d = promotion();
        let inputs = FrameRateRequestInputs {
            idle_streak_frames: 600,
            input_event_arrived: true,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 120);
        assert_eq!(req.reason, RequestReason::InputWakeUp);
    }

    #[test]
    fn freesync_floor_clamps_idle_request() {
        // 48 Hz panel: request 30 Hz idle, clamp lifts to 48.
        let d = freesync_48_144();
        let inputs = FrameRateRequestInputs {
            idle_streak_frames: IDLE_FLOOR_THRESHOLD_FRAMES + 10,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 48);
    }

    #[test]
    fn unsupported_display_returns_max_regardless_of_inputs() {
        let d = DisplayCapability::unsupported();
        let inputs = FrameRateRequestInputs {
            idle_streak_frames: 600,
            battery: BatteryState::BatteryLow,
            ..FrameRateRequestInputs::idle_baseline()
        };
        let req = decide_request(inputs, d, true);
        assert_eq!(req.target_hz, 60); // unsupported max
    }

    // ------------------------------------------------------------------------
    // NegotiationOutcome
    // ------------------------------------------------------------------------

    #[test]
    fn outcome_rate_hz_extraction() {
        assert_eq!(NegotiationOutcome::Honored { rate_hz: 60 }.rate_hz(), 60);
        assert_eq!(
            NegotiationOutcome::ClampedByCompositor {
                requested_hz: 120,
                actual_hz: 90
            }
            .rate_hz(),
            90
        );
        assert_eq!(NegotiationOutcome::Failed { rate_hz: 60 }.rate_hz(), 60);
        assert_eq!(
            NegotiationOutcome::FellBackToFixedRate { rate_hz: 60 }.rate_hz(),
            60
        );
    }

    #[test]
    fn mismatch_predicate_correctness() {
        assert!(!NegotiationOutcome::Honored { rate_hz: 60 }.is_mismatch());
        assert!(
            NegotiationOutcome::ClampedByCompositor {
                requested_hz: 120,
                actual_hz: 90
            }
            .is_mismatch()
        );
        assert!(NegotiationOutcome::Failed { rate_hz: 60 }.is_mismatch());
        assert!(!NegotiationOutcome::FellBackToFixedRate { rate_hz: 60 }.is_mismatch());
    }

    // ------------------------------------------------------------------------
    // VrrHealth
    // ------------------------------------------------------------------------

    #[test]
    fn baseline_health_safe_with_no_negotiations() {
        let h = VrrHealth::baseline(promotion());
        assert!(h.is_safe());
        assert!(!h.vrr_active);
        assert_eq!(h.mismatch_rate(), 0.0);
    }

    #[test]
    fn fold_outcome_increments_distribution() {
        let mut h = VrrHealth::baseline(promotion());
        let req = FrameRateRequest {
            target_hz: 120,
            reason: RequestReason::LiveResizeMax,
        };
        fold_outcome(&mut h, req, NegotiationOutcome::Honored { rate_hz: 120 });
        fold_outcome(&mut h, req, NegotiationOutcome::Honored { rate_hz: 120 });
        let req2 = FrameRateRequest {
            target_hz: 30,
            reason: RequestReason::IdleFloor,
        };
        fold_outcome(&mut h, req2, NegotiationOutcome::Honored { rate_hz: 30 });
        assert_eq!(h.negotiations_total, 3);
        assert!(h.vrr_active);
        assert_eq!(h.rate_distribution.get(&120), Some(&2));
        assert_eq!(h.rate_distribution.get(&30), Some(&1));
        assert_eq!(h.mismatched_total, 0);
    }

    #[test]
    fn mismatch_rate_high_marks_unsafe() {
        let mut h = VrrHealth::baseline(promotion());
        let req = FrameRateRequest {
            target_hz: 120,
            reason: RequestReason::LiveResizeMax,
        };
        // 10 negotiations, 6 mismatched → 60% mismatch rate.
        for _ in 0..6 {
            fold_outcome(
                &mut h,
                req,
                NegotiationOutcome::ClampedByCompositor {
                    requested_hz: 120,
                    actual_hz: 60,
                },
            );
        }
        for _ in 0..4 {
            fold_outcome(&mut h, req, NegotiationOutcome::Honored { rate_hz: 120 });
        }
        assert!(!h.is_safe());
        assert!(h.mismatch_rate() > 0.5);
    }

    #[test]
    fn low_failure_rate_within_budget_stays_safe() {
        let mut h = VrrHealth::baseline(promotion());
        let req = FrameRateRequest {
            target_hz: 60,
            reason: RequestReason::PluggedIdleDefault,
        };
        // 1000 negotiations, 5 failures → 0.5% failure rate.
        for _ in 0..995 {
            fold_outcome(&mut h, req, NegotiationOutcome::Honored { rate_hz: 60 });
        }
        for _ in 0..5 {
            fold_outcome(&mut h, req, NegotiationOutcome::Failed { rate_hz: 60 });
        }
        // Mismatch rate 0.5% is under 5% bound; failures 5/1000 is exactly 5/100 which is the bound.
        assert!(h.is_safe());
    }

    #[test]
    fn vrr_health_serde_roundtrips() {
        let mut h = VrrHealth::baseline(promotion());
        let req = FrameRateRequest {
            target_hz: 120,
            reason: RequestReason::InputWakeUp,
        };
        fold_outcome(&mut h, req, NegotiationOutcome::Honored { rate_hz: 120 });
        let json = serde_json::to_string(&h).unwrap();
        let parsed: VrrHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, h);
    }
}
