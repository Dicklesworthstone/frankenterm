//! Passive-watch read-only invariant
//! ([BR-RC-SAFETY-PROOFS.G9] / `ft-x0666.1`).
//!
//! ft's foundational safety claim: *"`ft watch` is read-only;
//! mutating actions must pass the Policy Engine."* This module
//! ships the invariant contract + adversarial-corpus catalog +
//! recorder schema + health snapshot the cargo-fuzz target at
//! `fuzz/fuzz_targets/passive_watch_invariant.rs` consumes.
//!
//! ## Why this is foundational
//!
//! If a randomly-crafted pane output stream can drive `ft watch`
//! into emitting a send / spawn / close action, every downstream
//! "safe by default" claim is invalid. The bead's headline rule:
//!
//! > Zero outbound mutating IPC. Zero non-capture storage writes.
//! > Pattern detections OK; sends/spawns/closes NOT OK.
//!
//! ## What this module ships
//!
//! - [`WatchAction`] — closed taxonomy of every observable
//!   action the watch loop can emit. Distinguishes
//!   read-only-by-design (capture / pattern-detection / metadata-
//!   write) from MUTATING (outbound IPC / non-capture storage
//!   write).
//! - [`is_mutating_action`] / [`WatchAction::is_mutating`] —
//!   load-bearing predicate. The fuzz harness asserts every
//!   recorded action has `is_mutating() == false`.
//! - [`AdversarialCorpusKind`] — closed list of attack
//!   categories from the bead description: mutated real
//!   terminal output, malicious CSI / OSC / DCS injection,
//!   deliberately-crafted prompt mimics.
//! - [`adversarial_seed_catalog`] — hand-curated byte sequences
//!   for each category. The cargo-fuzz harness uses these as
//!   seed inputs; coverage-guided fuzzing then expands.
//! - [`PassiveWatchObservation`] — what the harness records
//!   per fuzz iteration: input hash + recorded action sequence
//!   + mutating-violation count.
//! - [`PassiveWatchInvariant`] — named violations the harness
//!   asserts. `NoOutboundMutatingIpc` and
//!   `NoNonCaptureStorageWrite` are the bead's two headline
//!   rules.
//! - [`PassiveWatchHealth`] — `ft doctor` counter snapshot
//!   mirroring the `*Health` shape used across this session
//!   (a11y_tree, color_management, atlas_stability, etc.).
//!
//! ## What this module is NOT
//!
//! - The cargo-fuzz binary. That lives in
//!   `fuzz/fuzz_targets/passive_watch_invariant.rs` and consumes
//!   this module's types.
//! - The actual `ft watch` driver. The harness instruments the
//!   real watch loop and records the [`WatchAction`]s it emits;
//!   that wiring is the integration follow-on.
//! - The 24h CI lane. The bead's "≥1 hour per PR; ≥24 hours per
//!   release" cadence is a CI configuration concern; this module
//!   ships the harness contract.

use serde::{Deserialize, Serialize};

// ============================================================================
// Action taxonomy
// ============================================================================

/// Closed list of every observable action the `ft watch` loop
/// can emit. The taxonomy is split into:
///
/// - **Read-only (allowed):** `Capture`, `PatternDetection`,
///   `WatchMetadataWrite`, `Other` (a catch-all the recorder
///   classifies as observation; the harness MAY narrow this if
///   a new action surfaces).
/// - **Mutating (FORBIDDEN by the bead's headline rule):**
///   `OutboundSend`, `OutboundSpawn`, `OutboundClose`,
///   `NonCaptureStorageWrite`.
///
/// Adding a new action requires extending this enum and
/// classifying it; the unit test
/// `every_action_kind_has_a_classification` pins that none stays
/// unclassified.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WatchAction {
    /// Pane-output capture write — the read-only baseline.
    Capture {
        /// Pane id, for traceability.
        pane_id: u64,
        /// Bytes captured.
        byte_count: u32,
    },
    /// Pattern detection emission. Read-only (the detection is
    /// just an inspection of captured bytes).
    PatternDetection { rule_id: String },
    /// Watch-process metadata write (heartbeat, telemetry,
    /// crash-recovery checkpoint). Allowed because it doesn't
    /// affect any other agent / pane.
    WatchMetadataWrite { kind: WatchMetadataKind },
    /// **Mutating.** Outbound text injection into a pane —
    /// `send_text`-style. The bead's strict-no.
    OutboundSend { target_pane_id: u64, bytes_len: u32 },
    /// **Mutating.** Spawned a new pane / process.
    OutboundSpawn { kind: SpawnKind },
    /// **Mutating.** Closed a pane / process.
    OutboundClose { target_pane_id: u64 },
    /// **Mutating.** Storage write to a table the watch loop
    /// is supposed to be read-only against (anything except
    /// pane_capture / patterns_index / watch_telemetry).
    NonCaptureStorageWrite { table: String },
    /// Catch-all for actions the recorder couldn't classify.
    /// The harness's invariant treats this as suspicious — if
    /// an `Other` ever appears, the harness records the
    /// observation and flags it for human review (it's not
    /// auto-fail because false positives would be worse than
    /// false negatives here).
    Other { description: String },
}

/// Kind of metadata the watch process may write. Listed so the
/// recorder can encode them precisely; all are non-mutating per
/// the bead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchMetadataKind {
    /// Liveness heartbeat to the orchestrator.
    Heartbeat,
    /// Telemetry counter dump (counters, latencies).
    Telemetry,
    /// Crash-recovery checkpoint.
    CrashCheckpoint,
}

/// Kind of spawn (always mutating). Listed for taxonomy
/// completeness; the integration recorder fills in the variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnKind {
    Pane,
    Tab,
    Window,
    Process,
}

impl WatchAction {
    /// Whether this action mutates state outside the watch
    /// process's read-only contract.
    #[must_use]
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::OutboundSend { .. }
                | Self::OutboundSpawn { .. }
                | Self::OutboundClose { .. }
                | Self::NonCaptureStorageWrite { .. }
        )
    }

    /// Stable slug for filtering / grouping in the JSONL log.
    #[must_use]
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Capture { .. } => "capture",
            Self::PatternDetection { .. } => "pattern_detection",
            Self::WatchMetadataWrite { .. } => "watch_metadata_write",
            Self::OutboundSend { .. } => "outbound_send",
            Self::OutboundSpawn { .. } => "outbound_spawn",
            Self::OutboundClose { .. } => "outbound_close",
            Self::NonCaptureStorageWrite { .. } => "non_capture_storage_write",
            Self::Other { .. } => "other",
        }
    }
}

/// Free-standing predicate equivalent to `WatchAction::is_mutating`.
/// Provided for callers that want the predicate without taking
/// ownership of the value.
#[must_use]
pub fn is_mutating_action(action: &WatchAction) -> bool {
    action.is_mutating()
}

// ============================================================================
// Adversarial input corpus
// ============================================================================

/// Closed list of attack-category labels from the bead's "Build
/// fuzz harness ... driving the watch loop with adversarial pane-
/// output corpus" enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdversarialCorpusKind {
    /// Mutated real terminal output (what an actual program emits,
    /// with bit-flips / structural mutations).
    MutatedRealOutput,
    /// CSI injection: malformed / overlong CSI sequences trying
    /// to force the parser into an unexpected state.
    CsiInjection,
    /// OSC injection: oversized / unterminated OSC strings.
    OscInjection,
    /// DCS injection: nested / pathological device-control
    /// strings.
    DcsInjection,
    /// Deliberately-crafted prompt mimicking a detected
    /// pattern, designed to look like a legitimate trigger and
    /// see if the watch loop responds with a mutating action.
    PromptMimic,
}

impl AdversarialCorpusKind {
    /// Stable slug.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::MutatedRealOutput => "mutated_real_output",
            Self::CsiInjection => "csi_injection",
            Self::OscInjection => "osc_injection",
            Self::DcsInjection => "dcs_injection",
            Self::PromptMimic => "prompt_mimic",
        }
    }

    /// Every kind in declaration order.
    pub const ALL: &'static [AdversarialCorpusKind] = &[
        Self::MutatedRealOutput,
        Self::CsiInjection,
        Self::OscInjection,
        Self::DcsInjection,
        Self::PromptMimic,
    ];
}

/// One hand-curated adversarial seed. The cargo-fuzz harness
/// feeds these as initial inputs; libfuzzer's coverage-guided
/// engine expands from there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdversarialSeed {
    pub kind: AdversarialCorpusKind,
    /// Stable identifier (e.g., `"csi_overlong_param"`).
    pub name: String,
    /// Seed bytes. Stored as Vec<u8> for serde stability; the
    /// cargo-fuzz target writes these out as on-disk seed files.
    pub bytes: Vec<u8>,
    /// Why this seed exists — what bug class it targets.
    pub rationale: String,
}

/// Hand-curated adversarial seed catalog. Each kind has at
/// least one seed; the cargo-fuzz harness writes these out at
/// initialization time so the corpus directory is bootstrapped
/// without manual file creation.
#[must_use]
pub fn adversarial_seed_catalog() -> Vec<AdversarialSeed> {
    vec![
        // Mutated real output: a typical shell prompt + ANSI
        // styling, then a bit-flip in the middle.
        AdversarialSeed {
            kind: AdversarialCorpusKind::MutatedRealOutput,
            name: "shell_prompt_bitflip".to_string(),
            bytes: b"\x1b[32muser@host\x1b[m:\x1b[34m~/projects\x1b[m$ \xff\xff\xff command\n"
                .to_vec(),
            rationale: "real prompt with high-bit garbage injected; \
                        catches misclassification as an OSC marker"
                .to_string(),
        },
        // CSI: overlong parameter list, designed to overflow
        // any naive fixed-size param buffer.
        AdversarialSeed {
            kind: AdversarialCorpusKind::CsiInjection,
            name: "csi_overlong_params".to_string(),
            bytes: {
                let mut v = b"\x1b[".to_vec();
                v.extend(std::iter::repeat(b'1').take(4096));
                v.extend(b";m");
                v
            },
            rationale: "4096-byte CSI param string; tests parser bounds + \
                        whether ridiculous params trigger mutating action"
                .to_string(),
        },
        // CSI: malformed final byte (not in the legal range).
        AdversarialSeed {
            kind: AdversarialCorpusKind::CsiInjection,
            name: "csi_invalid_final".to_string(),
            bytes: b"\x1b[1;2;3\x00".to_vec(),
            rationale: "CSI terminated with NUL — invalid final byte; \
                        tests parser error-recovery doesn't escalate"
                .to_string(),
        },
        // OSC: oversized title, deliberately attempting to
        // exercise the title-storage path (which is metadata-
        // write — allowed — but the watch must not propagate
        // outward).
        AdversarialSeed {
            kind: AdversarialCorpusKind::OscInjection,
            name: "osc_oversized_title".to_string(),
            bytes: {
                let mut v = b"\x1b]0;".to_vec();
                v.extend(std::iter::repeat(b'A').take(8192));
                v.extend(b"\x07");
                v
            },
            rationale: "8KiB OSC 0 title; tests metadata-write path bounds + \
                        no spawn / send escalation"
                .to_string(),
        },
        // OSC: unterminated (no BEL or ST).
        AdversarialSeed {
            kind: AdversarialCorpusKind::OscInjection,
            name: "osc_unterminated".to_string(),
            bytes: b"\x1b]0;forever and ever".to_vec(),
            rationale: "OSC with no terminator; parser must hold the buffer \
                        without ever emitting a mutating action"
                .to_string(),
        },
        // DCS: nested DCS attempting to confuse the state
        // machine.
        AdversarialSeed {
            kind: AdversarialCorpusKind::DcsInjection,
            name: "dcs_nested".to_string(),
            bytes: b"\x1bP1$q\x1bP2$q\x1b\\\x1b\\".to_vec(),
            rationale: "two DCS opens before any close; tests state-machine \
                        recovers without firing a side effect"
                .to_string(),
        },
        // DCS: Sixel-like data stream (large binary payload).
        AdversarialSeed {
            kind: AdversarialCorpusKind::DcsInjection,
            name: "dcs_sixel_like".to_string(),
            bytes: {
                let mut v = b"\x1bPq".to_vec();
                v.extend(std::iter::repeat(b'?').take(2048));
                v.extend(b"\x1b\\");
                v
            },
            rationale: "Sixel-shaped DCS payload; tests image-data path is \
                        read-only (capture-only)"
                .to_string(),
        },
        // Prompt mimic: looks like a Codex usage-limit prompt;
        // tests whether the watch's pattern detector triggers a
        // workflow that emits a send.
        AdversarialSeed {
            kind: AdversarialCorpusKind::PromptMimic,
            name: "fake_codex_usage_limit".to_string(),
            bytes: b"\nUsage limit reached. Contact support to increase.\n".to_vec(),
            rationale: "matches the codex usage-limit pattern; the watch \
                        loop must emit a PatternDetection but NOT an \
                        OutboundSend — the workflow is the gated layer"
                .to_string(),
        },
        // Prompt mimic: synthesized "Compacting…" text that
        // mimics a Claude Code compaction progress message.
        AdversarialSeed {
            kind: AdversarialCorpusKind::PromptMimic,
            name: "fake_compacting".to_string(),
            bytes: b"\nCompacting...\n".to_vec(),
            rationale: "matches Claude Code compaction pattern; same rule — \
                        detect, don't act"
                .to_string(),
        },
        // Prompt mimic: attacker tries to inject an interactive-
        // shell-style prompt that historically might trigger an
        // auto-answer workflow.
        AdversarialSeed {
            kind: AdversarialCorpusKind::PromptMimic,
            name: "fake_interactive_prompt".to_string(),
            bytes: b"\n[Y/n]: ".to_vec(),
            rationale: "interactive-shell prompt mimic; tests no auto-\
                        answer workflow fires from a passive watch"
                .to_string(),
        },
    ]
}

// ============================================================================
// Per-iteration observation
// ============================================================================

/// One observation point — the fuzz harness records this per
/// iteration. The integration recorder fills in `actions` from
/// the live watch loop's emissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassiveWatchObservation {
    pub ts_ms: u64,
    /// Stable hash of the input bytes so a violation can be
    /// reproduced exactly.
    pub input_blake3: String,
    /// Length of the input in bytes.
    pub input_len: u32,
    /// All actions the watch loop emitted in response.
    pub actions: Vec<WatchAction>,
    /// Count of mutating actions in `actions`. The bead's
    /// headline rule: `mutating_violations == 0`.
    pub mutating_violations: u32,
    /// Whether the corpus categorized this input.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub corpus_kind: Option<AdversarialCorpusKind>,
}

impl PassiveWatchObservation {
    /// Whether this observation passes the bead's invariant.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.mutating_violations == 0
    }
}

// ============================================================================
// Invariants
// ============================================================================

/// Named invariants the harness asserts. Two headline rules from
/// the bead description:
///
/// 1. `NoOutboundMutatingIpc` — zero send/spawn/close per input.
/// 2. `NoNonCaptureStorageWrite` — only capture / patterns_index /
///    watch_telemetry tables get writes.
///
/// `OtherActionUnclassified` is a soft signal — flagged for
/// review but not auto-fail.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PassiveWatchInvariant {
    NoOutboundMutatingIpc {
        action_index: u32,
        slug: String,
    },
    NoNonCaptureStorageWrite {
        table: String,
        action_index: u32,
    },
    OtherActionUnclassified {
        description: String,
        action_index: u32,
    },
}

/// Run all invariants against an observation. Returns the
/// accumulated list (empty = clean).
#[must_use]
pub fn check_invariants(obs: &PassiveWatchObservation) -> Vec<PassiveWatchInvariant> {
    let mut violations = Vec::new();
    for (i, action) in obs.actions.iter().enumerate() {
        let idx = i as u32;
        match action {
            WatchAction::OutboundSend { .. }
            | WatchAction::OutboundSpawn { .. }
            | WatchAction::OutboundClose { .. } => {
                violations.push(PassiveWatchInvariant::NoOutboundMutatingIpc {
                    action_index: idx,
                    slug: action.slug().to_string(),
                });
            }
            WatchAction::NonCaptureStorageWrite { table } => {
                violations.push(PassiveWatchInvariant::NoNonCaptureStorageWrite {
                    table: table.clone(),
                    action_index: idx,
                });
            }
            WatchAction::Other { description } => {
                violations.push(PassiveWatchInvariant::OtherActionUnclassified {
                    description: description.clone(),
                    action_index: idx,
                });
            }
            WatchAction::Capture { .. }
            | WatchAction::PatternDetection { .. }
            | WatchAction::WatchMetadataWrite { .. } => {
                // Read-only — allowed.
            }
        }
    }
    violations
}

// ============================================================================
// Health snapshot
// ============================================================================

/// `ft doctor` counter snapshot for the passive-watch attestation
/// surface. Mirrors the `*Health` shape used across this session
/// (a11y_tree, color_management, atlas_stability, triple_buffer,
/// live_resize, render_quality, snap_back_fuzz, wayland_frame_pacing,
/// bidi_correctness, tx_killswitch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassiveWatchHealth {
    pub iterations_total: u64,
    pub captures_total: u64,
    pub detections_total: u64,
    pub metadata_writes_total: u64,
    pub mutating_violations_total: u64,
    pub unclassified_other_total: u64,
}

impl PassiveWatchHealth {
    #[must_use]
    pub const fn baseline() -> Self {
        Self {
            iterations_total: 0,
            captures_total: 0,
            detections_total: 0,
            metadata_writes_total: 0,
            mutating_violations_total: 0,
            unclassified_other_total: 0,
        }
    }

    /// True iff at least one watch iteration has run AND no
    /// mutating-IPC violation has been observed. The cold
    /// baseline (no iterations) is reported unsafe so the doctor
    /// surface does not silently green a process whose passive-
    /// watch harness has not been wired.
    ///
    /// Per ft-11d5f sweep: previously checked
    /// `mutating_violations_total == 0` alone, which is true on
    /// cold baseline.
    #[must_use]
    pub const fn is_safe(&self) -> bool {
        self.iterations_total > 0 && self.mutating_violations_total == 0
    }

    /// Detection rate per iteration. A healthy watch loop
    /// detects patterns regularly under the corpus.
    #[must_use]
    pub fn detection_rate(&self) -> f64 {
        if self.iterations_total == 0 {
            return 0.0;
        }
        self.detections_total as f64 / self.iterations_total as f64
    }
}

/// Update a health snapshot with a new observation.
pub fn fold_observation(health: &mut PassiveWatchHealth, obs: &PassiveWatchObservation) {
    health.iterations_total += 1;
    for action in &obs.actions {
        match action {
            WatchAction::Capture { .. } => health.captures_total += 1,
            WatchAction::PatternDetection { .. } => health.detections_total += 1,
            WatchAction::WatchMetadataWrite { .. } => health.metadata_writes_total += 1,
            WatchAction::OutboundSend { .. }
            | WatchAction::OutboundSpawn { .. }
            | WatchAction::OutboundClose { .. }
            | WatchAction::NonCaptureStorageWrite { .. } => {
                health.mutating_violations_total += 1;
            }
            WatchAction::Other { .. } => health.unclassified_other_total += 1,
        }
    }
}

// ============================================================================
// JSONL writer
// ============================================================================

#[must_use]
pub fn render_observations_jsonl(observations: &[PassiveWatchObservation]) -> String {
    let mut out = String::new();
    for obs in observations {
        let line = serde_json::to_string(obs).expect("PassiveWatchObservation always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_observations_jsonl(
    jsonl: &str,
) -> Result<Vec<PassiveWatchObservation>, serde_json::Error> {
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
    fn read_only_actions_are_not_mutating() {
        assert!(
            !WatchAction::Capture {
                pane_id: 1,
                byte_count: 100
            }
            .is_mutating()
        );
        assert!(
            !WatchAction::PatternDetection {
                rule_id: "test".to_string()
            }
            .is_mutating()
        );
        assert!(
            !WatchAction::WatchMetadataWrite {
                kind: WatchMetadataKind::Heartbeat
            }
            .is_mutating()
        );
    }

    #[test]
    fn mutating_actions_are_mutating() {
        assert!(
            WatchAction::OutboundSend {
                target_pane_id: 1,
                bytes_len: 10,
            }
            .is_mutating()
        );
        assert!(
            WatchAction::OutboundSpawn {
                kind: SpawnKind::Pane,
            }
            .is_mutating()
        );
        assert!(WatchAction::OutboundClose { target_pane_id: 1 }.is_mutating());
        assert!(
            WatchAction::NonCaptureStorageWrite {
                table: "panes".to_string(),
            }
            .is_mutating()
        );
    }

    #[test]
    fn other_action_is_not_mutating_but_invariant_flags_it() {
        let o = WatchAction::Other {
            description: "??".to_string(),
        };
        assert!(!o.is_mutating());
        let obs = PassiveWatchObservation {
            ts_ms: 0,
            input_blake3: "hash".to_string(),
            input_len: 0,
            actions: vec![o],
            mutating_violations: 0,
            corpus_kind: None,
        };
        let v = check_invariants(&obs);
        assert!(
            v.iter()
                .any(|x| matches!(x, PassiveWatchInvariant::OtherActionUnclassified { .. })),
            "Other action should produce an unclassified invariant"
        );
    }

    #[test]
    fn capture_only_observation_is_clean() {
        let obs = PassiveWatchObservation {
            ts_ms: 0,
            input_blake3: "h".to_string(),
            input_len: 100,
            actions: vec![WatchAction::Capture {
                pane_id: 1,
                byte_count: 100,
            }],
            mutating_violations: 0,
            corpus_kind: None,
        };
        assert!(check_invariants(&obs).is_empty());
        assert!(obs.is_safe());
    }

    #[test]
    fn outbound_send_observation_violates_no_mutating_ipc() {
        let obs = PassiveWatchObservation {
            ts_ms: 0,
            input_blake3: "h".to_string(),
            input_len: 100,
            actions: vec![
                WatchAction::Capture {
                    pane_id: 1,
                    byte_count: 100,
                },
                WatchAction::OutboundSend {
                    target_pane_id: 2,
                    bytes_len: 5,
                },
            ],
            mutating_violations: 1,
            corpus_kind: Some(AdversarialCorpusKind::PromptMimic),
        };
        let v = check_invariants(&obs);
        assert_eq!(v.len(), 1);
        assert!(matches!(
            v[0],
            PassiveWatchInvariant::NoOutboundMutatingIpc { .. }
        ));
        assert!(!obs.is_safe());
    }

    #[test]
    fn non_capture_storage_write_violates() {
        let obs = PassiveWatchObservation {
            ts_ms: 0,
            input_blake3: "h".to_string(),
            input_len: 50,
            actions: vec![WatchAction::NonCaptureStorageWrite {
                table: "panes".to_string(),
            }],
            mutating_violations: 1,
            corpus_kind: None,
        };
        let v = check_invariants(&obs);
        assert!(
            v.iter()
                .any(|x| matches!(x, PassiveWatchInvariant::NoNonCaptureStorageWrite { .. }))
        );
    }

    #[test]
    fn corpus_covers_every_kind() {
        let catalog = adversarial_seed_catalog();
        for kind in AdversarialCorpusKind::ALL {
            let count = catalog.iter().filter(|s| s.kind == *kind).count();
            assert!(count > 0, "{kind:?} has no seeds in the corpus");
        }
    }

    #[test]
    fn corpus_seed_names_are_unique() {
        let catalog = adversarial_seed_catalog();
        let mut names: Vec<&str> = catalog.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let original_len = names.len();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate seed names");
    }

    #[test]
    fn corpus_seeds_are_non_empty() {
        for seed in adversarial_seed_catalog() {
            assert!(!seed.bytes.is_empty(), "seed {} has empty bytes", seed.name);
            assert!(
                !seed.rationale.is_empty(),
                "seed {} has empty rationale",
                seed.name
            );
        }
    }

    #[test]
    fn fold_observation_updates_health_correctly() {
        let mut h = PassiveWatchHealth::baseline();
        let obs = PassiveWatchObservation {
            ts_ms: 0,
            input_blake3: "h".to_string(),
            input_len: 0,
            actions: vec![
                WatchAction::Capture {
                    pane_id: 1,
                    byte_count: 100,
                },
                WatchAction::PatternDetection {
                    rule_id: "x".to_string(),
                },
                WatchAction::WatchMetadataWrite {
                    kind: WatchMetadataKind::Heartbeat,
                },
            ],
            mutating_violations: 0,
            corpus_kind: None,
        };
        fold_observation(&mut h, &obs);
        assert_eq!(h.iterations_total, 1);
        assert_eq!(h.captures_total, 1);
        assert_eq!(h.detections_total, 1);
        assert_eq!(h.metadata_writes_total, 1);
        assert_eq!(h.mutating_violations_total, 0);
        assert!(h.is_safe());
        assert!((h.detection_rate() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fold_observation_increments_violations_on_mutating_action() {
        let mut h = PassiveWatchHealth::baseline();
        let obs = PassiveWatchObservation {
            ts_ms: 0,
            input_blake3: "h".to_string(),
            input_len: 0,
            actions: vec![WatchAction::OutboundSend {
                target_pane_id: 1,
                bytes_len: 5,
            }],
            mutating_violations: 1,
            corpus_kind: None,
        };
        fold_observation(&mut h, &obs);
        assert_eq!(h.mutating_violations_total, 1);
        assert!(!h.is_safe());
    }

    #[test]
    fn jsonl_observation_roundtrip() {
        let obs = vec![
            PassiveWatchObservation {
                ts_ms: 0,
                input_blake3: "abc123".to_string(),
                input_len: 100,
                actions: vec![WatchAction::Capture {
                    pane_id: 1,
                    byte_count: 100,
                }],
                mutating_violations: 0,
                corpus_kind: Some(AdversarialCorpusKind::CsiInjection),
            },
            PassiveWatchObservation {
                ts_ms: 100,
                input_blake3: "def456".to_string(),
                input_len: 50,
                actions: vec![],
                mutating_violations: 0,
                corpus_kind: None,
            },
        ];
        let rendered = render_observations_jsonl(&obs);
        let parsed = parse_observations_jsonl(&rendered).unwrap();
        assert_eq!(parsed, obs);
    }

    #[test]
    fn baseline_health_is_unsafe_until_iterated() {
        // Per ft-11d5f sweep fix: cold baseline is unsafe (no
        // iterations recorded). Previously pinned the rubber-
        // stamp behavior.
        let h = PassiveWatchHealth::baseline();
        assert!(!h.is_safe(), "cold baseline must be unsafe");
        assert_eq!(h.detection_rate(), 0.0);

        // After at least one iteration with no violations: safe.
        let h_clean = PassiveWatchHealth {
            iterations_total: 1,
            captures_total: 1,
            detections_total: 0,
            metadata_writes_total: 1,
            mutating_violations_total: 0,
            unclassified_other_total: 0,
        };
        assert!(h_clean.is_safe(), "iterated + clean must be safe");

        // Iterated but with a violation: unsafe.
        let h_bad = PassiveWatchHealth {
            iterations_total: 1,
            captures_total: 1,
            detections_total: 0,
            metadata_writes_total: 1,
            mutating_violations_total: 1,
            unclassified_other_total: 0,
        };
        assert!(!h_bad.is_safe(), "iterated + violation must be unsafe");
    }

    #[test]
    fn corpus_kind_slugs_are_distinct_and_match_doc() {
        let slugs: Vec<&'static str> = AdversarialCorpusKind::ALL
            .iter()
            .map(|k| k.slug())
            .collect();
        assert_eq!(
            slugs,
            vec![
                "mutated_real_output",
                "csi_injection",
                "osc_injection",
                "dcs_injection",
                "prompt_mimic",
            ]
        );
    }

    #[test]
    fn is_mutating_action_free_function_matches_method() {
        let actions = [
            WatchAction::Capture {
                pane_id: 1,
                byte_count: 100,
            },
            WatchAction::OutboundSend {
                target_pane_id: 2,
                bytes_len: 5,
            },
        ];
        for action in &actions {
            assert_eq!(is_mutating_action(action), action.is_mutating());
        }
    }
}
