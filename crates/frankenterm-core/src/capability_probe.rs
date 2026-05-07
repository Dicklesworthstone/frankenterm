//! Capability probes — the producer side of the passport pipeline
//! (ft-1650n.1 slice 7 / ft-6b7i0).
//!
//! Slice 6 ships a way to issue Declared-only passports at pane-
//! registration time; slices 3-5 ship gates that reject anything
//! short of Verified. The bridge between them is THIS module: a
//! [`CapabilityProbe`] trait + [`ProbeRunner`] that takes a
//! Declared-only passport and produces a Verified-when-evidence-
//! matches passport.
//!
//! # Fail-closed contract
//!
//! Per ft-6b7i0 acceptance: every probe MUST honor a deadline and
//! MUST fail-closed on timeout / error / indeterminate result.
//! Failure / timeout MUST NOT promote a class to Verified — the
//! pre-existing verification state is preserved (Declared stays
//! Declared, Unknown stays Unknown). The runner only promotes
//! entries on an explicit `ProbeOutcome::Verified` result.
//!
//! # Scope of slice 7
//!
//! This slice ships pure-function probes that take pre-collected
//! evidence as input and check predicates synchronously — no real
//! I/O. Concrete examples:
//!
//! - [`ToolAvailabilityProbe`] checks whether a tool name appears
//!   in a pre-collected list of available tools.
//! - [`FilesystemScopeProbe`] checks whether a pre-collected `cwd`
//!   is a child of the declared scope.
//!
//! Probes that DO real I/O (e.g. actually running `which bash` or
//! reading network namespace state) follow the same trait but
//! belong in a follow-up slice — separate concern from the data
//! layer this slice ships.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::capability_passport::{
    CapabilityClass, CapabilityEntry, CapabilityPassport, CapabilityVerification, RedactedProof,
};

/// Outcome of a single probe invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeOutcome {
    /// Probe confirmed the capability. The carried [`RedactedProof`]
    /// is plumbed back into the passport entry's `proof` field so
    /// operators can correlate verifications across passports
    /// without exposing the underlying evidence.
    Verified(RedactedProof),
    /// Probe ran to completion but the capability is NOT confirmed
    /// (e.g. the requested tool is not in the available-tools list).
    /// The carried `reason` string is short and operator-readable.
    Failed { reason: String },
    /// Probe did not complete within the allotted deadline. Slice 7's
    /// pure-function probes never produce this outcome (they
    /// terminate synchronously); future I/O-doing probes will.
    TimedOut,
}

impl ProbeOutcome {
    /// Stable label for telemetry / display.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Verified(_) => "verified",
            Self::Failed { .. } => "failed",
            Self::TimedOut => "timed_out",
        }
    }

    /// True iff the outcome promotes the underlying capability to
    /// [`CapabilityVerification::Verified`].
    #[must_use]
    pub fn promotes_to_verified(&self) -> bool {
        matches!(self, Self::Verified(_))
    }
}

/// A capability probe knows which class of capability it tests and
/// produces a [`ProbeOutcome`] when invoked. Implementations MUST
/// honor `deadline` — return [`ProbeOutcome::TimedOut`] without
/// blocking past the deadline.
pub trait CapabilityProbe: Send + Sync {
    /// The capability class this probe is responsible for. The
    /// runner uses this to find matching entries in a passport.
    fn class(&self) -> CapabilityClass;

    /// Run the probe. Implementations MUST NOT block past
    /// `deadline`. Pure-function probes (no I/O) ignore the
    /// deadline parameter and return synchronously.
    fn probe(&self, deadline: Instant) -> ProbeOutcome;
}

/// Pure-function probe: checks whether `requested_tool` appears in
/// a pre-collected list of `available_tools`. Operators populate
/// `available_tools` from a side-channel (agent metadata, manifest,
/// etc.); the probe is a deterministic predicate over that input.
///
/// Slice 7 deliberately keeps this synchronous — running the actual
/// `which $tool` shell command is a separate I/O probe shipped in a
/// follow-up slice.
#[derive(Debug, Clone)]
pub struct ToolAvailabilityProbe {
    requested_tool: String,
    available_tools: Vec<String>,
}

impl ToolAvailabilityProbe {
    #[must_use]
    pub fn new(requested_tool: impl Into<String>, available_tools: Vec<String>) -> Self {
        Self {
            requested_tool: requested_tool.into(),
            available_tools,
        }
    }
}

impl CapabilityProbe for ToolAvailabilityProbe {
    fn class(&self) -> CapabilityClass {
        CapabilityClass::ToolAvailability(self.requested_tool.clone())
    }

    fn probe(&self, _deadline: Instant) -> ProbeOutcome {
        if self
            .available_tools
            .iter()
            .any(|t| t == &self.requested_tool)
        {
            // The tool name is the proof — but we hash it through
            // RedactedProof so the digest format matches every other
            // verification in the passport.
            ProbeOutcome::Verified(RedactedProof::from_value(self.requested_tool.as_bytes()))
        } else {
            ProbeOutcome::Failed {
                reason: format!(
                    "tool {} not in available list ({} entries)",
                    self.requested_tool,
                    self.available_tools.len()
                ),
            }
        }
    }
}

/// Pure-function probe: checks whether `actual_cwd` is `declared_scope`
/// or a descendant. Strict prefix-of-canonical-path semantics — does
/// NOT follow symlinks (intentional: a sym-pointed-elsewhere cwd
/// should NOT count as "inside" the declared scope).
#[derive(Debug, Clone)]
pub struct FilesystemScopeProbe {
    declared_scope: PathBuf,
    actual_cwd: PathBuf,
}

impl FilesystemScopeProbe {
    #[must_use]
    pub fn new(declared_scope: impl Into<PathBuf>, actual_cwd: impl Into<PathBuf>) -> Self {
        Self {
            declared_scope: declared_scope.into(),
            actual_cwd: actual_cwd.into(),
        }
    }
}

impl CapabilityProbe for FilesystemScopeProbe {
    fn class(&self) -> CapabilityClass {
        CapabilityClass::FilesystemScope(self.declared_scope.display().to_string())
    }

    fn probe(&self, _deadline: Instant) -> ProbeOutcome {
        if path_is_within(&self.actual_cwd, &self.declared_scope) {
            // Proof is the cwd path bytes — operators can correlate
            // probe verifications across panes that share a cwd.
            let bytes = self.actual_cwd.as_os_str().as_encoded_bytes();
            ProbeOutcome::Verified(RedactedProof::from_value(bytes))
        } else {
            ProbeOutcome::Failed {
                reason: format!(
                    "actual cwd {} is not within declared scope {}",
                    self.actual_cwd.display(),
                    self.declared_scope.display()
                ),
            }
        }
    }
}

/// Strict path-prefix check: `child` is the same as `parent` or a
/// descendant. Component-wise comparison so `/foo/bar` is NOT inside
/// `/foo/ba` (string-prefix would say yes; we say no).
fn path_is_within(child: &Path, parent: &Path) -> bool {
    let mut child_iter = child.components();
    for parent_component in parent.components() {
        match child_iter.next() {
            Some(c) if c == parent_component => {}
            _ => return false,
        }
    }
    true
}

/// Promotes Declared / Unknown entries to Verified by running each
/// matching probe. Per the fail-closed contract, an existing
/// Verified entry is NEVER demoted by a non-Verified probe outcome —
/// promotion is monotonic in the verification rank.
#[derive(Default)]
pub struct ProbeRunner {
    probes: Vec<Box<dyn CapabilityProbe>>,
}

impl std::fmt::Debug for ProbeRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeRunner")
            .field("probe_count", &self.probes.len())
            .finish()
    }
}

impl ProbeRunner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a probe to the runner.
    pub fn add_probe<P: CapabilityProbe + 'static>(&mut self, probe: P) -> &mut Self {
        self.probes.push(Box::new(probe));
        self
    }

    /// Number of probes registered.
    #[must_use]
    pub fn probe_count(&self) -> usize {
        self.probes.len()
    }

    /// Run every probe against `passport`, mutating entries in place.
    /// Returns a [`ProbeRunReport`] summarizing what changed.
    ///
    /// `deadline_per_probe` bounds each individual probe; the runner
    /// itself does not enforce a global deadline (that's the
    /// caller's responsibility — wrap the call in a select/timeout).
    pub fn run(
        &self,
        passport: &mut CapabilityPassport,
        deadline_per_probe: Duration,
    ) -> ProbeRunReport {
        let mut report = ProbeRunReport::default();
        let now_ms = passport.signed_at_ms;
        for probe in &self.probes {
            let class = probe.class();
            let deadline = Instant::now() + deadline_per_probe;
            let outcome = probe.probe(deadline);
            let outcome_label = outcome.label();
            // ft-11wl6: per-probe outcome trace at debug! level
            // (high-volume on large probe sets — operators turn on
            // selectively for tuning).
            debug!(
                target: "ft.probe",
                agent_id = passport.agent_id.as_str(),
                pane_id = passport.pane_id,
                class = class.label().as_str(),
                outcome = outcome_label,
                "capability probe ran",
            );
            report.invocations += 1;
            match outcome {
                ProbeOutcome::Verified(proof) => {
                    let promoted = promote_class_to_verified(passport, &class, proof, now_ms);
                    if promoted {
                        report.verified += 1;
                    } else {
                        report.no_op += 1;
                    }
                }
                ProbeOutcome::Failed { .. } => {
                    report.failed += 1;
                }
                ProbeOutcome::TimedOut => {
                    report.timed_out += 1;
                }
            }
            report.per_class.push((class, outcome_label.to_string()));
        }
        report
    }
}

/// Promote (or insert) a passport entry for `class` to Verified with
/// the supplied `proof`. Returns true iff a promotion actually
/// happened (existing-Verified or no matching entry → false).
fn promote_class_to_verified(
    passport: &mut CapabilityPassport,
    class: &CapabilityClass,
    proof: RedactedProof,
    now_ms: u64,
) -> bool {
    if let Some(entry) = passport.capabilities.iter_mut().find(|e| &e.class == class) {
        if entry.verification == CapabilityVerification::Verified {
            // Already verified — fail-closed monotonic: do not
            // overwrite proof with a fresh probe's. Operators
            // expecting determinism around the verification timestamp
            // can re-issue the passport at a higher generation.
            false
        } else {
            entry.verification = CapabilityVerification::Verified;
            entry.last_observed_at_ms = Some(now_ms);
            entry.proof = proof;
            true
        }
    } else {
        // No existing entry for this class — insert a fresh Verified
        // entry. Distinguishes "probe verified an unannounced
        // capability" from "probe matched a Declared claim" — both
        // land at Verified, but operators can audit insertion order
        // by examining last_observed_at_ms.
        passport.capabilities.push(CapabilityEntry {
            class: class.clone(),
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(now_ms),
            proof,
        });
        true
    }
}

/// Summary of a [`ProbeRunner::run`] invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProbeRunReport {
    pub invocations: usize,
    pub verified: usize,
    pub failed: usize,
    pub timed_out: usize,
    pub no_op: usize,
    pub per_class: Vec<(CapabilityClass, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability_passport::{
        CapabilityClass as Cap, CapabilityPassport, CapabilityVerification,
    };

    fn passport_with(capabilities: Vec<CapabilityEntry>) -> CapabilityPassport {
        CapabilityPassport {
            agent_id: "cc1".into(),
            pane_id: Some(1),
            capabilities,
            generation: 1,
            signed_at_ms: 1_000,
        }
    }

    fn declared_entry(class: Cap) -> CapabilityEntry {
        CapabilityEntry {
            class,
            verification: CapabilityVerification::Declared,
            last_observed_at_ms: Some(900),
            proof: RedactedProof::empty(),
        }
    }

    fn deadline_in(ms: u64) -> Instant {
        Instant::now() + Duration::from_millis(ms)
    }

    // ── ToolAvailabilityProbe ────────────────────────────────────────────

    #[test]
    fn tool_probe_verifies_when_tool_in_available_list() {
        let probe = ToolAvailabilityProbe::new("bash", vec!["bash".into(), "rg".into()]);
        let outcome = probe.probe(deadline_in(1000));
        assert!(matches!(outcome, ProbeOutcome::Verified(_)));
        assert_eq!(outcome.label(), "verified");
    }

    #[test]
    fn tool_probe_fails_when_tool_not_in_list() {
        let probe = ToolAvailabilityProbe::new("nethack", vec!["bash".into(), "rg".into()]);
        let outcome = probe.probe(deadline_in(1000));
        let ProbeOutcome::Failed { reason } = outcome else {
            panic!("expected Failed");
        };
        assert!(reason.contains("nethack"));
    }

    #[test]
    fn tool_probe_class_matches_requested_tool() {
        let probe = ToolAvailabilityProbe::new("write", vec![]);
        assert_eq!(probe.class(), Cap::ToolAvailability("write".into()));
    }

    // ── FilesystemScopeProbe ─────────────────────────────────────────────

    #[test]
    fn fs_scope_probe_verifies_when_cwd_is_inside() {
        let probe = FilesystemScopeProbe::new(
            "/Users/jemanuel/projects",
            "/Users/jemanuel/projects/frankenterm",
        );
        assert!(matches!(
            probe.probe(deadline_in(100)),
            ProbeOutcome::Verified(_)
        ));
    }

    #[test]
    fn fs_scope_probe_verifies_when_cwd_equals_scope() {
        let probe = FilesystemScopeProbe::new("/Users/jemanuel", "/Users/jemanuel");
        assert!(matches!(
            probe.probe(deadline_in(100)),
            ProbeOutcome::Verified(_)
        ));
    }

    #[test]
    fn fs_scope_probe_fails_when_cwd_is_sibling() {
        let probe =
            FilesystemScopeProbe::new("/Users/jemanuel/projects", "/Users/jemanuel/Downloads");
        assert!(matches!(
            probe.probe(deadline_in(100)),
            ProbeOutcome::Failed { .. }
        ));
    }

    #[test]
    fn fs_scope_probe_rejects_string_prefix_collision() {
        // /foo/bar is NOT inside /foo/ba even though string-prefix
        // would say yes — component-wise comparison required.
        let probe = FilesystemScopeProbe::new("/foo/ba", "/foo/bar");
        assert!(matches!(
            probe.probe(deadline_in(100)),
            ProbeOutcome::Failed { .. }
        ));
    }

    #[test]
    fn fs_scope_probe_class_carries_declared_scope_string() {
        let probe = FilesystemScopeProbe::new("/srv/scope", "/srv/scope/sub");
        assert_eq!(probe.class(), Cap::FilesystemScope("/srv/scope".into()));
    }

    // ── ProbeRunner promotion semantics ──────────────────────────────────

    #[test]
    fn runner_promotes_declared_to_verified_on_match() {
        let mut passport =
            passport_with(vec![declared_entry(Cap::ToolAvailability("bash".into()))]);
        let mut runner = ProbeRunner::new();
        runner.add_probe(ToolAvailabilityProbe::new("bash", vec!["bash".into()]));
        let report = runner.run(&mut passport, Duration::from_millis(100));
        assert_eq!(report.invocations, 1);
        assert_eq!(report.verified, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(
            passport.capabilities[0].verification,
            CapabilityVerification::Verified
        );
    }

    #[test]
    fn runner_does_not_demote_existing_verified_entry() {
        let mut passport = passport_with(vec![CapabilityEntry {
            class: Cap::ToolAvailability("bash".into()),
            verification: CapabilityVerification::Verified,
            last_observed_at_ms: Some(500),
            proof: RedactedProof::from_value(b"prior-proof"),
        }]);
        let original_proof = passport.capabilities[0].proof.clone();
        let original_observed_at = passport.capabilities[0].last_observed_at_ms;

        let mut runner = ProbeRunner::new();
        runner.add_probe(ToolAvailabilityProbe::new("bash", vec!["bash".into()]));
        let report = runner.run(&mut passport, Duration::from_millis(100));

        // Probe ran and matched, but entry was already Verified —
        // fail-closed monotonic semantics preserve the prior proof
        // and timestamp; no_op = 1, verified = 0.
        assert_eq!(report.invocations, 1);
        assert_eq!(report.verified, 0);
        assert_eq!(report.no_op, 1);
        assert_eq!(
            passport.capabilities[0].verification,
            CapabilityVerification::Verified
        );
        assert_eq!(passport.capabilities[0].proof, original_proof);
        assert_eq!(
            passport.capabilities[0].last_observed_at_ms,
            original_observed_at
        );
    }

    #[test]
    fn runner_failed_outcome_preserves_existing_declared_state() {
        let mut passport =
            passport_with(vec![declared_entry(Cap::ToolAvailability("bash".into()))]);
        let mut runner = ProbeRunner::new();
        // Probe targets bash but bash NOT in available tools — Failed.
        runner.add_probe(ToolAvailabilityProbe::new("bash", vec!["rg".into()]));
        let report = runner.run(&mut passport, Duration::from_millis(100));
        assert_eq!(report.invocations, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.verified, 0);
        // Fail-closed: Declared is preserved, NOT downgraded to Unknown.
        assert_eq!(
            passport.capabilities[0].verification,
            CapabilityVerification::Declared
        );
    }

    #[test]
    fn runner_inserts_new_entry_when_probe_class_not_in_passport() {
        // Probe verifies a class the passport hasn't declared. Slice
        // 7 chooses to INSERT a fresh Verified entry — operators
        // correlate insertion order via last_observed_at_ms. This
        // distinguishes "probe verified an unannounced capability"
        // from "probe matched a Declared claim" without rejecting
        // the new evidence.
        let mut passport = passport_with(vec![]);
        let mut runner = ProbeRunner::new();
        runner.add_probe(ToolAvailabilityProbe::new("bash", vec!["bash".into()]));
        let report = runner.run(&mut passport, Duration::from_millis(100));
        assert_eq!(report.invocations, 1);
        assert_eq!(report.verified, 1);
        assert_eq!(passport.capabilities.len(), 1);
        assert_eq!(
            passport.capabilities[0].class,
            Cap::ToolAvailability("bash".into())
        );
        assert_eq!(
            passport.capabilities[0].verification,
            CapabilityVerification::Verified
        );
    }

    #[test]
    fn runner_combines_multiple_probes_into_one_report() {
        let mut passport = passport_with(vec![
            declared_entry(Cap::ToolAvailability("bash".into())),
            declared_entry(Cap::FilesystemScope("/srv/scope".into())),
        ]);
        let mut runner = ProbeRunner::new();
        runner
            .add_probe(ToolAvailabilityProbe::new("bash", vec!["bash".into()]))
            .add_probe(FilesystemScopeProbe::new("/srv/scope", "/srv/scope/sub"));
        let report = runner.run(&mut passport, Duration::from_millis(100));
        assert_eq!(report.invocations, 2);
        assert_eq!(report.verified, 2);
        assert_eq!(report.failed, 0);
        assert!(
            passport
                .capabilities
                .iter()
                .all(|c| c.verification == CapabilityVerification::Verified)
        );
    }

    #[test]
    fn outcome_serde_roundtrip() {
        let cases = vec![
            ProbeOutcome::Verified(RedactedProof::from_value(b"some-proof")),
            ProbeOutcome::Failed {
                reason: "no go".into(),
            },
            ProbeOutcome::TimedOut,
        ];
        for outcome in cases {
            let json = serde_json::to_string(&outcome).expect("serialize");
            let back: ProbeOutcome = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, outcome);
        }
    }

    #[test]
    fn outcome_promotes_to_verified_only_for_verified_variant() {
        assert!(ProbeOutcome::Verified(RedactedProof::empty()).promotes_to_verified());
        assert!(!ProbeOutcome::Failed { reason: "x".into() }.promotes_to_verified());
        assert!(!ProbeOutcome::TimedOut.promotes_to_verified());
    }
}
