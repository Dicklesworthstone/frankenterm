//! Per-platform display probe schema (ft-zgb5v / ft-0thg2.cont).
//!
//! The integration's startup probe queries the OS for display
//! capabilities — refresh rate range, VRR support, sparse-
//! residency feature flags, recording state. The actual
//! syscalls are platform-specific (`CADisplayLink`,
//! `DwmGetCompositionTimingInfo`, `/sys/class/drm/`,
//! `wp_tearing_control_v1`). This module ships the
//! **pure-data schema** the probes populate — substrate-
//! validated, integration-populated.
//!
//! Cross-link: feeds into
//! `display_pipeline_ci_matrix.rs::IdleBenchSummary` and
//! `wayland_direct_scanout.rs::ScanoutInputs`.
//!
//! ## What this module ships
//!
//! - [`PlatformOs`] 4-variant (`MacOs / Linux / Windows /
//!   Other`) with `is_unix` / `is_windows` predicates.
//! - [`DisplayProbeResult`] aggregate of refresh + VRR +
//!   recording state, plus probe metadata
//!   (`probe_succeeded` / `probe_skipped_reason`).
//! - [`RefreshRangeProbe`] / [`VrrProbe`] / [`RecordingProbe`]
//!   per-axis probe results with confidence levels.
//! - [`ProbeConfidence`] 3-variant (`Authoritative` / `Heuristic`
//!   / `Unknown`) — the integration's probe reports its own
//!   confidence so the substrate can decide fallback paths.
//! - [`merge_probe_results`] — pure-logic merge of two probe
//!   runs (e.g., adapter probe + sysfs probe). Picks the
//!   higher-confidence value per field.
//! - [`DisplayProbeTelemetry`] bead's per-session counters
//!   (probes_succeeded / probes_failed / heuristic_fallbacks).
//!
//! ## What is deferred to the integration
//!
//! - macOS: `CADisplayLink.preferredFrameRateRange` +
//!   `MTLDevice` adapter info + `ScreenCaptureKit` for
//!   recording state.
//! - Linux:`/sys/class/drm/*/edid` + `XDG_SESSION_TYPE` +
//!   `wp_tearing_control_v1` advertisement parsing +
//!   PipeWire portal scan for recording.
//! - Windows: `DwmGetCompositionTimingInfo` +
//!   `DXGI_ADAPTER_DESC` + `IGameBarServices` for recording.
//! - Wiring into ft-mpc9b.5.3 idle detector + ft-2okh0.2.2
//!   scanout fallback (`ScanoutSupport::Native` etc.).

#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ============================================================================
// PlatformOs
// ============================================================================

/// Top-level platform discriminator. The integration's
/// `cfg!(target_os = ...)` path maps to one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformOs {
    MacOs,
    Linux,
    Windows,
    /// Anything else (BSD, illumos, embedded). Probes still
    /// run but heuristic paths are limited.
    Other,
}

impl PlatformOs {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MacOs => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Other => "other",
        }
    }

    #[must_use]
    pub const fn is_unix(self) -> bool {
        matches!(self, Self::MacOs | Self::Linux | Self::Other)
    }

    #[must_use]
    pub const fn is_windows(self) -> bool {
        matches!(self, Self::Windows)
    }
}

// ============================================================================
// ProbeConfidence
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeConfidence {
    /// Probe couldn't determine the value — substrate falls
    /// back to defaults.
    Unknown,
    /// Probe inferred via heuristic (e.g., reading EDID with
    /// best-guess parser). Caller should treat as advisory.
    Heuristic,
    /// Probe got the answer directly from the OS API. Highest
    /// confidence; substrate trusts.
    Authoritative,
}

impl ProbeConfidence {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Heuristic => "heuristic",
            Self::Authoritative => "authoritative",
        }
    }

    /// Pick the higher-confidence value when merging two
    /// probe runs.
    #[must_use]
    pub fn higher(self, other: Self) -> Self {
        if self >= other { self } else { other }
    }
}

// ============================================================================
// RefreshRangeProbe
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RefreshRangeProbe {
    /// Maximum supported refresh rate in Hz. 0 when unknown.
    pub max_hz: u32,
    /// Minimum supported refresh rate (for VRR displays;
    /// 0 = same as max for fixed-rate displays).
    pub min_hz: u32,
    /// Display's current preferred refresh rate.
    pub preferred_hz: u32,
    pub confidence: ProbeConfidence,
}

impl RefreshRangeProbe {
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            max_hz: 0,
            min_hz: 0,
            preferred_hz: 0,
            confidence: ProbeConfidence::Unknown,
        }
    }

    /// Whether the display has a meaningful (non-zero) VRR
    /// range. Fixed-rate displays return false.
    #[must_use]
    pub const fn has_vrr_range(&self) -> bool {
        self.min_hz > 0 && self.max_hz > self.min_hz
    }

    /// Whether the probe succeeded with at least heuristic
    /// confidence.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        !matches!(self.confidence, ProbeConfidence::Unknown)
    }
}

// ============================================================================
// VrrProbe
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VrrSupportLevel {
    /// VRR available and operator opted in.
    EnabledAndAvailable,
    /// VRR available but operator opted out (config flag) or
    /// no fullscreen surface present.
    Available,
    /// Hardware doesn't support VRR.
    NotSupported,
    /// Probe couldn't determine.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VrrProbe {
    pub level: VrrSupportLevel,
    pub confidence: ProbeConfidence,
}

impl VrrProbe {
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            level: VrrSupportLevel::Unknown,
            confidence: ProbeConfidence::Unknown,
        }
    }

    #[must_use]
    pub const fn is_active(&self) -> bool {
        matches!(self.level, VrrSupportLevel::EnabledAndAvailable)
    }
}

// ============================================================================
// RecordingProbe
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingState {
    Active,
    Inactive,
    /// Probe couldn't determine — substrate's safety default
    /// is to assume Active so frame_dedup force-presents.
    UnknownAssumeActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecordingProbe {
    pub state: RecordingState,
    pub confidence: ProbeConfidence,
}

impl RecordingProbe {
    #[must_use]
    pub const fn unknown() -> Self {
        Self {
            state: RecordingState::UnknownAssumeActive,
            confidence: ProbeConfidence::Unknown,
        }
    }

    /// Whether the integration should force-present (treat
    /// recording as active).
    #[must_use]
    pub const fn forces_present(&self) -> bool {
        matches!(
            self.state,
            RecordingState::Active | RecordingState::UnknownAssumeActive
        )
    }
}

// ============================================================================
// DisplayProbeResult — aggregate
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DisplayProbeResult {
    pub platform: PlatformOs,
    pub refresh: RefreshRangeProbe,
    pub vrr: VrrProbe,
    pub recording: RecordingProbe,
    /// Whether *all three* sub-probes returned at least
    /// heuristic confidence.
    pub probe_succeeded: bool,
    /// Reason the probe was skipped (Unknown across all
    /// sub-probes), `None` when probe ran.
    pub probe_skipped_reason: Option<String>,
}

impl DisplayProbeResult {
    /// Construct a fully-unknown result for the case where
    /// the integration couldn't run any probe (e.g., no
    /// graphical session).
    #[must_use]
    pub fn unknown(platform: PlatformOs, reason: impl Into<String>) -> Self {
        Self {
            platform,
            refresh: RefreshRangeProbe::unknown(),
            vrr: VrrProbe::unknown(),
            recording: RecordingProbe::unknown(),
            probe_succeeded: false,
            probe_skipped_reason: Some(reason.into()),
        }
    }

    /// Whether all three sub-probes returned at least
    /// heuristic confidence. Recomputes from the current sub-
    /// probe state — useful after manual edits.
    pub fn recompute_succeeded(&mut self) {
        self.probe_succeeded = self.refresh.succeeded()
            && !matches!(self.vrr.confidence, ProbeConfidence::Unknown)
            && !matches!(self.recording.confidence, ProbeConfidence::Unknown);
    }

    /// Sub-probes with at least heuristic confidence (1..=3).
    #[must_use]
    pub fn confident_probe_count(&self) -> u32 {
        let r = u32::from(self.refresh.succeeded());
        let v = u32::from(!matches!(self.vrr.confidence, ProbeConfidence::Unknown));
        let rec = u32::from(!matches!(
            self.recording.confidence,
            ProbeConfidence::Unknown
        ));
        r + v + rec
    }
}

// ============================================================================
// merge_probe_results
// ============================================================================

/// Merge two probe runs into one, preferring the higher-
/// confidence value per axis. Useful when the integration
/// runs both an adapter-info probe (Authoritative for VRR
/// hardware) and a sysfs probe (Authoritative for refresh
/// range) — the merged result has the best-of-both.
///
/// `primary` and `secondary` must agree on `platform`;
/// returns `None` if they don't (defensive — substrate
/// refuses to merge cross-platform results).
#[must_use]
pub fn merge_probe_results(
    primary: &DisplayProbeResult,
    secondary: &DisplayProbeResult,
) -> Option<DisplayProbeResult> {
    if primary.platform != secondary.platform {
        return None;
    }
    let refresh = if primary.refresh.confidence >= secondary.refresh.confidence {
        primary.refresh
    } else {
        secondary.refresh
    };
    let vrr = if primary.vrr.confidence >= secondary.vrr.confidence {
        primary.vrr
    } else {
        secondary.vrr
    };
    let recording = if primary.recording.confidence >= secondary.recording.confidence {
        primary.recording
    } else {
        secondary.recording
    };
    let mut merged = DisplayProbeResult {
        platform: primary.platform,
        refresh,
        vrr,
        recording,
        probe_succeeded: false,
        probe_skipped_reason: None,
    };
    merged.recompute_succeeded();
    if merged.confident_probe_count() == 0 {
        merged.probe_skipped_reason = merge_skipped_reasons(
            &primary.probe_skipped_reason,
            &secondary.probe_skipped_reason,
        );
    }
    Some(merged)
}

fn merge_skipped_reasons(primary: &Option<String>, secondary: &Option<String>) -> Option<String> {
    match (primary.as_deref(), secondary.as_deref()) {
        (Some(a), Some(b)) if a == b => Some(a.to_string()),
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => Some("all display probes returned unknown confidence".to_string()),
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayProbeTelemetry {
    pub probes_run_total: u64,
    pub probes_fully_succeeded: u64,
    pub probes_partial: u64,
    pub probes_failed: u64,
    pub heuristic_fallbacks: u64,
    pub recording_unknown_assume_active: u64,
    pub merges_succeeded: u64,
    pub merges_rejected_platform_mismatch: u64,
}

impl DisplayProbeTelemetry {
    pub fn record_probe(&mut self, result: &DisplayProbeResult) {
        self.probes_run_total = self.probes_run_total.saturating_add(1);
        let n = result.confident_probe_count();
        match n {
            3 => {
                self.probes_fully_succeeded = self.probes_fully_succeeded.saturating_add(1);
            }
            1 | 2 => {
                self.probes_partial = self.probes_partial.saturating_add(1);
            }
            _ => {
                self.probes_failed = self.probes_failed.saturating_add(1);
            }
        }
        if matches!(result.refresh.confidence, ProbeConfidence::Heuristic)
            || matches!(result.vrr.confidence, ProbeConfidence::Heuristic)
            || matches!(result.recording.confidence, ProbeConfidence::Heuristic)
        {
            self.heuristic_fallbacks = self.heuristic_fallbacks.saturating_add(1);
        }
        if matches!(result.recording.state, RecordingState::UnknownAssumeActive) {
            self.recording_unknown_assume_active =
                self.recording_unknown_assume_active.saturating_add(1);
        }
    }

    pub fn record_merge(&mut self, succeeded: bool) {
        if succeeded {
            self.merges_succeeded = self.merges_succeeded.saturating_add(1);
        } else {
            self.merges_rejected_platform_mismatch =
                self.merges_rejected_platform_mismatch.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_refresh(max: u32, min: u32, preferred: u32) -> RefreshRangeProbe {
        RefreshRangeProbe {
            max_hz: max,
            min_hz: min,
            preferred_hz: preferred,
            confidence: ProbeConfidence::Authoritative,
        }
    }

    fn auth_vrr(level: VrrSupportLevel) -> VrrProbe {
        VrrProbe {
            level,
            confidence: ProbeConfidence::Authoritative,
        }
    }

    fn auth_recording(state: RecordingState) -> RecordingProbe {
        RecordingProbe {
            state,
            confidence: ProbeConfidence::Authoritative,
        }
    }

    // ----------------------------------------------------------------
    // PlatformOs
    // ----------------------------------------------------------------

    #[test]
    fn platform_label_stable() {
        assert_eq!(PlatformOs::MacOs.label(), "macos");
        assert_eq!(PlatformOs::Linux.label(), "linux");
        assert_eq!(PlatformOs::Windows.label(), "windows");
        assert_eq!(PlatformOs::Other.label(), "other");
    }

    #[test]
    fn platform_unix_windows_classification() {
        assert!(PlatformOs::MacOs.is_unix());
        assert!(PlatformOs::Linux.is_unix());
        assert!(PlatformOs::Other.is_unix());
        assert!(!PlatformOs::Windows.is_unix());
        assert!(PlatformOs::Windows.is_windows());
    }

    // ----------------------------------------------------------------
    // ProbeConfidence
    // ----------------------------------------------------------------

    #[test]
    fn confidence_ordering() {
        assert!(ProbeConfidence::Authoritative > ProbeConfidence::Heuristic);
        assert!(ProbeConfidence::Heuristic > ProbeConfidence::Unknown);
    }

    #[test]
    fn confidence_higher_picks_max() {
        assert_eq!(
            ProbeConfidence::Heuristic.higher(ProbeConfidence::Unknown),
            ProbeConfidence::Heuristic,
        );
        assert_eq!(
            ProbeConfidence::Heuristic.higher(ProbeConfidence::Authoritative),
            ProbeConfidence::Authoritative,
        );
        assert_eq!(
            ProbeConfidence::Authoritative.higher(ProbeConfidence::Authoritative),
            ProbeConfidence::Authoritative,
        );
    }

    // ----------------------------------------------------------------
    // RefreshRangeProbe
    // ----------------------------------------------------------------

    #[test]
    fn refresh_unknown_doesnt_succeed() {
        let r = RefreshRangeProbe::unknown();
        assert!(!r.succeeded());
        assert!(!r.has_vrr_range());
    }

    #[test]
    fn refresh_fixed_rate_no_vrr_range() {
        let r = auth_refresh(60, 60, 60);
        assert!(r.succeeded());
        assert!(!r.has_vrr_range());
    }

    #[test]
    fn refresh_vrr_range_detected() {
        let r = auth_refresh(120, 48, 60);
        assert!(r.succeeded());
        assert!(r.has_vrr_range());
    }

    #[test]
    fn refresh_min_zero_doesnt_count_as_vrr() {
        // Sentinel: min=0 means fixed-rate, even if max > 0.
        let r = auth_refresh(120, 0, 60);
        assert!(!r.has_vrr_range());
    }

    // ----------------------------------------------------------------
    // VrrProbe + RecordingProbe
    // ----------------------------------------------------------------

    #[test]
    fn vrr_active_only_when_enabled_and_available() {
        let probe = auth_vrr(VrrSupportLevel::EnabledAndAvailable);
        assert!(probe.is_active());
        let probe = auth_vrr(VrrSupportLevel::Available);
        assert!(!probe.is_active());
        let probe = auth_vrr(VrrSupportLevel::NotSupported);
        assert!(!probe.is_active());
    }

    #[test]
    fn recording_unknown_forces_present_per_safety_default() {
        let probe = RecordingProbe::unknown();
        assert!(probe.forces_present()); // safety default
    }

    #[test]
    fn recording_active_forces_present() {
        assert!(auth_recording(RecordingState::Active).forces_present());
    }

    #[test]
    fn recording_inactive_does_not_force_present() {
        assert!(!auth_recording(RecordingState::Inactive).forces_present());
    }

    // ----------------------------------------------------------------
    // DisplayProbeResult
    // ----------------------------------------------------------------

    #[test]
    fn result_unknown_carries_skip_reason() {
        let r = DisplayProbeResult::unknown(PlatformOs::MacOs, "no graphical session");
        assert_eq!(r.platform, PlatformOs::MacOs);
        assert!(!r.probe_succeeded);
        assert_eq!(
            r.probe_skipped_reason.as_deref(),
            Some("no graphical session"),
        );
        assert_eq!(r.confident_probe_count(), 0);
    }

    #[test]
    fn result_confident_count_three_when_all_authoritative() {
        let r = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(120, 48, 60),
            vrr: auth_vrr(VrrSupportLevel::EnabledAndAvailable),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: true,
            probe_skipped_reason: None,
        };
        assert_eq!(r.confident_probe_count(), 3);
    }

    #[test]
    fn result_confident_count_partial_when_some_unknown() {
        let r = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(60, 0, 60),
            vrr: VrrProbe::unknown(),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: false,
            probe_skipped_reason: None,
        };
        assert_eq!(r.confident_probe_count(), 2);
    }

    #[test]
    fn result_recompute_succeeded() {
        let mut r = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(60, 0, 60),
            vrr: auth_vrr(VrrSupportLevel::EnabledAndAvailable),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: false, // intentionally wrong
            probe_skipped_reason: None,
        };
        r.recompute_succeeded();
        assert!(r.probe_succeeded);
    }

    // ----------------------------------------------------------------
    // merge_probe_results
    // ----------------------------------------------------------------

    #[test]
    fn merge_picks_higher_confidence_per_axis() {
        let primary = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(120, 48, 60), // Authoritative
            vrr: VrrProbe {
                level: VrrSupportLevel::Available,
                confidence: ProbeConfidence::Heuristic,
            },
            recording: RecordingProbe::unknown(),
            probe_succeeded: false,
            probe_skipped_reason: None,
        };
        let secondary = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: RefreshRangeProbe {
                max_hz: 60,
                min_hz: 60,
                preferred_hz: 60,
                confidence: ProbeConfidence::Heuristic,
            },
            vrr: auth_vrr(VrrSupportLevel::EnabledAndAvailable), // Authoritative
            recording: auth_recording(RecordingState::Inactive), // Authoritative
            probe_succeeded: false,
            probe_skipped_reason: None,
        };

        let merged = merge_probe_results(&primary, &secondary).expect("same platform");
        // Refresh: primary is Authoritative.
        assert_eq!(merged.refresh.max_hz, 120);
        assert_eq!(merged.refresh.min_hz, 48);
        // VRR: secondary is Authoritative.
        assert_eq!(merged.vrr.level, VrrSupportLevel::EnabledAndAvailable);
        // Recording: secondary is Authoritative (primary was Unknown).
        assert_eq!(merged.recording.state, RecordingState::Inactive);
        // probe_succeeded recomputed.
        assert!(merged.probe_succeeded);
    }

    #[test]
    fn merge_rejects_cross_platform() {
        let mac = DisplayProbeResult::unknown(PlatformOs::MacOs, "test");
        let linux = DisplayProbeResult::unknown(PlatformOs::Linux, "test");
        assert!(merge_probe_results(&mac, &linux).is_none());
    }

    #[test]
    fn merge_unknown_with_unknown_stays_unknown() {
        let r1 = DisplayProbeResult::unknown(PlatformOs::Linux, "1");
        let r2 = DisplayProbeResult::unknown(PlatformOs::Linux, "2");
        let merged = merge_probe_results(&r1, &r2).unwrap();
        assert_eq!(merged.confident_probe_count(), 0);
        assert!(!merged.probe_succeeded);
        assert_eq!(merged.probe_skipped_reason.as_deref(), Some("1; 2"));
    }

    #[test]
    fn merge_clears_skip_reason_once_any_probe_succeeds() {
        let primary = DisplayProbeResult::unknown(PlatformOs::Linux, "no graphical session");
        let secondary = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(60, 0, 60),
            vrr: VrrProbe::unknown(),
            recording: RecordingProbe::unknown(),
            probe_succeeded: false,
            probe_skipped_reason: Some("partial refresh-only fallback".to_string()),
        };

        let merged = merge_probe_results(&primary, &secondary).unwrap();
        assert_eq!(merged.confident_probe_count(), 1);
        assert!(!merged.probe_succeeded);
        assert_eq!(merged.probe_skipped_reason, None);
    }

    // ----------------------------------------------------------------
    // DisplayProbeTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_records_full_success() {
        let mut t = DisplayProbeTelemetry::default();
        let r = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(60, 60, 60),
            vrr: auth_vrr(VrrSupportLevel::Available),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: true,
            probe_skipped_reason: None,
        };
        t.record_probe(&r);
        assert_eq!(t.probes_run_total, 1);
        assert_eq!(t.probes_fully_succeeded, 1);
        assert_eq!(t.probes_partial, 0);
        assert_eq!(t.heuristic_fallbacks, 0);
        assert_eq!(t.recording_unknown_assume_active, 0);
    }

    #[test]
    fn telemetry_records_partial_with_heuristic_fallback() {
        let mut t = DisplayProbeTelemetry::default();
        let r = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: RefreshRangeProbe {
                max_hz: 60,
                min_hz: 0,
                preferred_hz: 60,
                confidence: ProbeConfidence::Heuristic,
            },
            vrr: VrrProbe::unknown(),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: false,
            probe_skipped_reason: None,
        };
        t.record_probe(&r);
        assert_eq!(t.probes_partial, 1);
        assert_eq!(t.heuristic_fallbacks, 1);
        assert_eq!(t.recording_unknown_assume_active, 0);
    }

    #[test]
    fn telemetry_records_unknown_recording_assume_active() {
        let mut t = DisplayProbeTelemetry::default();
        let r = DisplayProbeResult::unknown(PlatformOs::Linux, "test");
        t.record_probe(&r);
        assert_eq!(t.recording_unknown_assume_active, 1);
        assert_eq!(t.probes_failed, 1);
    }

    #[test]
    fn telemetry_records_merge_outcomes() {
        let mut t = DisplayProbeTelemetry::default();
        t.record_merge(true);
        t.record_merge(true);
        t.record_merge(false);
        assert_eq!(t.merges_succeeded, 2);
        assert_eq!(t.merges_rejected_platform_mismatch, 1);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_macos_apple_silicon_probe() {
        // M2 MacBook: CADisplayLink reports 60-120 Hz ProMotion;
        // ScreenCaptureKit reports inactive.
        let r = DisplayProbeResult {
            platform: PlatformOs::MacOs,
            refresh: auth_refresh(120, 60, 120),
            vrr: auth_vrr(VrrSupportLevel::Available),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: true,
            probe_skipped_reason: None,
        };
        assert_eq!(r.confident_probe_count(), 3);
        assert!(r.refresh.has_vrr_range());
        assert!(!r.recording.forces_present());
    }

    #[test]
    fn scenario_linux_obs_recording_active() {
        // Linux + PipeWire portal reports OBS scanning.
        let r = DisplayProbeResult {
            platform: PlatformOs::Linux,
            refresh: auth_refresh(144, 144, 144),
            vrr: auth_vrr(VrrSupportLevel::EnabledAndAvailable),
            recording: auth_recording(RecordingState::Active),
            probe_succeeded: true,
            probe_skipped_reason: None,
        };
        assert!(r.recording.forces_present());
        assert!(r.vrr.is_active());
    }

    #[test]
    fn scenario_two_probe_merge_mac() {
        // macOS: NSScreen probe gets refresh range; ScreenCaptureKit
        // probe gets recording. Merge composes them.
        let nsscreen = DisplayProbeResult {
            platform: PlatformOs::MacOs,
            refresh: auth_refresh(120, 60, 120),
            vrr: auth_vrr(VrrSupportLevel::Available),
            recording: RecordingProbe::unknown(),
            probe_succeeded: false,
            probe_skipped_reason: None,
        };
        let scstream = DisplayProbeResult {
            platform: PlatformOs::MacOs,
            refresh: RefreshRangeProbe::unknown(),
            vrr: VrrProbe::unknown(),
            recording: auth_recording(RecordingState::Inactive),
            probe_succeeded: false,
            probe_skipped_reason: None,
        };
        let merged = merge_probe_results(&nsscreen, &scstream).unwrap();
        assert!(merged.probe_succeeded);
        assert_eq!(merged.refresh.max_hz, 120);
        assert_eq!(merged.recording.state, RecordingState::Inactive);
    }

    #[test]
    fn scenario_no_graphical_session_probe_skipped() {
        // ssh session, no DISPLAY / WAYLAND_DISPLAY.
        let r = DisplayProbeResult::unknown(
            PlatformOs::Linux,
            "no DISPLAY/WAYLAND_DISPLAY environment variable",
        );
        assert!(!r.probe_succeeded);
        assert_eq!(r.confident_probe_count(), 0);
        assert!(r.recording.forces_present()); // safety default
    }
}
