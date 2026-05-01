//! iTerm2 OSC 1337 cluster — contract layer
//! ([BR-TERM-EMULATOR-UPLIFT-2.1.3.cont] / `ft-tzusd`).
//!
//! iTerm2's `OSC 1337` extension covers a cluster of in-band
//! protocols (`File=...`, `SetProfile=...`, `SetColors=...`,
//! `MultipartFile`...) that frankenterm must parse, gate
//! through the security alert machinery, and apply (or deny)
//! per the operator's allowlist.
//!
//! ## Already shipped (parent bead `ft-fy4ty`)
//!
//! - Term-layer security gate at `frankenterm/term/src/...`:
//!   `Alert::SetProfileRequested` variant + explicit dispatch
//!   arm + 6 integration tests. Profile state never mutates
//!   silently — every `OSC 1337 SetProfile=` raises the alert
//!   for the GUI to confirm.
//!
//! ## What this module ships
//!
//! - [`ItermFileArgument`] — typed envelope covering the full
//!   `File=...` argument matrix the bead's action #2 audits:
//!   `name`, `size`, `width`, `height`, `preserveAspectRatio`,
//!   `inline`. Each argument has a typed shape so the parser
//!   audit catches uncovered combinations.
//! - [`MultipartFileAccumulator`] — state machine for the
//!   bead's action #4 (chunked file transfers split across
//!   multiple OSC sequences). Enforces **depth cap** (max
//!   chunks) and **total-size cap** (max bytes accumulated)
//!   per the bead's security framing.
//! - [`SetColorsPaletteEntry`] — typed envelope for the
//!   bead's action #3 SetColors variant. Names the operation
//!   so the security gate decides whether to apply or alert.
//! - [`AllowlistDecision`] — operator response to a
//!   `SetProfileRequested` alert: `Allow`, `Deny`,
//!   `AlwaysAllow { app_id }`. Mirrors the bead's action #1
//!   GUI prompt UI.
//! - [`ProfileAllowlist`] — per-app persistence shape (the
//!   bead's "remember-per-name" rollout step).
//! - [`Iterm2Osc1337Health`] — `ft doctor` snapshot mirroring
//!   this session's `*Health` shape.
//! - [`ConformanceFixtureCorpus`] — the bead's action #5
//!   corpus contract (3 fixture slugs: `imgcat`, `setprofile`,
//!   `setcolors`).
//! - [`RolloutPhase`] — the bead's feature-flag staging
//!   (Hidden / OptIn / Default) per `ft-mpc9b.9` substrate.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// File= argument envelope
// ============================================================================

/// Argument matrix for `OSC 1337 File=...`. The bead's
/// action #2 audits parser coverage of this shape; each
/// field maps 1:1 onto an iTerm2 documented argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItermFileArgument {
    /// Caller-supplied filename. Optional per spec; the
    /// renderer falls back to a generic placeholder.
    pub name: Option<String>,
    /// Total expected payload size in bytes. Used to short-
    /// circuit decode if the size cap will be exceeded.
    pub size: Option<u64>,
    /// Display width hint. Either an explicit cell count or
    /// a special token (`auto`, `<N>px`, `<N>%`).
    pub width: Option<DimensionHint>,
    /// Display height hint.
    pub height: Option<DimensionHint>,
    /// Whether to preserve aspect ratio when scaling.
    pub preserve_aspect_ratio: Option<bool>,
    /// True iff the image is rendered inline at the cursor
    /// position (the imgcat default).
    pub inline: Option<bool>,
    /// Catch-all for arguments the parser saw but doesn't
    /// have a typed slot for. The audit asserts this is
    /// empty for a representative corpus; growth here
    /// indicates the parser is leaking arguments.
    #[serde(default)]
    pub unknown_args: BTreeMap<String, String>,
}

impl ItermFileArgument {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            name: None,
            size: None,
            width: None,
            height: None,
            preserve_aspect_ratio: None,
            inline: None,
            unknown_args: BTreeMap::new(),
        }
    }

    /// True iff every known argument has a typed slot —
    /// `unknown_args` is empty.
    #[must_use]
    pub fn is_fully_audited(&self) -> bool {
        self.unknown_args.is_empty()
    }
}

/// Dimension hint as iTerm2 specifies it. Variants use named
/// fields (rather than newtype) because serde's internally-
/// tagged enum representation can't carry primitive newtype
/// payloads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DimensionHint {
    /// Auto-size based on content + remaining cells.
    Auto,
    /// Explicit cell count.
    Cells { count: u32 },
    /// Explicit pixel count.
    Pixels { count: u32 },
    /// Percentage of the terminal dimension (0..=100).
    Percent { value: u8 },
}

// ============================================================================
// Multipart file accumulator
// ============================================================================

/// State for accumulating a multipart `OSC 1337 File=...`
/// transfer. iTerm2 splits very large file payloads across
/// multiple OSC sequences; the accumulator gathers them under
/// a depth + total-size cap.
///
/// Caps come from the security framing in the bead: a
/// runaway multipart could exhaust memory; the cap forces a
/// `MultipartViolation::SizeCapExceeded` rather than letting
/// the renderer OOM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartFileAccumulator {
    /// Caller-declared total payload size from the leading
    /// `File=` arg (may be `None` if size wasn't declared
    /// upfront).
    pub declared_size: Option<u64>,
    /// Bytes received so far across all chunks.
    pub bytes_received: u64,
    /// Number of chunks received so far.
    pub chunks_received: u32,
    /// Maximum allowed chunk count before the accumulator
    /// rejects further input.
    pub depth_cap: u32,
    /// Maximum allowed total bytes before the accumulator
    /// rejects further input.
    pub size_cap: u64,
    /// Whether the accumulator has seen a final chunk.
    pub finalized: bool,
}

impl MultipartFileAccumulator {
    /// Default caps — 1024 chunks × 64 MiB total. Tunable
    /// via `[tuning.iterm2_osc1337]` config; the constants
    /// here are the spec defaults.
    pub const DEFAULT_DEPTH_CAP: u32 = 1024;
    pub const DEFAULT_SIZE_CAP: u64 = 64 * 1024 * 1024;

    /// Construct with default caps.
    #[must_use]
    pub const fn new() -> Self {
        Self::with_caps(Self::DEFAULT_DEPTH_CAP, Self::DEFAULT_SIZE_CAP)
    }

    #[must_use]
    pub const fn with_caps(depth_cap: u32, size_cap: u64) -> Self {
        Self {
            declared_size: None,
            bytes_received: 0,
            chunks_received: 0,
            depth_cap,
            size_cap,
            finalized: false,
        }
    }

    /// Record the leading `File=size=<n>` argument.
    pub fn declare_size(&mut self, size: u64) {
        self.declared_size = Some(size);
    }

    /// Append one chunk's bytes. Returns the resulting
    /// outcome. Mutates the accumulator only on `Accepted`.
    pub fn append_chunk(&mut self, chunk_size: u64) -> MultipartOutcome {
        if self.finalized {
            return MultipartOutcome::Denied {
                reason: MultipartDenialReason::AlreadyFinalized,
            };
        }
        if self.chunks_received >= self.depth_cap {
            return MultipartOutcome::Denied {
                reason: MultipartDenialReason::DepthCapExceeded {
                    cap: self.depth_cap,
                },
            };
        }
        let next_total = self.bytes_received.saturating_add(chunk_size);
        if next_total > self.size_cap {
            return MultipartOutcome::Denied {
                reason: MultipartDenialReason::SizeCapExceeded {
                    cap: self.size_cap,
                    attempted: next_total,
                },
            };
        }
        self.bytes_received = next_total;
        self.chunks_received += 1;
        MultipartOutcome::Accepted {
            chunks_received: self.chunks_received,
            bytes_received: self.bytes_received,
        }
    }

    /// Mark the accumulator finalized. After this, no more
    /// chunks may be appended. Returns `Denied` if the
    /// declared size doesn't match received bytes (likely
    /// dropped-chunk indicator).
    pub fn finalize(&mut self) -> MultipartOutcome {
        if self.finalized {
            return MultipartOutcome::Denied {
                reason: MultipartDenialReason::AlreadyFinalized,
            };
        }
        if let Some(declared) = self.declared_size {
            if declared != self.bytes_received {
                return MultipartOutcome::Denied {
                    reason: MultipartDenialReason::SizeMismatch {
                        declared,
                        received: self.bytes_received,
                    },
                };
            }
        }
        self.finalized = true;
        MultipartOutcome::Finalized {
            total_bytes: self.bytes_received,
            total_chunks: self.chunks_received,
        }
    }
}

impl Default for MultipartFileAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MultipartOutcome {
    Accepted {
        chunks_received: u32,
        bytes_received: u64,
    },
    Finalized {
        total_bytes: u64,
        total_chunks: u32,
    },
    Denied {
        reason: MultipartDenialReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum MultipartDenialReason {
    DepthCapExceeded { cap: u32 },
    SizeCapExceeded { cap: u64, attempted: u64 },
    SizeMismatch { declared: u64, received: u64 },
    AlreadyFinalized,
}

// ============================================================================
// SetColors
// ============================================================================

/// One palette entry from `OSC 1337 SetColors=...`. The bead's
/// action #3 audits whether each variant is parsed +
/// dispatched + applied; this typed envelope names the slot
/// the dispatcher fills.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "slot", rename_all = "snake_case")]
pub enum SetColorsPaletteEntry {
    /// ANSI color index 0..=15.
    AnsiIndex { index: u8, rgb_hex: String },
    /// Named slot (`fg`, `bg`, `bold`, `selbg`, `selfg`,
    /// `curfg`, `curbg`, `link`, etc.).
    Named { name: String, rgb_hex: String },
    /// 256-color palette index 16..=255.
    Index256 { index: u8, rgb_hex: String },
}

impl SetColorsPaletteEntry {
    /// Whether this entry is a privileged slot the security
    /// gate should alert on (changing fg/bg/curfg/curbg
    /// affects readability; the alert lets the user confirm).
    #[must_use]
    pub fn is_privileged_slot(&self) -> bool {
        match self {
            Self::Named { name, .. } => matches!(
                name.as_str(),
                "fg" | "bg" | "curfg" | "curbg" | "selbg" | "selfg"
            ),
            Self::AnsiIndex { .. } | Self::Index256 { .. } => false,
        }
    }
}

// ============================================================================
// SetProfile allowlist
// ============================================================================

/// Operator response to a `SetProfileRequested` alert.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum AllowlistDecision {
    Allow,
    Deny,
    AlwaysAllow {
        /// Stable app identifier the allowlist remembers.
        app_id: String,
    },
}

/// Per-app allowlist for the bead's "remember-per-name"
/// rollout step. The bead's GUI prompt persists `AlwaysAllow`
/// decisions here; future `SetProfileRequested` alerts for
/// the same `app_id` short-circuit through the gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileAllowlist {
    /// Set of app_ids the operator has marked AlwaysAllow.
    pub always_allowed: std::collections::BTreeSet<String>,
}

impl ProfileAllowlist {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply an `AllowlistDecision`. Returns true iff the
    /// allowlist was mutated.
    pub fn apply(&mut self, decision: &AllowlistDecision) -> bool {
        match decision {
            AllowlistDecision::AlwaysAllow { app_id } => self.always_allowed.insert(app_id.clone()),
            AllowlistDecision::Allow | AllowlistDecision::Deny => false,
        }
    }

    /// Whether `app_id` is on the AlwaysAllow list.
    #[must_use]
    pub fn is_always_allowed(&self, app_id: &str) -> bool {
        self.always_allowed.contains(app_id)
    }

    /// Resolve a fresh request to a final decision. If
    /// already allowlisted, return Allow without prompting;
    /// otherwise the caller (GUI) prompts and feeds the
    /// resulting decision back via `apply`.
    #[must_use]
    pub fn resolve(&self, app_id: &str) -> Option<AllowlistDecision> {
        if self.is_always_allowed(app_id) {
            Some(AllowlistDecision::Allow)
        } else {
            None
        }
    }
}

// ============================================================================
// Conformance corpus
// ============================================================================

/// The bead's action #5 corpus — 3 fixture slugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceFixture {
    /// `tests/golden/iterm2/imgcat/` — recorded imgcat
    /// invocation (the canonical inline-image test).
    Imgcat,
    /// `tests/golden/iterm2/setprofile/` — recorded profile-
    /// switch attempt, exercises the security gate.
    SetProfile,
    /// `tests/golden/iterm2/setcolors/` — palette mutation
    /// sequence.
    SetColors,
}

impl ConformanceFixture {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Imgcat => "imgcat",
            Self::SetProfile => "setprofile",
            Self::SetColors => "setcolors",
        }
    }

    pub const ALL: &'static [Self] = &[Self::Imgcat, Self::SetProfile, Self::SetColors];
}

/// The full corpus — 3 fixtures, each at
/// `tests/golden/iterm2/<slug>/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConformanceFixtureCorpus {
    pub fixtures: Vec<ConformanceFixture>,
}

impl ConformanceFixtureCorpus {
    #[must_use]
    pub fn full() -> Self {
        Self {
            fixtures: ConformanceFixture::ALL.to_vec(),
        }
    }

    #[must_use]
    pub fn fixture_path(fixture: ConformanceFixture) -> String {
        format!("tests/golden/iterm2/{}", fixture.slug())
    }
}

// ============================================================================
// Rollout staging
// ============================================================================

/// Feature-flag rollout phase. Mirrors the bead's required
/// staging: always-deny → prompt-each-time →
/// remember-per-name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutPhase {
    /// Default-deny every SetProfileRequested. No prompt.
    AlwaysDeny,
    /// Prompt the user for every SetProfileRequested. No
    /// allowlist persistence.
    PromptEachTime,
    /// Prompt + allow `AlwaysAllow` decisions to persist
    /// per app_id.
    RememberPerName,
}

impl RolloutPhase {
    /// Whether the user is shown a prompt at this phase.
    #[must_use]
    pub const fn user_prompted(self) -> bool {
        matches!(self, Self::PromptEachTime | Self::RememberPerName)
    }

    /// Whether the allowlist persists `AlwaysAllow` decisions.
    #[must_use]
    pub const fn allowlist_persists(self) -> bool {
        matches!(self, Self::RememberPerName)
    }
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` snapshot for the iTerm2 OSC 1337 cluster.
/// Mirrors this session's `*Health` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Iterm2Osc1337Health {
    pub rollout_phase: RolloutPhase,
    pub allowlist_size: u32,
    pub set_profile_alerts_total: u64,
    pub set_profile_allows_total: u64,
    pub set_profile_denies_total: u64,
    pub set_colors_dispatched_total: u64,
    pub multipart_finalizes_total: u64,
    pub multipart_denies_total: u64,
    pub file_args_with_unknown_args_total: u64,
}

impl Iterm2Osc1337Health {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            rollout_phase: RolloutPhase::AlwaysDeny,
            allowlist_size: 0,
            set_profile_alerts_total: 0,
            set_profile_allows_total: 0,
            set_profile_denies_total: 0,
            set_colors_dispatched_total: 0,
            multipart_finalizes_total: 0,
            multipart_denies_total: 0,
            file_args_with_unknown_args_total: 0,
        }
    }

    /// Whether the allow-vs-deny ratio is balanced — high
    /// allow ratio in `AlwaysDeny` phase indicates a bug
    /// (the gate isn't actually blocking).
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        if matches!(self.rollout_phase, RolloutPhase::AlwaysDeny) {
            self.set_profile_allows_total == 0
        } else {
            true
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // ItermFileArgument
    // ------------------------------------------------------------------------

    #[test]
    fn empty_iterm_file_argument_is_fully_audited() {
        let f = ItermFileArgument::empty();
        assert!(f.is_fully_audited());
    }

    #[test]
    fn unknown_args_marks_unaudited() {
        let mut f = ItermFileArgument::empty();
        f.unknown_args
            .insert("strange".to_string(), "1".to_string());
        assert!(!f.is_fully_audited());
    }

    // ------------------------------------------------------------------------
    // DimensionHint
    // ------------------------------------------------------------------------

    #[test]
    fn dimension_hint_serde_roundtrip() {
        for hint in [
            DimensionHint::Auto,
            DimensionHint::Cells { count: 80 },
            DimensionHint::Pixels { count: 640 },
            DimensionHint::Percent { value: 50 },
        ] {
            let json = serde_json::to_string(&hint).unwrap();
            let parsed: DimensionHint = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, hint);
        }
    }

    // ------------------------------------------------------------------------
    // MultipartFileAccumulator
    // ------------------------------------------------------------------------

    #[test]
    fn multipart_default_caps_match_constants() {
        let acc = MultipartFileAccumulator::default();
        assert_eq!(acc.depth_cap, 1024);
        assert_eq!(acc.size_cap, 64 * 1024 * 1024);
        assert!(!acc.finalized);
    }

    #[test]
    fn multipart_canonical_three_chunk_finalize() {
        let mut acc = MultipartFileAccumulator::with_caps(8, 1024);
        acc.declare_size(300);
        for chunk in [100u64, 100, 100] {
            assert!(matches!(
                acc.append_chunk(chunk),
                MultipartOutcome::Accepted { .. }
            ));
        }
        let outcome = acc.finalize();
        assert_eq!(
            outcome,
            MultipartOutcome::Finalized {
                total_bytes: 300,
                total_chunks: 3,
            }
        );
        assert!(acc.finalized);
    }

    #[test]
    fn multipart_depth_cap_blocks_extra_chunks() {
        let mut acc = MultipartFileAccumulator::with_caps(2, 1024);
        assert!(matches!(
            acc.append_chunk(10),
            MultipartOutcome::Accepted { .. }
        ));
        assert!(matches!(
            acc.append_chunk(10),
            MultipartOutcome::Accepted { .. }
        ));
        // Third chunk exceeds depth_cap=2.
        assert_eq!(
            acc.append_chunk(10),
            MultipartOutcome::Denied {
                reason: MultipartDenialReason::DepthCapExceeded { cap: 2 },
            }
        );
    }

    #[test]
    fn multipart_size_cap_blocks_oversized_chunk() {
        let mut acc = MultipartFileAccumulator::with_caps(8, 100);
        assert!(matches!(
            acc.append_chunk(60),
            MultipartOutcome::Accepted { .. }
        ));
        // Next chunk would push to 161, over cap=100.
        assert_eq!(
            acc.append_chunk(101),
            MultipartOutcome::Denied {
                reason: MultipartDenialReason::SizeCapExceeded {
                    cap: 100,
                    attempted: 161,
                },
            }
        );
        // State unchanged on denial.
        assert_eq!(acc.bytes_received, 60);
        assert_eq!(acc.chunks_received, 1);
    }

    #[test]
    fn multipart_size_mismatch_on_finalize_is_denied() {
        let mut acc = MultipartFileAccumulator::with_caps(8, 1024);
        acc.declare_size(100);
        acc.append_chunk(50);
        let outcome = acc.finalize();
        assert_eq!(
            outcome,
            MultipartOutcome::Denied {
                reason: MultipartDenialReason::SizeMismatch {
                    declared: 100,
                    received: 50,
                },
            }
        );
        // NOT marked finalized — caller can investigate.
        assert!(!acc.finalized);
    }

    #[test]
    fn multipart_double_finalize_denied() {
        let mut acc = MultipartFileAccumulator::with_caps(8, 1024);
        acc.append_chunk(10);
        acc.finalize();
        let outcome = acc.finalize();
        assert_eq!(
            outcome,
            MultipartOutcome::Denied {
                reason: MultipartDenialReason::AlreadyFinalized,
            }
        );
    }

    #[test]
    fn multipart_chunk_after_finalize_denied() {
        let mut acc = MultipartFileAccumulator::with_caps(8, 1024);
        acc.append_chunk(10);
        acc.finalize();
        let outcome = acc.append_chunk(10);
        assert_eq!(
            outcome,
            MultipartOutcome::Denied {
                reason: MultipartDenialReason::AlreadyFinalized,
            }
        );
    }

    // ------------------------------------------------------------------------
    // SetColorsPaletteEntry
    // ------------------------------------------------------------------------

    #[test]
    fn ansi_indexed_slots_are_unprivileged() {
        let entry = SetColorsPaletteEntry::AnsiIndex {
            index: 5,
            rgb_hex: "ff00ff".to_string(),
        };
        assert!(!entry.is_privileged_slot());
    }

    #[test]
    fn fg_bg_named_slots_are_privileged() {
        for name in ["fg", "bg", "curfg", "curbg", "selbg", "selfg"] {
            let entry = SetColorsPaletteEntry::Named {
                name: name.to_string(),
                rgb_hex: "abcdef".to_string(),
            };
            assert!(entry.is_privileged_slot(), "{name} should be privileged");
        }
    }

    #[test]
    fn other_named_slots_unprivileged() {
        let entry = SetColorsPaletteEntry::Named {
            name: "link".to_string(),
            rgb_hex: "abcdef".to_string(),
        };
        assert!(!entry.is_privileged_slot());
    }

    // ------------------------------------------------------------------------
    // ProfileAllowlist
    // ------------------------------------------------------------------------

    #[test]
    fn allowlist_apply_always_allow_persists() {
        let mut a = ProfileAllowlist::new();
        let changed = a.apply(&AllowlistDecision::AlwaysAllow {
            app_id: "claude_code".to_string(),
        });
        assert!(changed);
        assert!(a.is_always_allowed("claude_code"));
    }

    #[test]
    fn allowlist_apply_one_shot_decisions_do_not_persist() {
        let mut a = ProfileAllowlist::new();
        assert!(!a.apply(&AllowlistDecision::Allow));
        assert!(!a.apply(&AllowlistDecision::Deny));
        assert!(a.always_allowed.is_empty());
    }

    #[test]
    fn allowlist_resolve_returns_allow_for_known_app() {
        let mut a = ProfileAllowlist::new();
        a.apply(&AllowlistDecision::AlwaysAllow {
            app_id: "claude_code".to_string(),
        });
        assert_eq!(a.resolve("claude_code"), Some(AllowlistDecision::Allow));
        assert_eq!(a.resolve("untrusted_app"), None);
    }

    #[test]
    fn allowlist_apply_idempotent_on_duplicate_always_allow() {
        let mut a = ProfileAllowlist::new();
        let changed1 = a.apply(&AllowlistDecision::AlwaysAllow {
            app_id: "x".to_string(),
        });
        let changed2 = a.apply(&AllowlistDecision::AlwaysAllow {
            app_id: "x".to_string(),
        });
        assert!(changed1);
        assert!(!changed2);
        assert_eq!(a.always_allowed.len(), 1);
    }

    // ------------------------------------------------------------------------
    // RolloutPhase
    // ------------------------------------------------------------------------

    #[test]
    fn rollout_always_deny_does_not_prompt() {
        assert!(!RolloutPhase::AlwaysDeny.user_prompted());
        assert!(!RolloutPhase::AlwaysDeny.allowlist_persists());
    }

    #[test]
    fn rollout_prompt_each_time_does_not_persist() {
        assert!(RolloutPhase::PromptEachTime.user_prompted());
        assert!(!RolloutPhase::PromptEachTime.allowlist_persists());
    }

    #[test]
    fn rollout_remember_per_name_persists() {
        assert!(RolloutPhase::RememberPerName.user_prompted());
        assert!(RolloutPhase::RememberPerName.allowlist_persists());
    }

    #[test]
    fn rollout_phase_is_ordered() {
        assert!(RolloutPhase::AlwaysDeny < RolloutPhase::PromptEachTime);
        assert!(RolloutPhase::PromptEachTime < RolloutPhase::RememberPerName);
    }

    // ------------------------------------------------------------------------
    // Conformance corpus
    // ------------------------------------------------------------------------

    #[test]
    fn corpus_has_three_fixtures() {
        let c = ConformanceFixtureCorpus::full();
        assert_eq!(c.fixtures.len(), 3);
        assert_eq!(ConformanceFixture::ALL.len(), 3);
    }

    #[test]
    fn fixture_path_is_under_iterm2_dir() {
        assert_eq!(
            ConformanceFixtureCorpus::fixture_path(ConformanceFixture::Imgcat),
            "tests/golden/iterm2/imgcat"
        );
        assert_eq!(
            ConformanceFixtureCorpus::fixture_path(ConformanceFixture::SetProfile),
            "tests/golden/iterm2/setprofile"
        );
        assert_eq!(
            ConformanceFixtureCorpus::fixture_path(ConformanceFixture::SetColors),
            "tests/golden/iterm2/setcolors"
        );
    }

    #[test]
    fn fixture_slugs_distinct() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for f in ConformanceFixture::ALL {
            assert!(seen.insert(f.slug()), "dup {}", f.slug());
        }
    }

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn baseline_health_safe_in_always_deny() {
        let h = Iterm2Osc1337Health::baseline();
        assert!(h.is_safe());
        // Default phase is the strictest.
        assert_eq!(h.rollout_phase, RolloutPhase::AlwaysDeny);
    }

    #[test]
    fn health_unsafe_when_always_deny_records_an_allow() {
        let h = Iterm2Osc1337Health {
            rollout_phase: RolloutPhase::AlwaysDeny,
            set_profile_allows_total: 1, // bug: gate didn't block
            ..Iterm2Osc1337Health::baseline()
        };
        assert!(!h.is_safe());
    }

    #[test]
    fn health_safe_when_prompt_phase_records_allows() {
        let h = Iterm2Osc1337Health {
            rollout_phase: RolloutPhase::PromptEachTime,
            set_profile_allows_total: 5,
            ..Iterm2Osc1337Health::baseline()
        };
        assert!(h.is_safe());
    }

    #[test]
    fn health_serde_roundtrips() {
        let h = Iterm2Osc1337Health {
            rollout_phase: RolloutPhase::RememberPerName,
            allowlist_size: 7,
            ..Iterm2Osc1337Health::baseline()
        };
        let json = serde_json::to_string(&h).unwrap();
        let parsed: Iterm2Osc1337Health = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, h);
    }

    // ------------------------------------------------------------------------
    // Property-style: 1024-trial multipart sweep
    // ------------------------------------------------------------------------

    #[test]
    fn multipart_random_schedule_sweep_no_invariant_violations() {
        // Property: across 1024 random schedules of
        // chunk-append + finalize attempts, the accumulator
        // never:
        //   - reports Accepted when it shouldn't (over cap)
        //   - mutates state on Denied
        //   - finalizes twice
        let mut rng: u64 = 0xa5a5_dead_beef_cafeu64;
        let xorshift = |s: &mut u64| -> u64 {
            let mut x = *s;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *s = x;
            x
        };

        for trial in 0..1024 {
            let depth_cap = ((xorshift(&mut rng) % 8) + 1) as u32;
            let size_cap = (xorshift(&mut rng) % 1000) + 1;
            let mut acc = MultipartFileAccumulator::with_caps(depth_cap, size_cap);
            for _ in 0..16 {
                let r = xorshift(&mut rng);
                let action = r % 3;
                match action {
                    0 => {
                        let chunk = (xorshift(&mut rng) % 200) + 1;
                        let prior = acc.clone();
                        let outcome = acc.append_chunk(chunk);
                        match outcome {
                            MultipartOutcome::Accepted { .. } => {
                                assert!(acc.bytes_received <= acc.size_cap);
                                assert!(acc.chunks_received <= acc.depth_cap);
                                assert!(!acc.finalized);
                            }
                            MultipartOutcome::Denied { .. } => {
                                // State must be unchanged on denial.
                                assert_eq!(
                                    acc, prior,
                                    "trial {trial}: append_chunk Denied mutated state"
                                );
                            }
                            _ => panic!("append_chunk returned {outcome:?}"),
                        }
                    }
                    1 => {
                        let prior = acc.clone();
                        let outcome = acc.finalize();
                        match outcome {
                            MultipartOutcome::Finalized { .. } => {
                                assert!(acc.finalized);
                            }
                            MultipartOutcome::Denied { .. } => {
                                if matches!(
                                    outcome,
                                    MultipartOutcome::Denied {
                                        reason: MultipartDenialReason::AlreadyFinalized
                                    }
                                ) {
                                    // OK to be already-finalized.
                                } else {
                                    // Other denial paths must
                                    // leave finalized unchanged.
                                    assert_eq!(acc.finalized, prior.finalized);
                                }
                            }
                            _ => panic!("finalize returned {outcome:?}"),
                        }
                    }
                    _ => {
                        // No-op step.
                    }
                }
            }
        }
    }
}
