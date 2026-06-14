//! Proof-quality classifier for deferred proof outcomes.
//!
//! Bead: `ft-7h5da.9.3` (W8.3) of the 2026-06-06 dueling-idea-wizards
//! program. This module turns a retained proof intent plus one retained replay
//! attempt into a typed, fail-closed receipt: local fallback, stale source,
//! unconfirmed remote execution, and missing expected artifacts are never
//! classified as valid proof.

#![allow(clippy::module_name_repetitions)]

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::deferred_proof_replay::{
    DeferredProofReplayAttemptOutcome, DeferredProofReplayAttemptRecord,
};
use crate::proof_intent::{ProofIntent, ProofKind, ProofScope};

/// Stable contract id for W8.3 proof-quality receipts.
pub const PROOF_QUALITY_CONTRACT_ID: &str = "ft.proof_quality_receipt.v1";

/// Current schema version for proof-quality receipts.
pub const PROOF_QUALITY_SCHEMA_VERSION: u16 = 1;

/// Caller-supplied observation for an expected proof artifact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofArtifactObservation {
    /// The artifact was not checked. If the intent expected an artifact, this
    /// is fail-closed and cannot validate the proof.
    #[default]
    NotChecked,
    /// The artifact exists and satisfies its contract.
    PresentValid,
    /// The artifact exists but does not satisfy its contract.
    PresentInvalid,
    /// The artifact was expected but absent.
    Missing,
}

/// Artifact status recorded on the proof-quality receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofArtifactStatus {
    /// The proof intent did not require a retained artifact.
    NotExpected,
    /// An expected artifact exists and satisfies its contract.
    PresentValid,
    /// An expected artifact exists but fails contract validation.
    PresentInvalid,
    /// An expected artifact was absent.
    Missing,
    /// An expected artifact was not checked.
    NotChecked,
}

impl ProofArtifactStatus {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::NotExpected => "not_expected",
            Self::PresentValid => "present_valid",
            Self::PresentInvalid => "present_invalid",
            Self::Missing => "missing",
            Self::NotChecked => "not_checked",
        }
    }

    #[must_use]
    fn invalid_for_valid_proof(self) -> bool {
        matches!(
            self,
            Self::PresentInvalid | Self::Missing | Self::NotChecked
        )
    }
}

/// Package/workspace scope summary carried by proof-quality receipts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProofQualityScope {
    /// A single cargo package proof.
    Package {
        /// Cargo package name.
        package: String,
    },
    /// A workspace-wide proof.
    Workspace,
}

impl ProofQualityScope {
    #[must_use]
    fn from_intent(scope: &ProofScope) -> Self {
        match scope {
            ProofScope::Package { package } => Self::Package {
                package: package.clone(),
            },
            ProofScope::Workspace => Self::Workspace,
        }
    }

    #[must_use]
    fn as_str(&self) -> &str {
        match self {
            Self::Package { .. } => "package",
            Self::Workspace => "workspace",
        }
    }
}

/// Source freshness classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofSourceState {
    /// The attempt matches the current source hash.
    Current,
    /// The attempt was minted against a different source hash.
    Stale,
}

impl ProofSourceState {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Stale => "stale",
        }
    }
}

/// Where execution actually occurred according to retained attempt evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofExecutionVenue {
    /// A named remote RCH worker was reached.
    RemoteWorker,
    /// RCH attempted or reported local fallback.
    LocalFallback,
    /// The output did not prove remote worker execution.
    RemoteNotConfirmed,
}

impl ProofExecutionVenue {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::RemoteWorker => "remote_worker",
            Self::LocalFallback => "local_fallback",
            Self::RemoteNotConfirmed => "remote_not_confirmed",
        }
    }
}

/// Terminal quality class for a proof attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofTerminalClass {
    /// Remote worker reached the proof command, it passed, source is current,
    /// and any expected artifact is valid.
    ValidRemoteProof,
    /// Remote worker reached the proof command and the command failed.
    CodeFailure,
    /// Infrastructure prevented proof from being minted.
    TerminalInfraBlocker,
    /// Source hash drifted from the proof intent.
    StaleSource,
    /// The command passed but the expected retained artifact is missing or bad.
    InvalidArtifact,
}

impl ProofTerminalClass {
    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::ValidRemoteProof => "valid_remote_proof",
            Self::CodeFailure => "code_failure",
            Self::TerminalInfraBlocker => "terminal_infra_blocker",
            Self::StaleSource => "stale_source",
            Self::InvalidArtifact => "invalid_artifact",
        }
    }
}

/// Release impact for a proof receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofReleaseImpact {
    /// The proof supports a release/attestation slot.
    ReleaseBlocking,
    /// The proof is useful but not release-blocking.
    Supplemental,
}

impl ProofReleaseImpact {
    #[must_use]
    fn from_intent(intent: &ProofIntent) -> Self {
        if intent.attestation_slot.is_some() {
            Self::ReleaseBlocking
        } else {
            Self::Supplemental
        }
    }

    #[must_use]
    fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseBlocking => "release_blocking",
            Self::Supplemental => "supplemental",
        }
    }
}

/// Stable blocker vocabulary for proof-quality receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofQualityBlocker {
    /// Live source hash differs from the intent source hash.
    SourceStale,
    /// RCH local fallback was detected.
    RchLocalFallback,
    /// RCH reported `worker=null`.
    RchWorkerNull,
    /// RCH reported no admissible workers.
    RchNoAdmissibleWorkers,
    /// RCH exited 143 before confirmed remote proof execution.
    RchExit143,
    /// RCH selected a remote worker but timed out.
    RchRemoteTimeout,
    /// RCH topology preflight failed before proof execution.
    RchTopologyPreflight,
    /// Remote execution was not confirmed.
    RchRemoteNotConfirmed,
    /// Remote proof command failed.
    CodeFailure,
    /// Expected artifact was not checked.
    ArtifactNotChecked,
    /// Expected artifact was missing.
    ArtifactMissing,
    /// Expected artifact was invalid.
    ArtifactInvalid,
}

impl ProofQualityBlocker {
    /// Stable string token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceStale => "source.stale",
            Self::RchLocalFallback => "rch.local_fallback",
            Self::RchWorkerNull => "rch.worker_null",
            Self::RchNoAdmissibleWorkers => "rch.no_admissible_workers",
            Self::RchExit143 => "rch.exit_143",
            Self::RchRemoteTimeout => "rch.remote_timeout",
            Self::RchTopologyPreflight => "rch.topology_preflight_failed",
            Self::RchRemoteNotConfirmed => "rch.remote_not_confirmed",
            Self::CodeFailure => "proof.code_failure",
            Self::ArtifactNotChecked => "artifact.not_checked",
            Self::ArtifactMissing => "artifact.missing",
            Self::ArtifactInvalid => "artifact.invalid",
        }
    }
}

/// Input to [`classify_proof_quality`].
#[derive(Debug, Clone, Copy)]
pub struct ProofQualityInput<'a> {
    /// The stored proof intent.
    pub intent: &'a ProofIntent,
    /// Current source hash. Must match [`ProofIntent::source_hash`] for the
    /// attempt to be considered current.
    pub live_source_hash: &'a str,
    /// Retained live replay attempt record.
    pub attempt: &'a DeferredProofReplayAttemptRecord,
    /// Artifact observation for the expected artifact, if any.
    pub artifact_observation: ProofArtifactObservation,
    /// Optional explicit impact override. Defaults to `ReleaseBlocking` when
    /// the intent carries an attestation slot, otherwise `Supplemental`.
    pub release_impact: Option<ProofReleaseImpact>,
}

/// Typed receipt emitted by the proof-quality classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofQualityReceipt {
    /// Schema version.
    pub schema_version: u16,
    /// Contract id.
    pub contract_id: String,
    /// Content-addressed receipt id (`proof-quality:<32 hex>`).
    pub receipt_id: String,
    /// Source [`ProofIntent::intent_id`].
    pub intent_id: String,
    /// Optional Bead id from the proof intent.
    pub bead_id: Option<String>,
    /// Optional release attestation slot from the proof intent.
    pub attestation_slot: Option<String>,
    /// Proof kind (`test`, `check`, `clippy`, ...).
    pub proof_kind: ProofKind,
    /// Package/workspace scope.
    pub proof_scope: ProofQualityScope,
    /// Source freshness.
    pub source_state: ProofSourceState,
    /// Release impact.
    pub release_impact: ProofReleaseImpact,
    /// Execution venue.
    pub execution_venue: ProofExecutionVenue,
    /// Terminal quality class.
    pub terminal_class: ProofTerminalClass,
    /// Artifact status.
    pub artifact_status: ProofArtifactStatus,
    /// Whether a remote worker was confirmed.
    pub remote_worker_reached: bool,
    /// Whether local fallback was detected.
    pub local_fallback_detected: bool,
    /// Whether this receipt is acceptable as valid remote proof.
    pub valid_remote_proof: bool,
    /// Selected RCH worker, if known.
    pub selected_worker: Option<String>,
    /// RCH job id, if known.
    pub rch_job_id: Option<String>,
    /// Remote process exit status, if known.
    pub remote_exit_status: Option<i32>,
    /// Stable blocker strings.
    pub blockers: Vec<String>,
    /// Human-readable note.
    pub note: String,
}

/// Classify one proof attempt into a typed proof-quality receipt.
#[must_use]
pub fn classify_proof_quality(input: ProofQualityInput<'_>) -> ProofQualityReceipt {
    let source_state = if input.intent.is_stale(input.live_source_hash) {
        ProofSourceState::Stale
    } else {
        ProofSourceState::Current
    };
    let artifact_status = artifact_status(input.intent, input.artifact_observation);
    let execution_venue = execution_venue(input.attempt);
    let mut blockers = blockers_for_attempt(input.attempt, execution_venue);

    if source_state == ProofSourceState::Stale {
        blockers.push(ProofQualityBlocker::SourceStale);
    }

    if input.attempt.outcome == DeferredProofReplayAttemptOutcome::RemoteProofPassed {
        if let Some(blocker) = artifact_blocker(artifact_status) {
            push_unique_blocker(&mut blockers, blocker);
        }
    }

    let terminal_class = terminal_class(
        input.attempt,
        source_state,
        execution_venue,
        artifact_status,
    );
    let valid_remote_proof = terminal_class == ProofTerminalClass::ValidRemoteProof;
    let release_impact = input
        .release_impact
        .unwrap_or_else(|| ProofReleaseImpact::from_intent(input.intent));

    let mut receipt = ProofQualityReceipt {
        schema_version: PROOF_QUALITY_SCHEMA_VERSION,
        contract_id: PROOF_QUALITY_CONTRACT_ID.to_string(),
        receipt_id: String::new(),
        intent_id: input.intent.intent_id.clone(),
        bead_id: input.intent.bead_id.clone(),
        attestation_slot: input.intent.attestation_slot.clone(),
        proof_kind: input.intent.kind,
        proof_scope: ProofQualityScope::from_intent(&input.intent.scope),
        source_state,
        release_impact,
        execution_venue,
        terminal_class,
        artifact_status,
        remote_worker_reached: execution_venue == ProofExecutionVenue::RemoteWorker,
        local_fallback_detected: input.attempt.local_fallback_detected,
        valid_remote_proof,
        selected_worker: input.attempt.selected_worker.clone(),
        rch_job_id: input.attempt.rch_job_id.clone(),
        remote_exit_status: input.attempt.remote_exit_status,
        blockers: blockers
            .into_iter()
            .map(ProofQualityBlocker::as_str)
            .map(str::to_string)
            .collect(),
        note: note_for_terminal_class(terminal_class).to_string(),
    };
    receipt.receipt_id = receipt.compute_receipt_id();
    receipt
}

impl ProofQualityReceipt {
    /// Deterministic receipt id from the proof-quality binding.
    #[must_use]
    pub fn compute_receipt_id(&self) -> String {
        let remote_exit_status = self.remote_exit_status.map(|status| status.to_string());
        let canonical = [
            self.intent_id.as_str(),
            self.proof_kind.as_str(),
            self.proof_scope.as_str(),
            self.source_state.as_str(),
            self.release_impact.as_str(),
            self.execution_venue.as_str(),
            self.terminal_class.as_str(),
            self.artifact_status.as_str(),
            self.selected_worker.as_deref().unwrap_or(""),
            self.rch_job_id.as_deref().unwrap_or(""),
            remote_exit_status.as_deref().unwrap_or(""),
        ]
        .join("|");
        let digest = Sha256::digest(canonical.as_bytes());
        format!("proof-quality:{}", &hex_digest(&digest)[..32])
    }
}

fn artifact_status(
    intent: &ProofIntent,
    observation: ProofArtifactObservation,
) -> ProofArtifactStatus {
    if intent.expected_artifact_path.is_none() {
        return ProofArtifactStatus::NotExpected;
    }
    match observation {
        ProofArtifactObservation::NotChecked => ProofArtifactStatus::NotChecked,
        ProofArtifactObservation::PresentValid => ProofArtifactStatus::PresentValid,
        ProofArtifactObservation::PresentInvalid => ProofArtifactStatus::PresentInvalid,
        ProofArtifactObservation::Missing => ProofArtifactStatus::Missing,
    }
}

fn execution_venue(attempt: &DeferredProofReplayAttemptRecord) -> ProofExecutionVenue {
    if attempt.local_fallback_detected
        || attempt.outcome == DeferredProofReplayAttemptOutcome::BlockedLocalFallback
    {
        return ProofExecutionVenue::LocalFallback;
    }
    if attempt.remote_cargo_reached && attempt.selected_worker.is_some() {
        return ProofExecutionVenue::RemoteWorker;
    }
    ProofExecutionVenue::RemoteNotConfirmed
}

fn terminal_class(
    attempt: &DeferredProofReplayAttemptRecord,
    source_state: ProofSourceState,
    execution_venue: ProofExecutionVenue,
    artifact_status: ProofArtifactStatus,
) -> ProofTerminalClass {
    if source_state == ProofSourceState::Stale {
        return ProofTerminalClass::StaleSource;
    }
    match attempt.outcome {
        DeferredProofReplayAttemptOutcome::RemoteProofPassed => {
            if execution_venue != ProofExecutionVenue::RemoteWorker {
                return ProofTerminalClass::TerminalInfraBlocker;
            }
            if artifact_status.invalid_for_valid_proof() {
                return ProofTerminalClass::InvalidArtifact;
            }
            ProofTerminalClass::ValidRemoteProof
        }
        DeferredProofReplayAttemptOutcome::RemoteProofFailed => {
            if execution_venue == ProofExecutionVenue::RemoteWorker {
                ProofTerminalClass::CodeFailure
            } else {
                ProofTerminalClass::TerminalInfraBlocker
            }
        }
        DeferredProofReplayAttemptOutcome::BlockedLocalFallback
        | DeferredProofReplayAttemptOutcome::BlockedWorkerNull
        | DeferredProofReplayAttemptOutcome::BlockedNoAdmissibleWorkers
        | DeferredProofReplayAttemptOutcome::BlockedExit143
        | DeferredProofReplayAttemptOutcome::BlockedRemoteTimeout
        | DeferredProofReplayAttemptOutcome::BlockedTopologyPreflight
        | DeferredProofReplayAttemptOutcome::BlockedRemoteNotConfirmed => {
            ProofTerminalClass::TerminalInfraBlocker
        }
    }
}

fn artifact_blocker(status: ProofArtifactStatus) -> Option<ProofQualityBlocker> {
    match status {
        ProofArtifactStatus::PresentInvalid => Some(ProofQualityBlocker::ArtifactInvalid),
        ProofArtifactStatus::Missing => Some(ProofQualityBlocker::ArtifactMissing),
        ProofArtifactStatus::NotChecked => Some(ProofQualityBlocker::ArtifactNotChecked),
        ProofArtifactStatus::NotExpected | ProofArtifactStatus::PresentValid => None,
    }
}

fn blockers_for_attempt(
    attempt: &DeferredProofReplayAttemptRecord,
    execution_venue: ProofExecutionVenue,
) -> Vec<ProofQualityBlocker> {
    let mut blockers = Vec::new();
    if attempt.local_fallback_detected {
        push_unique_blocker(&mut blockers, ProofQualityBlocker::RchLocalFallback);
    }

    match attempt.outcome {
        DeferredProofReplayAttemptOutcome::RemoteProofPassed => {
            if execution_venue == ProofExecutionVenue::RemoteNotConfirmed {
                push_unique_blocker(&mut blockers, ProofQualityBlocker::RchRemoteNotConfirmed);
            }
        }
        DeferredProofReplayAttemptOutcome::RemoteProofFailed => {
            if execution_venue == ProofExecutionVenue::RemoteWorker {
                push_unique_blocker(&mut blockers, ProofQualityBlocker::CodeFailure);
            } else if execution_venue == ProofExecutionVenue::RemoteNotConfirmed {
                push_unique_blocker(&mut blockers, ProofQualityBlocker::RchRemoteNotConfirmed);
            }
        }
        DeferredProofReplayAttemptOutcome::BlockedLocalFallback => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchLocalFallback);
        }
        DeferredProofReplayAttemptOutcome::BlockedWorkerNull => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchWorkerNull);
        }
        DeferredProofReplayAttemptOutcome::BlockedNoAdmissibleWorkers => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchNoAdmissibleWorkers);
        }
        DeferredProofReplayAttemptOutcome::BlockedExit143 => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchExit143);
        }
        DeferredProofReplayAttemptOutcome::BlockedRemoteTimeout => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchRemoteTimeout);
        }
        DeferredProofReplayAttemptOutcome::BlockedTopologyPreflight => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchTopologyPreflight);
        }
        DeferredProofReplayAttemptOutcome::BlockedRemoteNotConfirmed => {
            push_unique_blocker(&mut blockers, ProofQualityBlocker::RchRemoteNotConfirmed);
        }
    }
    blockers
}

fn push_unique_blocker(blockers: &mut Vec<ProofQualityBlocker>, blocker: ProofQualityBlocker) {
    if !blockers.contains(&blocker) {
        blockers.push(blocker);
    }
}

fn note_for_terminal_class(terminal_class: ProofTerminalClass) -> &'static str {
    match terminal_class {
        ProofTerminalClass::ValidRemoteProof => {
            "Remote proof is valid for the current source tree."
        }
        ProofTerminalClass::CodeFailure => {
            "Remote worker reached the proof command and the command failed."
        }
        ProofTerminalClass::TerminalInfraBlocker => {
            "Infrastructure prevented a valid remote proof from being minted."
        }
        ProofTerminalClass::StaleSource => "Proof attempt was minted against a stale source hash.",
        ProofTerminalClass::InvalidArtifact => {
            "Proof command passed but the expected artifact was missing or invalid."
        }
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble & 0x0f {
        value @ 0..=9 => char::from(b'0' + value),
        value @ 10..=15 => char::from(b'a' + (value - 10)),
        _ => '?',
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deferred_proof_replay::{
        DEFERRED_PROOF_REPLAY_ATTEMPT_CONTRACT_ID, DEFERRED_PROOF_REPLAY_SCHEMA_VERSION,
        DeferredProofEnvVar,
    };
    use crate::proof_intent::{ProofRedactionPolicy, ProofScope};

    fn sample_intent(
        kind: ProofKind,
        scope: ProofScope,
        source_hash: &str,
        expected_artifact_path: Option<String>,
        attestation_slot: Option<String>,
    ) -> ProofIntent {
        ProofIntent::new(
            "RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-proof-quality cargo test -p frankenterm-core --lib proof_quality",
            scope,
            kind,
            source_hash,
            expected_artifact_path,
            true,
            Some("ft-7h5da.9.3".to_string()),
            attestation_slot,
            ProofRedactionPolicy::Standard,
            1_704_000_000_000,
        )
    }

    fn attempt(
        outcome: DeferredProofReplayAttemptOutcome,
        remote_cargo_reached: bool,
        local_fallback_detected: bool,
        remote_exit_status: Option<i32>,
    ) -> DeferredProofReplayAttemptRecord {
        DeferredProofReplayAttemptRecord {
            schema_version: DEFERRED_PROOF_REPLAY_SCHEMA_VERSION,
            contract_id: DEFERRED_PROOF_REPLAY_ATTEMPT_CONTRACT_ID.to_string(),
            bead_id: "ft-7h5da.9.3".to_string(),
            receipt_id: "receipt-1".to_string(),
            source_receipt_digest:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
            source_text_sha256: None,
            command_argv: vec!["rch".to_string(), "exec".to_string()],
            command_env: vec![
                DeferredProofEnvVar {
                    name: "RCH_REQUIRE_REMOTE".to_string(),
                    value: "1".to_string(),
                },
                DeferredProofEnvVar {
                    name: "RCH_NO_SELF_HEALING".to_string(),
                    value: "1".to_string(),
                },
            ],
            target_dir: Some("/tmp/ft-proof-quality".to_string()),
            selected_worker: remote_cargo_reached.then(|| "vmi1293453".to_string()),
            rch_job_id: remote_cargo_reached.then(|| "j-29884604911452330".to_string()),
            remote_exit_status,
            outcome,
            blockers: Vec::new(),
            remote_cargo_reached,
            local_fallback_detected,
            stdout_path: None,
            stderr_path: None,
            attempt_record_path: None,
            note: "test attempt".to_string(),
        }
    }

    #[test]
    fn remote_pass_with_current_source_and_valid_artifact_mints_valid_receipt() {
        let intent = sample_intent(
            ProofKind::Test,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            Some("target/proof-quality.json".to_string()),
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofPassed,
            true,
            false,
            Some(0),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::PresentValid,
            release_impact: None,
        });

        assert_eq!(receipt.contract_id, PROOF_QUALITY_CONTRACT_ID);
        assert_eq!(receipt.proof_kind, ProofKind::Test);
        assert_eq!(
            receipt.proof_scope,
            ProofQualityScope::Package {
                package: "frankenterm-core".to_string()
            }
        );
        assert_eq!(receipt.execution_venue, ProofExecutionVenue::RemoteWorker);
        assert_eq!(receipt.terminal_class, ProofTerminalClass::ValidRemoteProof);
        assert_eq!(receipt.artifact_status, ProofArtifactStatus::PresentValid);
        assert_eq!(receipt.release_impact, ProofReleaseImpact::Supplemental);
        assert!(receipt.remote_worker_reached);
        assert!(receipt.valid_remote_proof);
        assert!(receipt.blockers.is_empty());
        assert!(receipt.receipt_id.starts_with("proof-quality:"));
    }

    #[test]
    fn local_fallback_is_never_valid_remote_proof() {
        let intent = sample_intent(
            ProofKind::Check,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            None,
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::BlockedLocalFallback,
            false,
            true,
            Some(0),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(receipt.execution_venue, ProofExecutionVenue::LocalFallback);
        assert_eq!(
            receipt.terminal_class,
            ProofTerminalClass::TerminalInfraBlocker
        );
        assert!(!receipt.valid_remote_proof);
        assert_eq!(receipt.blockers, vec!["rch.local_fallback"]);
    }

    #[test]
    fn stale_source_overrides_a_passing_attempt() {
        let intent = sample_intent(
            ProofKind::Clippy,
            ProofScope::Workspace,
            "sha256:tree-a",
            None,
            Some("release.clippy".to_string()),
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofPassed,
            true,
            false,
            Some(0),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-b",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(receipt.proof_kind, ProofKind::Clippy);
        assert_eq!(receipt.proof_scope, ProofQualityScope::Workspace);
        assert_eq!(receipt.source_state, ProofSourceState::Stale);
        assert_eq!(receipt.terminal_class, ProofTerminalClass::StaleSource);
        assert_eq!(receipt.release_impact, ProofReleaseImpact::ReleaseBlocking);
        assert!(!receipt.valid_remote_proof);
        assert_eq!(receipt.blockers, vec!["source.stale"]);
    }

    #[test]
    fn expected_artifact_must_be_checked_and_valid() {
        let intent = sample_intent(
            ProofKind::Schema,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            Some("target/schema-proof.json".to_string()),
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofPassed,
            true,
            false,
            Some(0),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(receipt.artifact_status, ProofArtifactStatus::NotChecked);
        assert_eq!(receipt.terminal_class, ProofTerminalClass::InvalidArtifact);
        assert!(!receipt.valid_remote_proof);
        assert_eq!(receipt.blockers, vec!["artifact.not_checked"]);
    }

    #[test]
    fn remote_code_failure_is_distinct_from_infra_blocker() {
        let intent = sample_intent(
            ProofKind::Test,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            None,
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofFailed,
            true,
            false,
            Some(101),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(receipt.execution_venue, ProofExecutionVenue::RemoteWorker);
        assert_eq!(receipt.terminal_class, ProofTerminalClass::CodeFailure);
        assert!(!receipt.valid_remote_proof);
        assert_eq!(receipt.blockers, vec!["proof.code_failure"]);
    }

    #[test]
    fn passed_attempt_with_local_fallback_flag_records_local_fallback_blocker() {
        let intent = sample_intent(
            ProofKind::Check,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            None,
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofPassed,
            false,
            true,
            Some(0),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(receipt.execution_venue, ProofExecutionVenue::LocalFallback);
        assert_eq!(
            receipt.terminal_class,
            ProofTerminalClass::TerminalInfraBlocker
        );
        assert!(!receipt.valid_remote_proof);
        assert_eq!(receipt.blockers, vec!["rch.local_fallback"]);
    }

    #[test]
    fn passed_attempt_without_confirmed_worker_records_remote_not_confirmed() {
        let intent = sample_intent(
            ProofKind::Test,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            None,
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofPassed,
            false,
            false,
            Some(0),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(
            receipt.execution_venue,
            ProofExecutionVenue::RemoteNotConfirmed
        );
        assert_eq!(
            receipt.terminal_class,
            ProofTerminalClass::TerminalInfraBlocker
        );
        assert!(!receipt.valid_remote_proof);
        assert_eq!(receipt.blockers, vec!["rch.remote_not_confirmed"]);
    }

    #[test]
    fn remote_failure_without_confirmed_worker_is_infra_not_code_failure() {
        let intent = sample_intent(
            ProofKind::Test,
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            "sha256:tree-a",
            None,
            None,
        );
        let attempt = attempt(
            DeferredProofReplayAttemptOutcome::RemoteProofFailed,
            false,
            false,
            Some(101),
        );

        let receipt = classify_proof_quality(ProofQualityInput {
            intent: &intent,
            live_source_hash: "sha256:tree-a",
            attempt: &attempt,
            artifact_observation: ProofArtifactObservation::NotChecked,
            release_impact: None,
        });

        assert_eq!(
            receipt.terminal_class,
            ProofTerminalClass::TerminalInfraBlocker
        );
        assert!(!receipt.blockers.contains(&"proof.code_failure".to_string()));
        assert_eq!(receipt.blockers, vec!["rch.remote_not_confirmed"]);
    }

    #[test]
    fn infra_blocker_outcomes_preserve_reason_codes() {
        let cases = [
            (
                DeferredProofReplayAttemptOutcome::BlockedWorkerNull,
                "rch.worker_null",
            ),
            (
                DeferredProofReplayAttemptOutcome::BlockedNoAdmissibleWorkers,
                "rch.no_admissible_workers",
            ),
            (
                DeferredProofReplayAttemptOutcome::BlockedExit143,
                "rch.exit_143",
            ),
            (
                DeferredProofReplayAttemptOutcome::BlockedRemoteTimeout,
                "rch.remote_timeout",
            ),
            (
                DeferredProofReplayAttemptOutcome::BlockedTopologyPreflight,
                "rch.topology_preflight_failed",
            ),
            (
                DeferredProofReplayAttemptOutcome::BlockedRemoteNotConfirmed,
                "rch.remote_not_confirmed",
            ),
        ];

        for (outcome, blocker) in cases {
            let intent = sample_intent(
                ProofKind::Test,
                ProofScope::Package {
                    package: "frankenterm-core".to_string(),
                },
                "sha256:tree-a",
                None,
                None,
            );
            let attempt = attempt(outcome, false, false, None);

            let receipt = classify_proof_quality(ProofQualityInput {
                intent: &intent,
                live_source_hash: "sha256:tree-a",
                attempt: &attempt,
                artifact_observation: ProofArtifactObservation::NotChecked,
                release_impact: None,
            });

            assert_eq!(
                receipt.terminal_class,
                ProofTerminalClass::TerminalInfraBlocker
            );
            assert_eq!(receipt.blockers, vec![blocker]);
            assert!(!receipt.valid_remote_proof);
        }
    }

    #[test]
    fn all_proof_kinds_survive_classification() {
        let kinds = [
            ProofKind::Test,
            ProofKind::Check,
            ProofKind::Clippy,
            ProofKind::Fmt,
            ProofKind::Schema,
            ProofKind::Fuzz,
            ProofKind::Replay,
            ProofKind::Attestation,
        ];

        for kind in kinds {
            let intent = sample_intent(
                kind,
                ProofScope::Package {
                    package: "frankenterm-core".to_string(),
                },
                "sha256:tree-a",
                None,
                None,
            );
            let attempt = attempt(
                DeferredProofReplayAttemptOutcome::RemoteProofPassed,
                true,
                false,
                Some(0),
            );

            let receipt = classify_proof_quality(ProofQualityInput {
                intent: &intent,
                live_source_hash: "sha256:tree-a",
                attempt: &attempt,
                artifact_observation: ProofArtifactObservation::NotChecked,
                release_impact: None,
            });

            assert_eq!(receipt.proof_kind, kind);
            assert_eq!(receipt.terminal_class, ProofTerminalClass::ValidRemoteProof);
            assert!(receipt.valid_remote_proof);
        }
    }
}
