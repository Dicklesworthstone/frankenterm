//! Feature-flag rollout-phase substrate (ft-mpc9b.9).
//!
//! Codifies the bead's 4-phase rollout model (Hidden → OptIn →
//! Default → Cleanup) so every renderer-overhaul feature follows the
//! same lifecycle. The pure-logic substrate gives the integration
//! layer a typed surface for:
//!
//! - Reading the current phase per feature (drives whether the
//!   `FT_RENDERER_<feature>=1` env var or `--features <name>` Cargo
//!   flag is required).
//! - Validating phase transitions (Hidden → OptIn → Default → Cleanup
//!   only; no skipping; no going backwards except for emergency
//!   rollback).
//! - Looking up the canonical per-feature timeline against the
//!   abstract release markers (m0 / m1 / m2 / m3+).
//!
//! Per AGENTS.md "no backwards compatibility" doctrine, the Cleanup
//! phase is aggressive — legacy paths are deleted one release after
//! the feature reaches Default.
//!
//! ## What this module ships
//!
//! - `RolloutPhase` — `Hidden / OptIn / Default / Cleanup`. Pure
//!   data; the integration's startup logic matches on the current
//!   phase to decide whether the feature path is live.
//! - `Marker` — `M0 / M1 / M2 / M3 / M4 / M5 / M6` release-cycle
//!   markers from the bead. Sequence model, not calendar.
//! - `FeatureTimeline` — per-feature struct mapping `RolloutPhase`
//!   to `Marker`.
//! - `FeatureRolloutRegistry` — registry of canonical features
//!   from the bead's per-feature timeline (seed via
//!   `FeatureRolloutRegistry::canonical()`).
//! - `RolloutState` — current phase per feature, advanced via
//!   `transition_to`.
//! - `transition_validity` — pure predicate: legal next phases from
//!   the current one.
//! - `RolloutPhaseAtMarker` — pure-logic lookup: given a feature
//!   timeline + the current marker, return what phase the feature
//!   is in.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.9.cont)
//!
//! - `docs/rollout/feature-flag-rollout.md` — operator-facing
//!   markdown rendering this substrate's data into a human-readable
//!   roadmap.
//! - `FT_RENDERER_<feature>=1` env-var routing in `frankenterm-gui`
//!   startup (parse + dispatch into `RolloutState::transition_to`
//!   for ad-hoc flips during testing).
//! - Cargo feature-flag scaffolding (`#[cfg(feature = "...")]`
//!   guards on each renderer path).
//! - Per-release notes auto-generation from the registry's current
//!   state.
//! - Cross-link to the SLO catalog (ft-mpc9b.7) so each release's
//!   rollout-phase advance can attest the SLOs that gate it.

#![allow(dead_code)]

// ============================================================================
// RolloutPhase
// ============================================================================

/// 4-phase rollout lifecycle per the bead. Order is significant:
/// `Hidden` → `OptIn` → `Default` → `Cleanup` is the only legal
/// forward path; `transition_validity` enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum RolloutPhase {
    /// Built but disabled by default. Enable via Cargo `--features`
    /// flag + `FT_RENDERER_<name>=1` env var (both required —
    /// belt-and-braces against accidental enable).
    #[default]
    Hidden,
    /// Documented; default off; users can opt in via
    /// `frankenterm.toml` config or env var.
    OptIn,
    /// Shipped as default. Legacy path still compilable via Cargo
    /// flag for emergency rollback.
    Default,
    /// Legacy path deleted per AGENTS.md "no backwards
    /// compatibility" doctrine. Done aggressively — one release
    /// after Default.
    Cleanup,
}

impl RolloutPhase {
    /// Whether the feature is *enabled* in this phase by default.
    /// `Hidden` and `OptIn` are off-by-default; `Default` and
    /// `Cleanup` are on (Cleanup is "always on, legacy gone").
    #[must_use]
    pub fn is_enabled_by_default(self) -> bool {
        matches!(self, Self::Default | Self::Cleanup)
    }

    /// Whether the operator can opt in via env var / config.
    /// `Hidden` requires the Cargo flag; `OptIn` and `Default` are
    /// runtime-toggleable; `Cleanup` has no legacy to fall back to.
    #[must_use]
    pub fn is_runtime_toggleable(self) -> bool {
        matches!(self, Self::OptIn | Self::Default)
    }

    /// Whether legacy / fallback code is still compilable in this
    /// phase. `Hidden` is the only phase with neither legacy nor
    /// new (the new path's just being built); `OptIn` and `Default`
    /// have legacy alongside; `Cleanup` deletes legacy.
    #[must_use]
    pub fn has_legacy_fallback(self) -> bool {
        matches!(self, Self::OptIn | Self::Default)
    }
}

// ============================================================================
// Transition validity
// ============================================================================

/// Validate a `current → next` phase transition. Pure-logic; the
/// integration's rollout-driver calls this before applying a
/// transition.
///
/// Legal forward transitions:
/// - `Hidden → OptIn`
/// - `OptIn → Default`
/// - `Default → Cleanup`
///
/// Legal rollback transitions (emergency-only — operator opt-in;
/// the substrate doesn't enforce intent, just the legal pairs):
/// - `OptIn → Hidden` (pull a feature that broke at OptIn)
/// - `Default → OptIn` (pull a default-on feature that regressed)
///
/// Illegal transitions (substrate refuses):
/// - Skipping phases (e.g. `Hidden → Default`).
/// - Returning from `Cleanup` (legacy is gone; can't go back).
#[must_use]
pub fn transition_validity(current: RolloutPhase, next: RolloutPhase) -> TransitionValidity {
    if current == next {
        return TransitionValidity::NoOp;
    }
    match (current, next) {
        (RolloutPhase::Hidden, RolloutPhase::OptIn)
        | (RolloutPhase::OptIn, RolloutPhase::Default)
        | (RolloutPhase::Default, RolloutPhase::Cleanup) => TransitionValidity::Forward,
        (RolloutPhase::OptIn, RolloutPhase::Hidden)
        | (RolloutPhase::Default, RolloutPhase::OptIn) => TransitionValidity::EmergencyRollback,
        _ => TransitionValidity::Illegal,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionValidity {
    /// Same phase — no-op, accepted.
    NoOp,
    /// Legal forward step.
    Forward,
    /// Legal emergency rollback step.
    EmergencyRollback,
    /// Refused by policy (skip-ahead or post-Cleanup return).
    Illegal,
}

impl TransitionValidity {
    #[must_use]
    pub fn is_legal(self) -> bool {
        !matches!(self, Self::Illegal)
    }
}

// ============================================================================
// Release markers
// ============================================================================

/// Bead-defined release-cycle markers. `M0` is the foundation
/// release; later markers slip if work slips (this is a sequence
/// model, not a calendar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Marker {
    #[default]
    M0,
    M1,
    M2,
    M3,
    M4,
    M5,
    M6,
    /// "Beyond M6" — for features whose Cleanup phase is past the
    /// currently-planned roadmap.
    Future,
}

impl Marker {
    /// Linear position for ordering / "is at-or-after" comparisons.
    #[must_use]
    pub const fn ordinal(self) -> u8 {
        match self {
            Self::M0 => 0,
            Self::M1 => 1,
            Self::M2 => 2,
            Self::M3 => 3,
            Self::M4 => 4,
            Self::M5 => 5,
            Self::M6 => 6,
            Self::Future => u8::MAX,
        }
    }
}

// ============================================================================
// FeatureTimeline + RolloutPhaseAtMarker
// ============================================================================

/// Per-feature timeline — when each phase starts. The bead's table
/// values become entries in `FeatureRolloutRegistry::canonical()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureTimeline {
    pub hidden_at: Marker,
    pub opt_in_at: Marker,
    pub default_at: Marker,
    pub cleanup_at: Marker,
}

impl FeatureTimeline {
    /// Compute the rollout phase the feature is in at a given
    /// marker. Returns the latest phase whose marker is at-or-before
    /// the given one — so a feature with
    /// `(hidden=M0, opt_in=M1, default=M2, cleanup=M3)` queried at
    /// `M2` returns `Default`.
    #[must_use]
    pub fn phase_at(&self, marker: Marker) -> RolloutPhase {
        let m = marker.ordinal();
        if m >= self.cleanup_at.ordinal() {
            RolloutPhase::Cleanup
        } else if m >= self.default_at.ordinal() {
            RolloutPhase::Default
        } else if m >= self.opt_in_at.ordinal() {
            RolloutPhase::OptIn
        } else if m >= self.hidden_at.ordinal() {
            RolloutPhase::Hidden
        } else {
            // Before Hidden — feature isn't even built yet; the
            // operator's startup probe should treat this as
            // "feature doesn't exist". We surface it as Hidden
            // (most-conservative) here.
            RolloutPhase::Hidden
        }
    }
}

// ============================================================================
// FeatureRolloutRegistry
// ============================================================================

/// One feature in the registry. The integration layer's startup
/// logic walks this to print "what phase is each feature in?" and
/// emit warnings when the operator override conflicts with the
/// canonical phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRolloutEntry {
    pub feature_id: &'static str,
    pub display_name: &'static str,
    pub timeline: FeatureTimeline,
    pub source_bead: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRolloutRegistry {
    entries: Vec<FeatureRolloutEntry>,
}

impl FeatureRolloutRegistry {
    /// Empty registry. Tests + custom-config consumers build their
    /// own; the canonical bead-driven registry is
    /// `Self::canonical()`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Canonical per-feature timeline from the bead's table. The
    /// integration's startup logic uses this as the source of truth
    /// for "what phase is feature X in this release?".
    #[must_use]
    pub fn canonical() -> Self {
        let entries = vec![
            FeatureRolloutEntry {
                feature_id: "atlas_stability",
                display_name: "Stable versioned atlas",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M0,
                    opt_in_at: Marker::M0,
                    default_at: Marker::M1,
                    cleanup_at: Marker::M2,
                },
                source_bead: "ft-mpc9b.1.1",
            },
            FeatureRolloutEntry {
                feature_id: "per_line_dirty",
                display_name: "Per-line dirty bitmap",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M0,
                    opt_in_at: Marker::M0,
                    default_at: Marker::M1,
                    cleanup_at: Marker::M2,
                },
                source_bead: "ft-mpc9b.1.2",
            },
            FeatureRolloutEntry {
                feature_id: "elastic_instance_buffer",
                display_name: "Elastic instance buffer",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M0,
                    opt_in_at: Marker::M0,
                    default_at: Marker::M1,
                    cleanup_at: Marker::M2,
                },
                source_bead: "ft-mpc9b.1.3",
            },
            FeatureRolloutEntry {
                feature_id: "live_resize_draft",
                display_name: "Live-resize draft mode",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M1,
                    opt_in_at: Marker::M1,
                    default_at: Marker::M2,
                    cleanup_at: Marker::M3,
                },
                source_bead: "ft-mpc9b.2.2",
            },
            FeatureRolloutEntry {
                feature_id: "incremental_reflow",
                display_name: "Incremental reflow",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M1,
                    opt_in_at: Marker::M1,
                    default_at: Marker::M2,
                    cleanup_at: Marker::M3,
                },
                source_bead: "ft-mpc9b.2.3",
            },
            FeatureRolloutEntry {
                feature_id: "metal_direct",
                display_name: "macOS Metal-direct backend",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M2,
                    opt_in_at: Marker::M3,
                    default_at: Marker::M4,
                    cleanup_at: Marker::Future,
                },
                source_bead: "ft-mpc9b.3.1",
            },
            FeatureRolloutEntry {
                feature_id: "wayland_frame_callback",
                display_name: "Wayland frame-callback fix",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M1,
                    opt_in_at: Marker::M1,
                    default_at: Marker::M1,
                    cleanup_at: Marker::M2,
                },
                source_bead: "ft-mpc9b.3.2",
            },
            FeatureRolloutEntry {
                feature_id: "layered_compositor",
                display_name: "Layered compositor",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M3,
                    opt_in_at: Marker::M4,
                    default_at: Marker::M5,
                    cleanup_at: Marker::M6,
                },
                source_bead: "ft-mpc9b.4.1",
            },
            FeatureRolloutEntry {
                feature_id: "floating_panes",
                display_name: "Floating panes",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M4,
                    opt_in_at: Marker::M5,
                    default_at: Marker::M6,
                    cleanup_at: Marker::Future,
                },
                source_bead: "ft-mpc9b.4.2",
            },
            FeatureRolloutEntry {
                feature_id: "wasm_plugins",
                display_name: "WASM plugins",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M4,
                    opt_in_at: Marker::M5,
                    default_at: Marker::M6,
                    cleanup_at: Marker::Future,
                },
                source_bead: "ft-mpc9b.4.3",
            },
            FeatureRolloutEntry {
                feature_id: "conditional_redraw",
                display_name: "Conditional redraw predicate",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M0,
                    opt_in_at: Marker::M1,
                    default_at: Marker::M2,
                    cleanup_at: Marker::M3,
                },
                source_bead: "ft-mpc9b.5.1",
            },
            FeatureRolloutEntry {
                feature_id: "frame_budget",
                display_name: "Per-frame budget allocator",
                timeline: FeatureTimeline {
                    hidden_at: Marker::M1,
                    opt_in_at: Marker::M2,
                    default_at: Marker::M3,
                    cleanup_at: Marker::M4,
                },
                source_bead: "ft-mpc9b.5.2",
            },
        ];
        Self { entries }
    }

    pub fn push(&mut self, entry: FeatureRolloutEntry) {
        self.entries.push(entry);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn entries(&self) -> &[FeatureRolloutEntry] {
        &self.entries
    }

    #[must_use]
    pub fn lookup(&self, feature_id: &str) -> Option<&FeatureRolloutEntry> {
        self.entries.iter().find(|e| e.feature_id == feature_id)
    }

    /// Walk the registry and compute the phase of every feature at
    /// the given marker. Useful for "ft doctor --rollout" or
    /// release-notes generation.
    #[must_use]
    pub fn phases_at(&self, marker: Marker) -> Vec<(&'static str, RolloutPhase)> {
        self.entries
            .iter()
            .map(|e| (e.feature_id, e.timeline.phase_at(marker)))
            .collect()
    }
}

impl Default for FeatureRolloutRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// RolloutState — running mutable state for a single feature
// ============================================================================

/// Mutable per-feature state. The integration's rollout-driver
/// holds one of these per feature; advances it via `transition_to`
/// (which checks `transition_validity` first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutState {
    feature_id: &'static str,
    current: RolloutPhase,
    transitions_applied: u32,
}

impl RolloutState {
    #[must_use]
    pub fn new(feature_id: &'static str) -> Self {
        Self {
            feature_id,
            current: RolloutPhase::Hidden,
            transitions_applied: 0,
        }
    }

    #[must_use]
    pub fn at(feature_id: &'static str, phase: RolloutPhase) -> Self {
        Self {
            feature_id,
            current: phase,
            transitions_applied: 0,
        }
    }

    #[must_use]
    pub fn current(&self) -> RolloutPhase {
        self.current
    }

    #[must_use]
    pub fn feature_id(&self) -> &'static str {
        self.feature_id
    }

    #[must_use]
    pub fn transitions_applied(&self) -> u32 {
        self.transitions_applied
    }

    /// Attempt a transition. Returns the validity decision; on
    /// `Forward` / `EmergencyRollback`, applies the transition. On
    /// `NoOp`, returns NoOp without touching the counter. On
    /// `Illegal`, returns Illegal without applying.
    pub fn transition_to(&mut self, next: RolloutPhase) -> TransitionValidity {
        let v = transition_validity(self.current, next);
        match v {
            TransitionValidity::Forward | TransitionValidity::EmergencyRollback => {
                self.current = next;
                self.transitions_applied = self.transitions_applied.saturating_add(1);
            }
            TransitionValidity::NoOp | TransitionValidity::Illegal => {}
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // RolloutPhase predicates
    // ----------------------------------------------------------------

    #[test]
    fn phase_default_is_hidden() {
        assert_eq!(RolloutPhase::default(), RolloutPhase::Hidden);
    }

    #[test]
    fn phase_is_enabled_by_default() {
        assert!(!RolloutPhase::Hidden.is_enabled_by_default());
        assert!(!RolloutPhase::OptIn.is_enabled_by_default());
        assert!(RolloutPhase::Default.is_enabled_by_default());
        assert!(RolloutPhase::Cleanup.is_enabled_by_default());
    }

    #[test]
    fn phase_is_runtime_toggleable() {
        assert!(!RolloutPhase::Hidden.is_runtime_toggleable());
        assert!(RolloutPhase::OptIn.is_runtime_toggleable());
        assert!(RolloutPhase::Default.is_runtime_toggleable());
        assert!(!RolloutPhase::Cleanup.is_runtime_toggleable());
    }

    #[test]
    fn phase_has_legacy_fallback() {
        assert!(!RolloutPhase::Hidden.has_legacy_fallback());
        assert!(RolloutPhase::OptIn.has_legacy_fallback());
        assert!(RolloutPhase::Default.has_legacy_fallback());
        assert!(!RolloutPhase::Cleanup.has_legacy_fallback());
    }

    // ----------------------------------------------------------------
    // Transition validity
    // ----------------------------------------------------------------

    #[test]
    fn transition_no_op_when_same() {
        for p in [
            RolloutPhase::Hidden,
            RolloutPhase::OptIn,
            RolloutPhase::Default,
            RolloutPhase::Cleanup,
        ] {
            assert_eq!(transition_validity(p, p), TransitionValidity::NoOp);
        }
    }

    #[test]
    fn transition_forward_is_legal_one_step() {
        assert_eq!(
            transition_validity(RolloutPhase::Hidden, RolloutPhase::OptIn),
            TransitionValidity::Forward
        );
        assert_eq!(
            transition_validity(RolloutPhase::OptIn, RolloutPhase::Default),
            TransitionValidity::Forward
        );
        assert_eq!(
            transition_validity(RolloutPhase::Default, RolloutPhase::Cleanup),
            TransitionValidity::Forward
        );
    }

    #[test]
    fn transition_emergency_rollback_is_legal() {
        assert_eq!(
            transition_validity(RolloutPhase::OptIn, RolloutPhase::Hidden),
            TransitionValidity::EmergencyRollback
        );
        assert_eq!(
            transition_validity(RolloutPhase::Default, RolloutPhase::OptIn),
            TransitionValidity::EmergencyRollback
        );
    }

    #[test]
    fn transition_skip_is_illegal() {
        // Skipping OptIn or Default is illegal.
        assert_eq!(
            transition_validity(RolloutPhase::Hidden, RolloutPhase::Default),
            TransitionValidity::Illegal
        );
        assert_eq!(
            transition_validity(RolloutPhase::Hidden, RolloutPhase::Cleanup),
            TransitionValidity::Illegal
        );
        assert_eq!(
            transition_validity(RolloutPhase::OptIn, RolloutPhase::Cleanup),
            TransitionValidity::Illegal
        );
    }

    #[test]
    fn transition_post_cleanup_is_illegal() {
        for p in [
            RolloutPhase::Hidden,
            RolloutPhase::OptIn,
            RolloutPhase::Default,
        ] {
            assert_eq!(
                transition_validity(RolloutPhase::Cleanup, p),
                TransitionValidity::Illegal,
                "Cleanup → {p:?} must be Illegal (legacy gone)"
            );
        }
    }

    #[test]
    fn transition_validity_is_legal_helper() {
        assert!(TransitionValidity::Forward.is_legal());
        assert!(TransitionValidity::EmergencyRollback.is_legal());
        assert!(TransitionValidity::NoOp.is_legal());
        assert!(!TransitionValidity::Illegal.is_legal());
    }

    // ----------------------------------------------------------------
    // Marker
    // ----------------------------------------------------------------

    #[test]
    fn marker_default_is_m0() {
        assert_eq!(Marker::default(), Marker::M0);
    }

    #[test]
    fn marker_ordinal_is_monotonic() {
        assert!(Marker::M0.ordinal() < Marker::M1.ordinal());
        assert!(Marker::M1.ordinal() < Marker::M2.ordinal());
        assert!(Marker::M5.ordinal() < Marker::M6.ordinal());
        assert!(Marker::M6.ordinal() < Marker::Future.ordinal());
    }

    #[test]
    fn marker_future_is_max() {
        assert_eq!(Marker::Future.ordinal(), u8::MAX);
    }

    // ----------------------------------------------------------------
    // FeatureTimeline.phase_at
    // ----------------------------------------------------------------

    fn timeline(h: Marker, o: Marker, d: Marker, c: Marker) -> FeatureTimeline {
        FeatureTimeline {
            hidden_at: h,
            opt_in_at: o,
            default_at: d,
            cleanup_at: c,
        }
    }

    #[test]
    fn phase_at_walks_through_lifecycle() {
        let t = timeline(Marker::M0, Marker::M1, Marker::M2, Marker::M3);
        assert_eq!(t.phase_at(Marker::M0), RolloutPhase::Hidden);
        assert_eq!(t.phase_at(Marker::M1), RolloutPhase::OptIn);
        assert_eq!(t.phase_at(Marker::M2), RolloutPhase::Default);
        assert_eq!(t.phase_at(Marker::M3), RolloutPhase::Cleanup);
        assert_eq!(t.phase_at(Marker::M4), RolloutPhase::Cleanup);
    }

    #[test]
    fn phase_at_compressed_timeline_can_skip_to_default_quickly() {
        // wayland_frame_callback's bead timeline:
        // hidden_at = opt_in_at = default_at = M1; cleanup_at = M2.
        let t = timeline(Marker::M1, Marker::M1, Marker::M1, Marker::M2);
        assert_eq!(t.phase_at(Marker::M0), RolloutPhase::Hidden);
        assert_eq!(t.phase_at(Marker::M1), RolloutPhase::Default);
        assert_eq!(t.phase_at(Marker::M2), RolloutPhase::Cleanup);
    }

    #[test]
    fn phase_at_with_future_cleanup_stays_at_default_through_m6() {
        // Metal-direct: cleanup_at = Future.
        let t = timeline(Marker::M2, Marker::M3, Marker::M4, Marker::Future);
        assert_eq!(t.phase_at(Marker::M4), RolloutPhase::Default);
        assert_eq!(t.phase_at(Marker::M5), RolloutPhase::Default);
        assert_eq!(t.phase_at(Marker::M6), RolloutPhase::Default);
    }

    // ----------------------------------------------------------------
    // FeatureRolloutRegistry::canonical
    // ----------------------------------------------------------------

    #[test]
    fn canonical_registry_has_12_features_per_bead_table() {
        let r = FeatureRolloutRegistry::canonical();
        assert_eq!(r.len(), 12, "bead's per-feature table has 12 rows");
    }

    #[test]
    fn canonical_registry_lookup_by_feature_id() {
        let r = FeatureRolloutRegistry::canonical();
        let entry = r.lookup("atlas_stability").unwrap();
        assert_eq!(entry.source_bead, "ft-mpc9b.1.1");
        assert_eq!(entry.timeline.default_at, Marker::M1);
    }

    #[test]
    fn canonical_registry_lookup_unknown_returns_none() {
        let r = FeatureRolloutRegistry::canonical();
        assert!(r.lookup("nonexistent_feature").is_none());
    }

    #[test]
    fn canonical_registry_phases_at_m0_mostly_hidden() {
        let r = FeatureRolloutRegistry::canonical();
        let phases = r.phases_at(Marker::M0);
        // At M0 only the Sub-epic-1 features (atlas_stability,
        // per_line_dirty, elastic_instance_buffer) are at OptIn
        // (their hidden_at = opt_in_at = M0). conditional_redraw has
        // hidden_at=M0, opt_in_at=M1 → Hidden at M0. Everything else
        // is also Hidden at M0.
        let m0_optin: Vec<_> = phases
            .iter()
            .filter(|(_, p)| *p == RolloutPhase::OptIn)
            .map(|(id, _)| *id)
            .collect();
        // Assert specific features are OptIn at M0.
        assert!(m0_optin.contains(&"atlas_stability"));
        assert!(m0_optin.contains(&"per_line_dirty"));
        assert!(m0_optin.contains(&"elastic_instance_buffer"));
        // conditional_redraw is Hidden at M0 (opt_in_at=M1).
        let cr_phase = phases
            .iter()
            .find(|(id, _)| *id == "conditional_redraw")
            .unwrap()
            .1;
        assert_eq!(cr_phase, RolloutPhase::Hidden);
        // Floating panes should still be Hidden at M0.
        let floating_phase = phases
            .iter()
            .find(|(id, _)| *id == "floating_panes")
            .unwrap()
            .1;
        assert_eq!(floating_phase, RolloutPhase::Hidden);
    }

    #[test]
    fn canonical_registry_phases_at_m4_metal_direct_default() {
        let r = FeatureRolloutRegistry::canonical();
        let phases = r.phases_at(Marker::M4);
        let metal_phase = phases
            .iter()
            .find(|(id, _)| *id == "metal_direct")
            .unwrap()
            .1;
        assert_eq!(metal_phase, RolloutPhase::Default);
    }

    #[test]
    fn canonical_registry_compressed_wayland_at_m1_default() {
        let r = FeatureRolloutRegistry::canonical();
        let phases = r.phases_at(Marker::M1);
        let wayland_phase = phases
            .iter()
            .find(|(id, _)| *id == "wayland_frame_callback")
            .unwrap()
            .1;
        // Wayland's compressed timeline: M1 = Default.
        assert_eq!(wayland_phase, RolloutPhase::Default);
    }

    // ----------------------------------------------------------------
    // RolloutState
    // ----------------------------------------------------------------

    #[test]
    fn rollout_state_starts_hidden() {
        let s = RolloutState::new("test");
        assert_eq!(s.current(), RolloutPhase::Hidden);
        assert_eq!(s.transitions_applied(), 0);
        assert_eq!(s.feature_id(), "test");
    }

    #[test]
    fn rollout_state_at_initialises_to_phase() {
        let s = RolloutState::at("test", RolloutPhase::Default);
        assert_eq!(s.current(), RolloutPhase::Default);
    }

    #[test]
    fn rollout_state_forward_transition_advances() {
        let mut s = RolloutState::new("test");
        let v = s.transition_to(RolloutPhase::OptIn);
        assert_eq!(v, TransitionValidity::Forward);
        assert_eq!(s.current(), RolloutPhase::OptIn);
        assert_eq!(s.transitions_applied(), 1);
    }

    #[test]
    fn rollout_state_no_op_does_not_bump_counter() {
        let mut s = RolloutState::at("test", RolloutPhase::OptIn);
        let v = s.transition_to(RolloutPhase::OptIn);
        assert_eq!(v, TransitionValidity::NoOp);
        assert_eq!(s.transitions_applied(), 0);
    }

    #[test]
    fn rollout_state_illegal_transition_does_not_advance() {
        let mut s = RolloutState::new("test");
        let v = s.transition_to(RolloutPhase::Cleanup);
        assert_eq!(v, TransitionValidity::Illegal);
        assert_eq!(
            s.current(),
            RolloutPhase::Hidden,
            "must not advance on Illegal"
        );
        assert_eq!(s.transitions_applied(), 0);
    }

    #[test]
    fn rollout_state_emergency_rollback_advances() {
        let mut s = RolloutState::at("test", RolloutPhase::Default);
        let v = s.transition_to(RolloutPhase::OptIn);
        assert_eq!(v, TransitionValidity::EmergencyRollback);
        assert_eq!(s.current(), RolloutPhase::OptIn);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_full_lifecycle_advance() {
        // Walk a feature through Hidden → OptIn → Default → Cleanup.
        let mut s = RolloutState::new("atlas_stability");
        assert_eq!(
            s.transition_to(RolloutPhase::OptIn),
            TransitionValidity::Forward
        );
        assert_eq!(
            s.transition_to(RolloutPhase::Default),
            TransitionValidity::Forward
        );
        assert_eq!(
            s.transition_to(RolloutPhase::Cleanup),
            TransitionValidity::Forward
        );
        assert_eq!(s.current(), RolloutPhase::Cleanup);
        assert_eq!(s.transitions_applied(), 3);
        // Post-cleanup attempts illegal.
        assert_eq!(
            s.transition_to(RolloutPhase::Default),
            TransitionValidity::Illegal
        );
        assert_eq!(s.current(), RolloutPhase::Cleanup);
    }

    #[test]
    fn scenario_emergency_rollback_then_re_advance() {
        // Default → OptIn (emergency) → Default (re-advance after fix).
        let mut s = RolloutState::at("frame_budget", RolloutPhase::Default);
        assert_eq!(
            s.transition_to(RolloutPhase::OptIn),
            TransitionValidity::EmergencyRollback
        );
        assert_eq!(s.current(), RolloutPhase::OptIn);
        assert_eq!(
            s.transition_to(RolloutPhase::Default),
            TransitionValidity::Forward
        );
        assert_eq!(s.current(), RolloutPhase::Default);
        assert_eq!(s.transitions_applied(), 2);
    }

    #[test]
    fn scenario_release_advance_walks_registry() {
        // Move from M0 → M1 release: every feature whose timeline
        // changes phase at M1 advances.
        let r = FeatureRolloutRegistry::canonical();
        let phases_m0 = r.phases_at(Marker::M0);
        let phases_m1 = r.phases_at(Marker::M1);
        // Expect at least the Sub-epic-1 features to advance from
        // OptIn at M0 to Default at M1.
        let advanced: Vec<_> = phases_m0
            .iter()
            .zip(phases_m1.iter())
            .filter(|((_, p0), (_, p1))| p0 != p1)
            .map(|((id, _), _)| *id)
            .collect();
        assert!(advanced.contains(&"atlas_stability"));
        assert!(advanced.contains(&"per_line_dirty"));
        assert!(advanced.contains(&"elastic_instance_buffer"));
        // wayland_frame_callback also advances (Hidden → Default).
        assert!(advanced.contains(&"wayland_frame_callback"));
    }
}
