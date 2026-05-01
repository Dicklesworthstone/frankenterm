//! Per-compositor verification matrix for the Wayland frame-
//! callback chain-depth guard
//! ([BR-TERM-EMULATOR-UPLIFT.3.2.cont] / `ft-28opz`).
//!
//! This module ships the **contract layer** the bead's Linux
//! integration test (in `tests/wayland_resize_storm.rs`) and
//! `ft doctor` surface consume. The instrumentation that
//! drives the verification — `frame_callback_chain_depth` and
//! `frame_callback_chain_depth_peak` — already exists in
//! production at
//! `frankenterm/window/src/os/wayland/window.rs:663-1265`
//! (committed as `151bde5fe`); the bead's continuation work
//! is the **observability surface + Linux validation harness**
//! around it.
//!
//! ## Why a separate module
//!
//! The production accessors (`frame_callback_chain_depth()` /
//! `frame_callback_chain_depth_peak()`) live in
//! `frankenterm/window` (the wezterm-derived window crate),
//! `pub(crate)` only. This module ships **dependency-free
//! types** the integration harness, the `ft doctor` surface,
//! and the per-compositor verification doc all consume —
//! without depending on the Wayland backend (so this module
//! compiles under non-Linux targets, where the production
//! accessors don't exist).
//!
//! ## Headline rule
//!
//! From the parent bead `ft-mpc9b.3.2`:
//!
//! > Frame-callback chain depth must stay ≤ 1 under any
//! > resize-storm interleaving. The structural guards at
//! > window.rs lines 1070, 1153, and 1192 bound it to ≤ 1
//! > by inspection; this validation matrix is the runtime
//! > regression net per Tier-1 compositor.
//!
//! Post-fix target (if a real failing reproducer ever surfaces
//! and the bead's option-#1 reorder is shipped): chain depth
//! stays ≤ **2** transiently during the swap and returns to
//! ≤ 1 at steady-state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Compositor identity
// ============================================================================

/// Tier classification for compositor support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositorTier {
    /// First-class support — CI runs the resize-storm
    /// reproducer here on every Linux PR.
    Tier1,
    /// Best-effort — not in CI; manual verification per
    /// release.
    Tier2,
    /// Reference compositor (Wayland upstream / Weston). Used
    /// for spec-conformance triage.
    Reference,
}

/// Closed list of compositors covered by the verification
/// matrix. Adding a compositor requires extending this enum
/// AND the `CompositorIdentity::all` table; the conformance
/// test in `tests/wayland_resize_storm.rs` asserts coverage
/// parity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositorIdentity {
    /// GNOME — uses `mutter`. Tier-1.
    Mutter,
    /// KDE Plasma — uses `kwin`. Tier-1.
    Kwin,
    /// `sway` — Wayland-only i3 clone. Tier-1.
    Sway,
    /// `hyprland` — dynamic tiling. Tier-2 (best-effort).
    Hyprland,
    /// `wayfire` — 3D-effect compositor. Tier-2.
    Wayfire,
    /// `weston` — Wayland reference compositor.
    Weston,
}

impl CompositorIdentity {
    /// Stable slug used in the JSON verification report and
    /// the `WAYLAND_COMPOSITOR` env var override.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Mutter => "mutter",
            Self::Kwin => "kwin",
            Self::Sway => "sway",
            Self::Hyprland => "hyprland",
            Self::Wayfire => "wayfire",
            Self::Weston => "weston",
        }
    }

    /// Tier classification.
    #[must_use]
    pub const fn tier(self) -> CompositorTier {
        match self {
            Self::Mutter | Self::Kwin | Self::Sway => CompositorTier::Tier1,
            Self::Hyprland | Self::Wayfire => CompositorTier::Tier2,
            Self::Weston => CompositorTier::Reference,
        }
    }

    /// Every compositor in declaration order.
    pub const ALL: &'static [CompositorIdentity] = &[
        Self::Mutter,
        Self::Kwin,
        Self::Sway,
        Self::Hyprland,
        Self::Wayfire,
        Self::Weston,
    ];

    /// Resolve a compositor identity from its slug. Returns
    /// `None` for unrecognized strings.
    #[must_use]
    pub fn from_slug(slug: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.slug() == slug)
    }
}

// ============================================================================
// Resize-storm reproducer parameters
// ============================================================================

/// Parameters for the resize-storm reproducer the bead
/// enumerates.
///
/// > Drag window edge rapidly back and forth for 5 seconds.
/// > Assert chain depth peak never exceeded 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResizeStormConfig {
    /// Duration of the storm in milliseconds (default 5000).
    pub duration_ms: u32,
    /// Resize events per second to inject (default 60).
    pub events_per_second: u32,
    /// Minimum width during resize (px).
    pub min_width: u16,
    /// Maximum width during resize (px).
    pub max_width: u16,
    /// Minimum height (px).
    pub min_height: u16,
    /// Maximum height (px).
    pub max_height: u16,
}

impl Default for ResizeStormConfig {
    fn default() -> Self {
        Self {
            duration_ms: 5_000,
            events_per_second: 60,
            min_width: 400,
            max_width: 1200,
            min_height: 300,
            max_height: 900,
        }
    }
}

impl ResizeStormConfig {
    /// Total resize events the storm injects.
    #[must_use]
    pub const fn total_events(&self) -> u32 {
        self.duration_ms * self.events_per_second / 1000
    }
}

/// Acceptance bounds for the chain-depth peak after the
/// reproducer runs. Pre-fix target is 1 (the structural
/// guards bound it there by inspection); post-fix target
/// (if option-#1 is ever shipped) is 2 transiently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainDepthBound {
    /// Maximum acceptable peak. Default 1 — the bead's stated
    /// pre-fix target.
    pub peak_max: u32,
}

impl Default for ChainDepthBound {
    fn default() -> Self {
        Self { peak_max: 1 }
    }
}

impl ChainDepthBound {
    /// Pre-fix bound — chain depth ≤ 1 (structural).
    pub const PRE_FIX: Self = Self { peak_max: 1 };
    /// Post-fix bound — chain depth ≤ 2 (transient during swap).
    pub const POST_FIX: Self = Self { peak_max: 2 };

    /// Check an observed peak against this bound.
    #[must_use]
    pub const fn check(self, observed_peak: u32) -> bool {
        observed_peak <= self.peak_max
    }
}

// ============================================================================
// Verification record
// ============================================================================

/// Result of running the resize-storm reproducer once on a
/// specific compositor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationResult {
    pub compositor: CompositorIdentity,
    pub tier: CompositorTier,
    /// Compositor version string (e.g., `"mutter 47.2"`).
    /// Captured at runtime; the doc records what was tested.
    pub version: String,
    /// Storm config used.
    pub config: ResizeStormConfig,
    /// Bound the run was checked against.
    pub bound: ChainDepthBound,
    /// Observed peak.
    pub chain_depth_peak: u32,
    /// Whether the run passed (`peak <= bound.peak_max`).
    pub passed: bool,
    /// Optional notes (e.g., reproducer script ran via
    /// `ydotool`, version mismatch, etc.).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub notes: Option<String>,
}

impl VerificationResult {
    /// Synthesize a result from observed peak. Tier and bound
    /// are looked up from the compositor identity / `bound`
    /// argument.
    #[must_use]
    pub fn observed(
        compositor: CompositorIdentity,
        version: impl Into<String>,
        config: ResizeStormConfig,
        bound: ChainDepthBound,
        chain_depth_peak: u32,
    ) -> Self {
        let passed = bound.check(chain_depth_peak);
        Self {
            compositor,
            tier: compositor.tier(),
            version: version.into(),
            config,
            bound,
            chain_depth_peak,
            passed,
            notes: None,
        }
    }

    /// Builder — attach an explanatory note.
    #[must_use]
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.notes = Some(note.into());
        self
    }
}

/// Aggregate verification matrix snapshot — what the per-
/// release JSON artifact records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositorMatrixSnapshot {
    /// Schema version.
    pub schema_version: u32,
    /// Bead identifier.
    pub bead: String,
    /// Per-compositor result rows.
    pub results: BTreeMap<String, VerificationResult>,
}

impl CompositorMatrixSnapshot {
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: 1,
            bead: "ft-28opz".to_string(),
            results: BTreeMap::new(),
        }
    }

    /// Add or replace a result.
    pub fn record(&mut self, result: VerificationResult) {
        self.results
            .insert(result.compositor.slug().to_string(), result);
    }

    /// Whether every Tier-1 compositor has a passing result.
    #[must_use]
    pub fn all_tier1_passed(&self) -> bool {
        for c in CompositorIdentity::ALL {
            if c.tier() != CompositorTier::Tier1 {
                continue;
            }
            match self.results.get(c.slug()) {
                Some(r) if r.passed => {}
                _ => return false,
            }
        }
        true
    }

    /// Tier-1 compositors with no recorded result. The bead's
    /// acceptance criterion: every Tier-1 compositor must have
    /// at least one verification run per release.
    #[must_use]
    pub fn missing_tier1(&self) -> Vec<CompositorIdentity> {
        CompositorIdentity::ALL
            .iter()
            .copied()
            .filter(|c| c.tier() == CompositorTier::Tier1)
            .filter(|c| !self.results.contains_key(c.slug()))
            .collect()
    }
}

impl Default for CompositorMatrixSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// ft doctor exposure
// ============================================================================

/// `ft doctor` health snapshot for the Wayland frame-callback
/// chain-depth surface. Mirrors this session's `*Health` shape
/// (a11y_tree, color_management, atlas_stability,
/// triple_buffer, live_resize, render_quality,
/// snap_back_fuzz, wayland_frame_pacing, bidi_correctness,
/// tx_killswitch_model, passive_watch_invariant,
/// wire_dedup_model, redactor_coverage_matrix,
/// tui_parity_oracle, robot_checkpoint_state_machine,
/// robot_work_state_machine, robot_fleet_state_machine).
///
/// The integration layer reads this from the production
/// `frame_callback_chain_depth_peak()` accessor at runtime;
/// this struct is the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameCallbackHealth {
    /// Current in-flight chain depth at sample time.
    pub chain_depth_now: u32,
    /// Lifetime peak since window creation.
    pub chain_depth_peak: u32,
    /// Total resize events observed (best-effort counter; the
    /// production code can wire this from the existing event
    /// stream if needed).
    pub resize_events_total: u64,
    /// Number of times a chain depth > 1 was observed (the
    /// production code emits a `log::warn` at depth > 1; this
    /// counter exposes it as a runtime metric).
    pub depth_gt_one_observations_total: u64,
}

impl FrameCallbackHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            chain_depth_now: 0,
            chain_depth_peak: 0,
            resize_events_total: 0,
            depth_gt_one_observations_total: 0,
        }
    }

    /// Whether the chain-depth peak is within bounds.
    #[must_use]
    pub const fn is_safe(&self, bound: ChainDepthBound) -> bool {
        bound.check(self.chain_depth_peak)
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_compositor_has_a_tier_and_slug() {
        for c in CompositorIdentity::ALL {
            let _ = c.tier();
            assert!(!c.slug().is_empty());
        }
    }

    #[test]
    fn tier1_compositors_are_three() {
        let count = CompositorIdentity::ALL
            .iter()
            .filter(|c| c.tier() == CompositorTier::Tier1)
            .count();
        assert_eq!(count, 3, "Tier-1 set is mutter / kwin / sway");
    }

    #[test]
    fn slug_roundtrips_through_from_slug() {
        for c in CompositorIdentity::ALL {
            assert_eq!(CompositorIdentity::from_slug(c.slug()), Some(*c));
        }
    }

    #[test]
    fn unknown_slug_is_none() {
        assert_eq!(CompositorIdentity::from_slug("xfwm"), None);
    }

    #[test]
    fn resize_storm_default_total_events() {
        let cfg = ResizeStormConfig::default();
        assert_eq!(cfg.total_events(), 300); // 5s × 60 events/s
    }

    #[test]
    fn chain_depth_bound_pre_fix_is_one() {
        let b = ChainDepthBound::PRE_FIX;
        assert!(b.check(0));
        assert!(b.check(1));
        assert!(!b.check(2));
    }

    #[test]
    fn chain_depth_bound_post_fix_is_two() {
        let b = ChainDepthBound::POST_FIX;
        assert!(b.check(2));
        assert!(!b.check(3));
    }

    #[test]
    fn verification_result_observed_passes_at_or_below_bound() {
        let r = VerificationResult::observed(
            CompositorIdentity::Mutter,
            "mutter 47.2",
            ResizeStormConfig::default(),
            ChainDepthBound::PRE_FIX,
            1,
        );
        assert!(r.passed);
    }

    #[test]
    fn verification_result_observed_fails_above_bound() {
        let r = VerificationResult::observed(
            CompositorIdentity::Mutter,
            "mutter 47.2",
            ResizeStormConfig::default(),
            ChainDepthBound::PRE_FIX,
            5,
        );
        assert!(!r.passed);
    }

    #[test]
    fn matrix_snapshot_records_results() {
        let mut m = CompositorMatrixSnapshot::new();
        for c in CompositorIdentity::ALL {
            if c.tier() != CompositorTier::Tier1 {
                continue;
            }
            m.record(VerificationResult::observed(
                *c,
                format!("{} test", c.slug()),
                ResizeStormConfig::default(),
                ChainDepthBound::PRE_FIX,
                1,
            ));
        }
        assert!(m.all_tier1_passed());
        assert!(m.missing_tier1().is_empty());
    }

    #[test]
    fn missing_tier1_reports_gaps() {
        let mut m = CompositorMatrixSnapshot::new();
        m.record(VerificationResult::observed(
            CompositorIdentity::Sway,
            "sway 1.10",
            ResizeStormConfig::default(),
            ChainDepthBound::PRE_FIX,
            1,
        ));
        // mutter + kwin missing.
        let missing = m.missing_tier1();
        assert_eq!(missing.len(), 2);
        assert!(missing.contains(&CompositorIdentity::Mutter));
        assert!(missing.contains(&CompositorIdentity::Kwin));
    }

    #[test]
    fn matrix_snapshot_serde_roundtrip() {
        let mut m = CompositorMatrixSnapshot::new();
        m.record(
            VerificationResult::observed(
                CompositorIdentity::Mutter,
                "mutter 47.2",
                ResizeStormConfig::default(),
                ChainDepthBound::PRE_FIX,
                1,
            )
            .with_note("CI lane: ubuntu-24.04 / mutter snap"),
        );
        let json = serde_json::to_string(&m).unwrap();
        let parsed: CompositorMatrixSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, m);
    }

    #[test]
    fn baseline_health_is_safe_at_pre_fix() {
        let h = FrameCallbackHealth::baseline();
        assert!(h.is_safe(ChainDepthBound::PRE_FIX));
    }

    #[test]
    fn health_unsafe_when_peak_exceeds_bound() {
        let h = FrameCallbackHealth {
            chain_depth_now: 0,
            chain_depth_peak: 3,
            resize_events_total: 100,
            depth_gt_one_observations_total: 5,
        };
        assert!(!h.is_safe(ChainDepthBound::PRE_FIX));
        assert!(!h.is_safe(ChainDepthBound::POST_FIX));
    }

    #[test]
    fn all_tier1_failed_when_one_missing() {
        let mut m = CompositorMatrixSnapshot::new();
        m.record(VerificationResult::observed(
            CompositorIdentity::Mutter,
            "x",
            ResizeStormConfig::default(),
            ChainDepthBound::PRE_FIX,
            1,
        ));
        // kwin + sway missing.
        assert!(!m.all_tier1_passed());
    }

    #[test]
    fn all_tier1_failed_when_one_failed() {
        let mut m = CompositorMatrixSnapshot::new();
        for c in [
            CompositorIdentity::Mutter,
            CompositorIdentity::Kwin,
            CompositorIdentity::Sway,
        ] {
            let peak = if c == CompositorIdentity::Sway { 5 } else { 1 };
            m.record(VerificationResult::observed(
                c,
                "x",
                ResizeStormConfig::default(),
                ChainDepthBound::PRE_FIX,
                peak,
            ));
        }
        assert!(!m.all_tier1_passed());
    }
}
