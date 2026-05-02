//! Display-pipeline CI matrix + 24h-bench acceptance substrate
//! (ft-0thg2 / BR-TERM-EMULATOR-UPLIFT-2.2.cont2).
//!
//! Pure-logic substrate covering the bead's substrate-shaped
//! pieces:
//!
//! - The per-compositor CI matrix schema (which Wayland/X11
//!   compositor combinations must pass which tests).
//! - 24h-bench acceptance predicate against the bead's
//!   RQ-S5/RQ-S7/RQ-S12 SLOs (dedup_rate ≥99%, battery drain
//!   ≤5% on M2 idle, ft doctor reports refresh rate).
//! - Recording-active detection schema (enum + classify; the
//!   actual ScreenCaptureKit/PipeWire probe is integration).
//! - DRM vendor-modifier allowlist for direct-scanout.
//! - Per-release attestation envelope tying it all together.
//!
//! VRR-mechanism enums + scanout-eligibility predicate +
//! decide_present already shipped via ft-4vxjb in
//! `display_pipeline.rs` (commit 3738540e9). Frame-dedup
//! shipped via 5c54dbf83 in `frame_dedup.rs`.
//!
//! ## What this module ships
//!
//! - `Compositor` 6-variant covering the bead's CI matrix
//!   (`MutterWayland / KwinWayland / Sway / Hyprland / I3X11
//!   / XfwmX11`).
//! - `CiTestKind` 5-variant per the bead's "## CI matrix"
//!   section (VrrNegotiation / DirectScanout /
//!   FrameDedupCorrectness / A11yPromptness /
//!   RecordingCompatibility).
//! - `CiCellOutcome` 3-variant (Pass / Fail / NotApplicable).
//! - `CiMatrixCell` (compositor × test) result.
//! - `CiMatrix` aggregate with `meets_release_bar` predicate
//!   per the bead's acceptance criteria.
//! - `BenchAcceptanceConfig` — RQ-S5 (≥99% dedup rate),
//!   RQ-S7 (≤5% battery drain over 24h), RQ-S12 thresholds.
//! - `IdleBenchSummary` 24h idle-bench result with
//!   `meets_acceptance` predicate.
//! - `RecordingActiveProbe` 4-variant
//!   (`MacOsScreenCaptureKit / LinuxPipeWirePortal / Disabled
//!   / Unknown`); `classify_recording_state` decision tree.
//! - `DrmModifier(u64)` opaque + `is_scanout_eligible`
//!   predicate against an operator-tunable allowlist.
//! - `DisplayPipelineAttestation` schema for
//!   `docs/attestations/display-pipeline-<version>.json`
//!   (cross-link BR-RC-FOUNDATION.G3.1).
//! - `DisplayPipelineCiTelemetry` per-session counters.
//!
//! ## What is deferred to ft-0thg2 follow-up
//!
//! - Actual platform probes (CADisplayLink / wp_tearing_control_v1
//!   / DwmGetCompositionTimingInfo).
//! - paint.rs Present-wiring with `decide_present` dispatch.
//! - 24h bench harness at `crates/frankenterm-core/benches/
//!   idle_battery_drain.rs`.
//! - Per-compositor CI test runners.
//! - ft doctor render of `VrrSupport` + `ScanoutEligibility`
//!   + `ForcePresent` + `dedup_rate_pct`.
//! - ScreenCaptureKit / PipeWire detection wiring.

#![allow(dead_code)]

// ============================================================================
// Compositor — the bead's CI matrix axis
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compositor {
    /// GNOME's mutter on Wayland.
    MutterWayland,
    /// KDE's KWin on Wayland.
    KwinWayland,
    /// Sway (wlroots-based, primary tearing-control test bench).
    Sway,
    /// Hyprland (wlroots-based).
    Hyprland,
    /// i3 on X11.
    I3X11,
    /// Xfwm on X11 (lower priority but on the matrix).
    XfwmX11,
}

impl Compositor {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::MutterWayland => "mutter_wayland",
            Self::KwinWayland => "kwin_wayland",
            Self::Sway => "sway",
            Self::Hyprland => "hyprland",
            Self::I3X11 => "i3_x11",
            Self::XfwmX11 => "xfwm_x11",
        }
    }

    #[must_use]
    pub const fn is_wayland(self) -> bool {
        matches!(
            self,
            Self::MutterWayland | Self::KwinWayland | Self::Sway | Self::Hyprland
        )
    }

    #[must_use]
    pub const fn is_x11(self) -> bool {
        !self.is_wayland()
    }

    /// All compositors that must pass the bead's CI matrix.
    #[must_use]
    pub const fn all() -> [Self; 6] {
        [
            Self::MutterWayland,
            Self::KwinWayland,
            Self::Sway,
            Self::Hyprland,
            Self::I3X11,
            Self::XfwmX11,
        ]
    }
}

// ============================================================================
// CiTestKind — the bead's CI matrix axis
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CiTestKind {
    /// VRR negotiation per compositor (table from substrate).
    VrrNegotiation,
    /// Direct-scanout test on Wayland fullscreen — zero
    /// compositor copies (NotApplicable on X11).
    DirectScanout,
    /// Frame-dedup correctness proptest: identical input
    /// bytes → same FrameHash.
    FrameDedupCorrectness,
    /// A11Y promptness: assistive-tech announcements not
    /// delayed by dedup (force_present fires within 16 ms
    /// of AT-SPI activity).
    A11yPromptness,
    /// Recording compatibility: when recording_active, no
    /// frames elided.
    RecordingCompatibility,
}

impl CiTestKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::VrrNegotiation => "vrr_negotiation",
            Self::DirectScanout => "direct_scanout",
            Self::FrameDedupCorrectness => "frame_dedup_correctness",
            Self::A11yPromptness => "a11y_promptness",
            Self::RecordingCompatibility => "recording_compatibility",
        }
    }

    /// Whether this test applies on the given compositor.
    /// `DirectScanout` is Wayland-only.
    #[must_use]
    pub const fn applies_to(self, compositor: Compositor) -> bool {
        match self {
            Self::DirectScanout => compositor.is_wayland(),
            _ => true,
        }
    }
}

// ============================================================================
// CiMatrixCell + CiMatrix
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CiCellOutcome {
    Pass,
    Fail,
    /// Test isn't applicable on this compositor (e.g.,
    /// DirectScanout on X11).
    NotApplicable,
}

impl CiCellOutcome {
    /// Whether this cell counts as a release-blocking failure.
    #[must_use]
    pub const fn blocks_release(self) -> bool {
        matches!(self, Self::Fail)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CiMatrixCell {
    pub compositor: Compositor,
    pub test: CiTestKind,
    pub outcome: CiCellOutcome,
}

/// CI release-gate matrix.
///
/// **Release-gate integrity**: `cells` is `pub(crate)` so
/// external code cannot `matrix.cells.clear()` to zero out
/// failures. The CI runner records cells via
/// [`Self::record_cell`]; release determination goes through
/// [`Self::meets_release_bar`] which now requires BOTH
/// "no failures" AND "all required cells present" — so an
/// incomplete matrix can't slip past the gate either.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CiMatrix {
    pub(crate) cells: Vec<CiMatrixCell>,
}

impl CiMatrix {
    /// Construct an empty matrix. The CI runner populates via
    /// [`Self::record_cell`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one cell's CI outcome. The CI runner is the only
    /// legitimate caller; pub(crate) field privacy prevents
    /// out-of-band cell injection.
    pub fn record_cell(&mut self, cell: CiMatrixCell) {
        self.cells.push(cell);
    }

    /// Read-only accessor for the recorded cells.
    #[must_use]
    pub fn cells(&self) -> &[CiMatrixCell] {
        &self.cells
    }

    /// Whether the matrix passes the bead's release bar:
    /// every applicable (compositor × test) cell either
    /// passed or was correctly marked NotApplicable; no
    /// outright failures; AND the matrix covers the full
    /// applicable set (no missing rows).
    ///
    /// **Two-part gate**: previously this method only checked
    /// "no failures", letting an empty matrix (cells cleared)
    /// or an incomplete matrix (CI runner crashed mid-run)
    /// silently pass the gate. Now the gate requires
    /// `covers_full_matrix()` as a precondition.
    #[must_use]
    pub fn meets_release_bar(&self) -> bool {
        if !self.covers_full_matrix() {
            return false;
        }
        !self.cells.iter().any(|c| c.outcome.blocks_release())
    }

    /// Number of cells that failed.
    #[must_use]
    pub fn fail_count(&self) -> usize {
        self.cells
            .iter()
            .filter(|c| c.outcome == CiCellOutcome::Fail)
            .count()
    }

    /// Number of cells that passed.
    #[must_use]
    pub fn pass_count(&self) -> usize {
        self.cells
            .iter()
            .filter(|c| c.outcome == CiCellOutcome::Pass)
            .count()
    }

    /// Whether every applicable cell has a recorded outcome
    /// (no missing rows).
    #[must_use]
    pub fn covers_full_matrix(&self) -> bool {
        for compositor in Compositor::all() {
            for test in [
                CiTestKind::VrrNegotiation,
                CiTestKind::DirectScanout,
                CiTestKind::FrameDedupCorrectness,
                CiTestKind::A11yPromptness,
                CiTestKind::RecordingCompatibility,
            ] {
                let applicable = test.applies_to(compositor);
                let observed = self
                    .cells
                    .iter()
                    .any(|c| c.compositor == compositor && c.test == test);
                if applicable && !observed {
                    return false;
                }
            }
        }
        true
    }
}

// ============================================================================
// 24h-bench acceptance — RQ-S5 / RQ-S7 / RQ-S12
// ============================================================================

/// CI release-gate thresholds.
///
/// Fields are `pub(crate)` because these are CI-release
/// thresholds (RQ-S5 / RQ-S7 / RQ-S12). External code that
/// holds an `&mut BenchAcceptanceConfig` could lower
/// `min_dedup_rate_pct` from 99% to 0% to bypass the gate.
/// Use the builder API for explicit reconfiguration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BenchAcceptanceConfig {
    /// RQ-S5: idle-frame dedup rate threshold (percent). Bead
    /// default 99%.
    pub(crate) min_dedup_rate_pct: u32,
    /// RQ-S7: max battery drain over 24h on M2 (percent).
    /// Bead default 5%.
    pub(crate) max_battery_drain_pct: u32,
    /// RQ-S12: minimum displays where ft doctor reports
    /// negotiated refresh rate cleanly.
    pub(crate) min_displays_reporting_refresh: u32,
    /// Substrate's own consistency check: dedup-rate must
    /// be in `[0, 100]`. Operator can't mis-configure to
    /// a vacuous threshold.
    pub(crate) max_dedup_rate_pct: u32,
    /// RQ-S7: minimum bench duration. Bead's "24h idle
    /// simulation" — substrate refuses to accept a bench
    /// that ran less than this. Default 86_400_000 ms (24h).
    /// Self-review fix (br-ft-wqg3j): previously omitted, so
    /// a 5-minute bench with great metrics could pass the
    /// release gate when the bead requires 24h.
    pub(crate) min_elapsed_ms: u64,
}

pub const RQS5_MIN_DEDUP_RATE_PCT: u32 = 99;
pub const RQS7_MAX_BATTERY_DRAIN_PCT: u32 = 5;
pub const RQS12_MIN_DISPLAYS: u32 = 1;
pub const RQS7_MIN_ELAPSED_MS: u64 = 86_400_000;

impl Default for BenchAcceptanceConfig {
    fn default() -> Self {
        Self {
            min_dedup_rate_pct: RQS5_MIN_DEDUP_RATE_PCT,
            max_battery_drain_pct: RQS7_MAX_BATTERY_DRAIN_PCT,
            min_displays_reporting_refresh: RQS12_MIN_DISPLAYS,
            max_dedup_rate_pct: 100,
            min_elapsed_ms: RQS7_MIN_ELAPSED_MS,
        }
    }
}

impl BenchAcceptanceConfig {
    // ----- Read-only accessors -----

    #[must_use]
    pub const fn min_dedup_rate_pct(self) -> u32 {
        self.min_dedup_rate_pct
    }
    #[must_use]
    pub const fn max_battery_drain_pct(self) -> u32 {
        self.max_battery_drain_pct
    }
    #[must_use]
    pub const fn min_displays_reporting_refresh(self) -> u32 {
        self.min_displays_reporting_refresh
    }
    #[must_use]
    pub const fn max_dedup_rate_pct(self) -> u32 {
        self.max_dedup_rate_pct
    }
    #[must_use]
    pub const fn min_elapsed_ms(self) -> u64 {
        self.min_elapsed_ms
    }

    // ----- Builder API (release-threshold changes are explicit) -----

    #[must_use]
    pub const fn with_min_dedup_rate_pct(mut self, pct: u32) -> Self {
        self.min_dedup_rate_pct = pct;
        self
    }
    #[must_use]
    pub const fn with_max_battery_drain_pct(mut self, pct: u32) -> Self {
        self.max_battery_drain_pct = pct;
        self
    }
    #[must_use]
    pub const fn with_min_displays_reporting_refresh(mut self, n: u32) -> Self {
        self.min_displays_reporting_refresh = n;
        self
    }
    #[must_use]
    pub const fn with_min_elapsed_ms(mut self, ms: u64) -> Self {
        self.min_elapsed_ms = ms;
        self
    }
}

/// Summary of a single 24h idle-bench run.
///
/// **CI integrity**: fields are `pub(crate)` because external
/// code that holds an `&mut IdleBenchSummary` could set
/// `deduped_frames = total_frames` to forge 100% dedup rate or
/// `elapsed_ms = u64::MAX` to fake a 24h bench. The CI runner
/// must construct via [`Self::new`] + the field-by-field
/// builder methods so the populating site is auditable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdleBenchSummary {
    /// Frames the integration's render loop generated.
    pub(crate) total_frames: u64,
    /// Frames that frame_dedup elided.
    pub(crate) deduped_frames: u64,
    /// Battery drain percent observed over the 24h window
    /// (Linux: read /sys/class/power_supply/.../capacity;
    /// macOS: pmset -g batt at start + end).
    pub(crate) battery_drain_pct: u32,
    /// Displays for which ft doctor returned a non-default
    /// VrrSupport snapshot.
    pub(crate) displays_reporting_refresh: u32,
    /// Bench wall-clock duration in milliseconds. Bead's
    /// 24h target = 86_400_000 ms.
    pub(crate) elapsed_ms: u64,
}

impl IdleBenchSummary {
    /// Construct an empty summary. The CI runner populates
    /// via the builder methods (with_total_frames, etc.) so
    /// the populating site is auditable.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            total_frames: 0,
            deduped_frames: 0,
            battery_drain_pct: 0,
            displays_reporting_refresh: 0,
            elapsed_ms: 0,
        }
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn total_frames(&self) -> u64 {
        self.total_frames
    }
    #[must_use]
    pub const fn deduped_frames(&self) -> u64 {
        self.deduped_frames
    }
    #[must_use]
    pub const fn battery_drain_pct(&self) -> u32 {
        self.battery_drain_pct
    }
    #[must_use]
    pub const fn displays_reporting_refresh(&self) -> u32 {
        self.displays_reporting_refresh
    }
    #[must_use]
    pub const fn elapsed_ms(&self) -> u64 {
        self.elapsed_ms
    }

    // ----- Builder API -----

    #[must_use]
    pub const fn with_total_frames(mut self, n: u64) -> Self {
        self.total_frames = n;
        self
    }
    #[must_use]
    pub const fn with_deduped_frames(mut self, n: u64) -> Self {
        self.deduped_frames = n;
        self
    }
    #[must_use]
    pub const fn with_battery_drain_pct(mut self, pct: u32) -> Self {
        self.battery_drain_pct = pct;
        self
    }
    #[must_use]
    pub const fn with_displays_reporting_refresh(mut self, n: u32) -> Self {
        self.displays_reporting_refresh = n;
        self
    }
    #[must_use]
    pub const fn with_elapsed_ms(mut self, ms: u64) -> Self {
        self.elapsed_ms = ms;
        self
    }

    #[must_use]
    pub fn dedup_rate_pct(&self) -> u32 {
        if self.total_frames == 0 {
            return 0;
        }
        ((self.deduped_frames * 100) / self.total_frames).min(100) as u32
    }

    /// Bead's RQ-S5 + RQ-S7 + RQ-S12 acceptance.
    ///
    /// Self-review fix (br-ft-wqg3j): now also enforces
    /// `elapsed_ms >= config.min_elapsed_ms` so a too-short
    /// bench with good metrics doesn't slip past the gate.
    #[must_use]
    pub fn meets_acceptance(&self, config: BenchAcceptanceConfig) -> bool {
        let dedup_rate = self.dedup_rate_pct();
        self.elapsed_ms >= config.min_elapsed_ms
            && dedup_rate >= config.min_dedup_rate_pct
            && dedup_rate <= config.max_dedup_rate_pct
            && self.battery_drain_pct <= config.max_battery_drain_pct
            && self.displays_reporting_refresh >= config.min_displays_reporting_refresh
    }
}

// ============================================================================
// Recording-active probe
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordingActiveProbe {
    /// macOS: ScreenCaptureKit `SCStream` enumeration.
    MacOsScreenCaptureKit,
    /// Linux: PipeWire `pw_registry` scan for screencast
    /// portal sessions.
    LinuxPipeWirePortal,
    /// Operator disabled detection (e.g., privacy-paranoid
    /// build).
    Disabled,
    /// Probe failed; substrate defaults to `RecordingActive`
    /// for safety (force-present wins → no frames elided
    /// during unknown recording state).
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordingState {
    Active,
    Inactive,
    /// Probe couldn't determine state; substrate's safety
    /// default treats this as Active.
    UnknownAssumeActive,
}

impl RecordingState {
    /// Whether this state should force-present and block
    /// scanout (per the bead's "no frames elided" rule).
    #[must_use]
    pub const fn forces_present(self) -> bool {
        matches!(self, Self::Active | Self::UnknownAssumeActive)
    }
}

/// Pure-logic decision over (probe + raw signal).
#[must_use]
pub fn classify_recording_state(probe: RecordingActiveProbe, raw_active: bool) -> RecordingState {
    match probe {
        RecordingActiveProbe::Disabled => RecordingState::Inactive,
        RecordingActiveProbe::Unknown => RecordingState::UnknownAssumeActive,
        RecordingActiveProbe::MacOsScreenCaptureKit | RecordingActiveProbe::LinuxPipeWirePortal => {
            if raw_active {
                RecordingState::Active
            } else {
                RecordingState::Inactive
            }
        }
    }
}

// ============================================================================
// DRM modifiers for direct-scanout
// ============================================================================

/// Opaque DRM modifier (the kernel's `__u64` modifier value
/// from `drm_fourcc.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct DrmModifier(pub u64);

/// `DRM_FORMAT_MOD_LINEAR` — universal fallback, slow.
pub const DRM_FORMAT_MOD_LINEAR: DrmModifier = DrmModifier(0);

/// `DRM_FORMAT_MOD_INVALID` — sentinel for "no modifier
/// negotiated". Never scanout-eligible.
pub const DRM_FORMAT_MOD_INVALID: DrmModifier = DrmModifier(u64::MAX);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanoutModifierAllowlist {
    /// Modifiers known to support direct-scanout on at
    /// least one tested GPU. The integration populates from
    /// the platform probe; substrate's role is to enforce
    /// an allowlist rather than allow-all.
    pub allowed: Vec<DrmModifier>,
}

impl ScanoutModifierAllowlist {
    #[must_use]
    pub fn is_scanout_eligible(&self, modifier: DrmModifier) -> bool {
        if modifier == DRM_FORMAT_MOD_INVALID {
            return false;
        }
        self.allowed.iter().any(|m| *m == modifier)
    }
}

// ============================================================================
// Attestation
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPipelineAttestation {
    pub version: String,
    pub ci_matrix: CiMatrix,
    pub idle_bench: Option<IdleBenchSummary>,
    pub idle_bench_passed: bool,
}

impl DisplayPipelineAttestation {
    /// Release-bar predicate combining the CI matrix and the
    /// 24h-bench result.
    #[must_use]
    pub fn meets_release_bar(&self, config: BenchAcceptanceConfig) -> bool {
        if !self.ci_matrix.meets_release_bar() {
            return false;
        }
        if !self.ci_matrix.covers_full_matrix() {
            return false;
        }
        if !self.idle_bench_passed {
            return false;
        }
        match &self.idle_bench {
            Some(b) => b.meets_acceptance(config),
            None => false,
        }
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DisplayPipelineCiTelemetry {
    pub ci_runs_total: u64,
    pub ci_runs_passed: u64,
    pub ci_runs_failed: u64,
    pub idle_benches_total: u64,
    pub idle_benches_passed: u64,
    pub recording_force_present_count: u64,
    pub scanout_modifier_rejections: u64,
}

impl DisplayPipelineCiTelemetry {
    pub fn record_ci_run(&mut self, passed: bool) {
        self.ci_runs_total = self.ci_runs_total.saturating_add(1);
        if passed {
            self.ci_runs_passed = self.ci_runs_passed.saturating_add(1);
        } else {
            self.ci_runs_failed = self.ci_runs_failed.saturating_add(1);
        }
    }

    pub fn record_idle_bench(&mut self, passed: bool) {
        self.idle_benches_total = self.idle_benches_total.saturating_add(1);
        if passed {
            self.idle_benches_passed = self.idle_benches_passed.saturating_add(1);
        }
    }

    pub fn record_recording_force_present(&mut self) {
        self.recording_force_present_count = self.recording_force_present_count.saturating_add(1);
    }

    pub fn record_scanout_modifier_rejection(&mut self) {
        self.scanout_modifier_rejections = self.scanout_modifier_rejections.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(c: Compositor, t: CiTestKind, o: CiCellOutcome) -> CiMatrixCell {
        CiMatrixCell {
            compositor: c,
            test: t,
            outcome: o,
        }
    }

    fn full_passing_matrix() -> CiMatrix {
        let mut cells = Vec::new();
        for compositor in Compositor::all() {
            for test in [
                CiTestKind::VrrNegotiation,
                CiTestKind::DirectScanout,
                CiTestKind::FrameDedupCorrectness,
                CiTestKind::A11yPromptness,
                CiTestKind::RecordingCompatibility,
            ] {
                let outcome = if test.applies_to(compositor) {
                    CiCellOutcome::Pass
                } else {
                    CiCellOutcome::NotApplicable
                };
                cells.push(cell(compositor, test, outcome));
            }
        }
        CiMatrix { cells }
    }

    // ----------------------------------------------------------------
    // Compositor
    // ----------------------------------------------------------------

    #[test]
    fn compositor_label_stable() {
        assert_eq!(Compositor::MutterWayland.label(), "mutter_wayland");
        assert_eq!(Compositor::Sway.label(), "sway");
        assert_eq!(Compositor::I3X11.label(), "i3_x11");
    }

    #[test]
    fn compositor_wayland_x11_partition() {
        for c in Compositor::all() {
            assert!(c.is_wayland() ^ c.is_x11());
        }
        assert!(Compositor::MutterWayland.is_wayland());
        assert!(Compositor::I3X11.is_x11());
    }

    #[test]
    fn compositor_all_returns_six() {
        assert_eq!(Compositor::all().len(), 6);
    }

    // ----------------------------------------------------------------
    // CiTestKind applicability
    // ----------------------------------------------------------------

    #[test]
    fn ci_test_direct_scanout_only_wayland() {
        assert!(CiTestKind::DirectScanout.applies_to(Compositor::Sway));
        assert!(!CiTestKind::DirectScanout.applies_to(Compositor::I3X11));
    }

    #[test]
    fn ci_test_others_apply_everywhere() {
        for c in Compositor::all() {
            assert!(CiTestKind::VrrNegotiation.applies_to(c));
            assert!(CiTestKind::FrameDedupCorrectness.applies_to(c));
            assert!(CiTestKind::A11yPromptness.applies_to(c));
            assert!(CiTestKind::RecordingCompatibility.applies_to(c));
        }
    }

    // ----------------------------------------------------------------
    // CiCellOutcome
    // ----------------------------------------------------------------

    #[test]
    fn outcome_blocks_release_only_on_fail() {
        assert!(CiCellOutcome::Fail.blocks_release());
        assert!(!CiCellOutcome::Pass.blocks_release());
        assert!(!CiCellOutcome::NotApplicable.blocks_release());
    }

    // ----------------------------------------------------------------
    // CiMatrix
    // ----------------------------------------------------------------

    #[test]
    fn matrix_full_pass_meets_release_bar() {
        let m = full_passing_matrix();
        assert!(m.meets_release_bar());
        assert!(m.covers_full_matrix());
    }

    #[test]
    fn matrix_one_fail_blocks_release() {
        let mut m = full_passing_matrix();
        m.cells[0].outcome = CiCellOutcome::Fail;
        assert!(!m.meets_release_bar());
        assert_eq!(m.fail_count(), 1);
    }

    #[test]
    fn matrix_missing_row_does_not_cover() {
        let mut m = full_passing_matrix();
        m.cells.pop(); // remove a cell
        assert!(!m.covers_full_matrix());
    }

    #[test]
    fn matrix_pass_count_correct() {
        let m = full_passing_matrix();
        // 6 compositors × 5 tests = 30; minus 2 X11 compositors
        // × 1 DirectScanout NotApplicable = 28 passes.
        assert_eq!(m.pass_count(), 28);
    }

    // ----------------------------------------------------------------
    // BenchAcceptanceConfig + IdleBenchSummary
    // ----------------------------------------------------------------

    #[test]
    fn bench_config_defaults_match_bead() {
        let c = BenchAcceptanceConfig::default();
        assert_eq!(c.min_dedup_rate_pct, 99);
        assert_eq!(c.max_battery_drain_pct, 5);
        assert_eq!(c.min_displays_reporting_refresh, 1);
        // Self-review fix (br-ft-wqg3j): default min_elapsed_ms
        // matches bead's 24h requirement.
        assert_eq!(c.min_elapsed_ms, 86_400_000);
    }

    #[test]
    fn idle_bench_too_short_fails_rqs7_duration() {
        // Self-review fix (br-ft-wqg3j): a 5-minute bench
        // with great metrics must NOT pass the 24h gate.
        let b = IdleBenchSummary {
            total_frames: 18_000,   // 60 fps × 5 min
            deduped_frames: 17_999, // 99.99% dedup
            battery_drain_pct: 0,
            displays_reporting_refresh: 1,
            elapsed_ms: 5 * 60 * 1000, // 5 min, well under 24h
        };
        let config = BenchAcceptanceConfig::default();
        assert!(!b.meets_acceptance(config));
    }

    #[test]
    fn idle_bench_at_exact_min_elapsed_passes() {
        // Boundary: elapsed = min_elapsed exactly. Substrate
        // accepts (>=), bead-aligned.
        let b = IdleBenchSummary {
            total_frames: 5_184_000,
            deduped_frames: 5_132_160, // 99% dedup
            battery_drain_pct: 4,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let config = BenchAcceptanceConfig::default();
        assert!(b.meets_acceptance(config));
    }

    #[test]
    fn idle_bench_dedup_rate_zero_for_no_frames() {
        let b = IdleBenchSummary {
            total_frames: 0,
            deduped_frames: 0,
            battery_drain_pct: 0,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        assert_eq!(b.dedup_rate_pct(), 0);
    }

    #[test]
    fn idle_bench_dedup_rate_99_pct() {
        let b = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        assert_eq!(b.dedup_rate_pct(), 99);
        let config = BenchAcceptanceConfig::default();
        assert!(b.meets_acceptance(config));
    }

    #[test]
    fn idle_bench_98_pct_dedup_fails_rqs5() {
        let b = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 98,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let config = BenchAcceptanceConfig::default();
        assert!(!b.meets_acceptance(config));
    }

    #[test]
    fn idle_bench_6_pct_battery_fails_rqs7() {
        let b = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 6,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let config = BenchAcceptanceConfig::default();
        assert!(!b.meets_acceptance(config));
    }

    #[test]
    fn idle_bench_no_displays_reporting_fails_rqs12() {
        let b = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 3,
            displays_reporting_refresh: 0,
            elapsed_ms: 86_400_000,
        };
        let config = BenchAcceptanceConfig::default();
        assert!(!b.meets_acceptance(config));
    }

    #[test]
    fn idle_bench_dedup_rate_caps_at_100() {
        // Defensive: deduped_frames > total_frames shouldn't
        // produce a 110% rate.
        let b = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 110,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        assert_eq!(b.dedup_rate_pct(), 100);
    }

    // ----------------------------------------------------------------
    // RecordingState classification
    // ----------------------------------------------------------------

    #[test]
    fn recording_disabled_always_inactive() {
        let s = classify_recording_state(RecordingActiveProbe::Disabled, true);
        assert_eq!(s, RecordingState::Inactive);
    }

    #[test]
    fn recording_unknown_assumes_active() {
        let s = classify_recording_state(RecordingActiveProbe::Unknown, false);
        assert_eq!(s, RecordingState::UnknownAssumeActive);
        assert!(s.forces_present());
    }

    #[test]
    fn recording_macos_screencapture_passes_through() {
        let active = classify_recording_state(RecordingActiveProbe::MacOsScreenCaptureKit, true);
        let inactive = classify_recording_state(RecordingActiveProbe::MacOsScreenCaptureKit, false);
        assert_eq!(active, RecordingState::Active);
        assert_eq!(inactive, RecordingState::Inactive);
    }

    #[test]
    fn recording_linux_pipewire_passes_through() {
        let active = classify_recording_state(RecordingActiveProbe::LinuxPipeWirePortal, true);
        assert_eq!(active, RecordingState::Active);
        assert!(active.forces_present());
    }

    // ----------------------------------------------------------------
    // ScanoutModifierAllowlist
    // ----------------------------------------------------------------

    #[test]
    fn scanout_invalid_modifier_never_eligible() {
        let allow = ScanoutModifierAllowlist {
            allowed: vec![DrmModifier(0x100)],
        };
        assert!(!allow.is_scanout_eligible(DRM_FORMAT_MOD_INVALID));
    }

    #[test]
    fn scanout_modifier_in_allowlist_eligible() {
        let allow = ScanoutModifierAllowlist {
            allowed: vec![DrmModifier(0x100), DrmModifier(0x200)],
        };
        assert!(allow.is_scanout_eligible(DrmModifier(0x100)));
        assert!(allow.is_scanout_eligible(DrmModifier(0x200)));
    }

    #[test]
    fn scanout_modifier_not_in_allowlist_rejected() {
        let allow = ScanoutModifierAllowlist {
            allowed: vec![DrmModifier(0x100)],
        };
        assert!(!allow.is_scanout_eligible(DrmModifier(0x999)));
    }

    #[test]
    fn scanout_empty_allowlist_rejects_everything() {
        let allow = ScanoutModifierAllowlist { allowed: vec![] };
        assert!(!allow.is_scanout_eligible(DrmModifier(0x100)));
        assert!(!allow.is_scanout_eligible(DRM_FORMAT_MOD_LINEAR));
    }

    // ----------------------------------------------------------------
    // DisplayPipelineAttestation
    // ----------------------------------------------------------------

    #[test]
    fn attestation_passes_with_full_matrix_and_passing_bench() {
        let bench = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let a = DisplayPipelineAttestation {
            version: "0.5.0".to_string(),
            ci_matrix: full_passing_matrix(),
            idle_bench: Some(bench),
            idle_bench_passed: true,
        };
        assert!(a.meets_release_bar(BenchAcceptanceConfig::default()));
    }

    #[test]
    fn attestation_fails_when_bench_missing() {
        let a = DisplayPipelineAttestation {
            version: "0.5.0".to_string(),
            ci_matrix: full_passing_matrix(),
            idle_bench: None,
            idle_bench_passed: false,
        };
        assert!(!a.meets_release_bar(BenchAcceptanceConfig::default()));
    }

    #[test]
    fn attestation_fails_when_matrix_partial() {
        let mut matrix = full_passing_matrix();
        matrix.cells.pop();
        let bench = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let a = DisplayPipelineAttestation {
            version: "0.5.0".to_string(),
            ci_matrix: matrix,
            idle_bench: Some(bench),
            idle_bench_passed: true,
        };
        assert!(!a.meets_release_bar(BenchAcceptanceConfig::default()));
    }

    #[test]
    fn attestation_fails_when_any_cell_failed() {
        let mut matrix = full_passing_matrix();
        matrix.cells[0].outcome = CiCellOutcome::Fail;
        let bench = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let a = DisplayPipelineAttestation {
            version: "0.5.0".to_string(),
            ci_matrix: matrix,
            idle_bench: Some(bench),
            idle_bench_passed: true,
        };
        assert!(!a.meets_release_bar(BenchAcceptanceConfig::default()));
    }

    // ----------------------------------------------------------------
    // Telemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_record_ci_run_routes() {
        let mut t = DisplayPipelineCiTelemetry::default();
        t.record_ci_run(true);
        t.record_ci_run(false);
        t.record_ci_run(true);
        assert_eq!(t.ci_runs_total, 3);
        assert_eq!(t.ci_runs_passed, 2);
        assert_eq!(t.ci_runs_failed, 1);
    }

    #[test]
    fn telemetry_record_idle_bench_routes() {
        let mut t = DisplayPipelineCiTelemetry::default();
        t.record_idle_bench(true);
        t.record_idle_bench(false);
        assert_eq!(t.idle_benches_total, 2);
        assert_eq!(t.idle_benches_passed, 1);
    }

    #[test]
    fn telemetry_record_recording_force_present() {
        let mut t = DisplayPipelineCiTelemetry::default();
        t.record_recording_force_present();
        t.record_recording_force_present();
        assert_eq!(t.recording_force_present_count, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_release_passes_full_acceptance() {
        let bench = IdleBenchSummary {
            total_frames: 5_184_000,   // 24h * 60fps
            deduped_frames: 5_132_160, // 99% dedup
            battery_drain_pct: 4,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let a = DisplayPipelineAttestation {
            version: "0.5.0".to_string(),
            ci_matrix: full_passing_matrix(),
            idle_bench: Some(bench),
            idle_bench_passed: true,
        };
        assert!(a.meets_release_bar(BenchAcceptanceConfig::default()));
    }

    #[test]
    fn scenario_recording_unknown_state_safety_default() {
        // Probe failed (e.g. macOS denied screen-recording
        // permission). Substrate defaults to UnknownAssumeActive
        // so the integration force-presents (no frames elided
        // during unknown recording state).
        let s = classify_recording_state(RecordingActiveProbe::Unknown, false);
        assert!(s.forces_present());
    }

    #[test]
    fn scenario_xfwm_x11_skips_direct_scanout() {
        // On X11, DirectScanout is NotApplicable; matrix
        // marks it as such and overall meets_release_bar.
        // After br-ft-bhb7i: meets_release_bar now requires
        // covers_full_matrix, so we use a full passing matrix
        // and verify that the X11 DirectScanout cells are
        // recorded as NotApplicable. Test name + intent
        // preserved.
        let m = full_passing_matrix();
        assert!(m.meets_release_bar());
        // Verify the X11 DirectScanout cells are
        // NotApplicable (not Pass — the bead's "DirectScanout
        // is Wayland-only" rule).
        for cell in m.cells() {
            if cell.compositor.is_x11() && cell.test == CiTestKind::DirectScanout {
                assert_eq!(cell.outcome, CiCellOutcome::NotApplicable);
            }
        }
    }

    #[test]
    fn meets_release_bar_blocks_incomplete_matrix() {
        // REGRESSION: previously, meets_release_bar() returned
        // true for an EMPTY matrix or any matrix missing
        // required cells. Now requires covers_full_matrix()
        // as a precondition. Pin: a 2-cell matrix (incomplete)
        // does NOT pass the gate even with no failures.
        let mut m = CiMatrix::default();
        m.record_cell(cell(
            Compositor::MutterWayland,
            CiTestKind::VrrNegotiation,
            CiCellOutcome::Pass,
        ));
        m.record_cell(cell(
            Compositor::MutterWayland,
            CiTestKind::DirectScanout,
            CiCellOutcome::Pass,
        ));
        assert!(!m.meets_release_bar());
        assert!(!m.covers_full_matrix());
    }

    #[test]
    fn meets_release_bar_blocks_empty_matrix() {
        // REGRESSION: previously, an empty matrix passed
        // meets_release_bar() because the !cells.iter().any()
        // check evaluated to true on zero cells. Operators
        // wanting to forge release readiness could
        // matrix.cells.clear() and pass the gate.
        let m = CiMatrix::new();
        assert!(!m.meets_release_bar());
    }

    #[test]
    fn scenario_battery_drain_at_exact_5pct_passes() {
        let b = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 5,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let config = BenchAcceptanceConfig::default();
        assert!(b.meets_acceptance(config));
    }

    #[test]
    fn scenario_typical_ci_failure_blocks_release() {
        // Hyprland's frame-dedup correctness test fires —
        // matrix marks Fail; release blocked.
        let mut m = full_passing_matrix();
        let target = m
            .cells
            .iter_mut()
            .find(|c| {
                c.compositor == Compositor::Hyprland && c.test == CiTestKind::FrameDedupCorrectness
            })
            .expect("cell exists in full matrix");
        target.outcome = CiCellOutcome::Fail;
        let bench = IdleBenchSummary {
            total_frames: 100,
            deduped_frames: 99,
            battery_drain_pct: 3,
            displays_reporting_refresh: 1,
            elapsed_ms: 86_400_000,
        };
        let a = DisplayPipelineAttestation {
            version: "0.5.0".to_string(),
            ci_matrix: m,
            idle_bench: Some(bench),
            idle_bench_passed: true,
        };
        assert!(!a.meets_release_bar(BenchAcceptanceConfig::default()));
    }
}
