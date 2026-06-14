//! Fail-closed deferred RCH proof replay substrate.
//!
//! This module consumes already-retained deferred proof receipts and decides
//! whether exactly one proof command is safe to replay. Dry-run decisions never
//! mint material proof. Live replay only runs canonical remote-only RCH commands
//! and classifies local fallback / admission failures as blockers.

#![allow(clippy::module_name_repetitions)]

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::proof_intent::{ProofIntentQueueEntry, ProofScope};

pub const DEFERRED_PROOF_REPLAY_DECISION_CONTRACT_ID: &str =
    "ft.deferred_proof_replay_harness.decision.v1";
pub const DEFERRED_PROOF_REPLAY_ATTEMPT_CONTRACT_ID: &str = "ft.deferred_proof_replay_attempt.v1";
pub const DEFERRED_PROOF_REPLAY_SCHEMA_VERSION: u16 = 1;

pub const SHAPE_RCH_NO_SELF_HEALING: &str = "rch-no-self-healing-v1";
pub const SHAPE_STATIC_VERIFIER: &str = "static-verifier-v1";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofReceipt {
    #[serde(default)]
    pub bead_id: String,
    #[serde(default)]
    pub receipt_id: String,
    #[serde(default)]
    pub source: Option<DeferredProofSource>,
    #[serde(default)]
    pub command: DeferredProofCommand,
    #[serde(default)]
    pub proof: DeferredProofProof,
    #[serde(default)]
    pub paths: DeferredProofPaths,
    #[serde(default)]
    pub coordination: DeferredProofCoordination,
    #[serde(default)]
    pub eligibility: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofSource {
    #[serde(default)]
    pub source_text_sha256: Option<String>,
    #[serde(default)]
    pub comment_id: Option<u64>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofCommand {
    #[serde(default)]
    pub command_shape_version: String,
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<DeferredProofEnvVar>,
    #[serde(default)]
    pub env_allowlist: Vec<String>,
    #[serde(default)]
    pub target_dir: Option<String>,
    #[serde(default)]
    pub material_remote_required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofEnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofProof {
    #[serde(default)]
    pub expected_kind: Option<String>,
    #[serde(default)]
    pub package: Option<String>,
    #[serde(default)]
    pub test_filter: Option<String>,
    #[serde(default)]
    pub feature_set: Vec<String>,
    #[serde(default)]
    pub material_cargo_required: bool,
    #[serde(default)]
    pub evidence_classification: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofPaths {
    #[serde(default)]
    pub owned_paths: Vec<String>,
    #[serde(default)]
    pub dirty_paths_at_capture: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofCoordination {
    #[serde(default)]
    pub prerequisite_beads: Vec<String>,
    #[serde(default)]
    pub agent_mail_state: Option<String>,
    #[serde(default)]
    pub rch_admission_state: Option<String>,
    #[serde(default)]
    pub operator_cancelled: bool,
    #[serde(default)]
    pub selected_worker: Option<String>,
    #[serde(default)]
    pub rch_job_id: Option<String>,
    #[serde(default)]
    pub remote_failure_phase: Option<String>,
    #[serde(default)]
    pub remote_failure_detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredProofReplayDecision {
    RunStaticNow,
    WouldRunRemote,
    DeferRemoteBlocked,
    DeferDirtyOverlap,
    DeferPrerequisite,
    Cancelled,
    RejectStaleCommand,
    RejectNonRemoteCommand,
    RequestTriage,
}

impl DeferredProofReplayDecision {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RunStaticNow => "run_static_now",
            Self::WouldRunRemote => "would_run_remote",
            Self::DeferRemoteBlocked => "defer_remote_blocked",
            Self::DeferDirtyOverlap => "defer_dirty_overlap",
            Self::DeferPrerequisite => "defer_prerequisite",
            Self::Cancelled => "cancelled",
            Self::RejectStaleCommand => "reject_stale_command",
            Self::RejectNonRemoteCommand => "reject_non_remote_command",
            Self::RequestTriage => "request_triage",
        }
    }

    #[must_use]
    pub fn is_candidate(self) -> bool {
        matches!(self, Self::RunStaticNow | Self::WouldRunRemote)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredProofReplayBlocker {
    RchTopologyPreflightFailed,
    RchWorkerPressure,
    RchLocalFallback,
    RchWorkerNull,
    RchNoAdmissibleWorkers,
    RchExit143,
    RchRemoteTimeout,
    RchStuckDetectorCancelled,
    RchRemoteNotConfirmed,
    OverlapDirtyPaths,
    PrereqBeadOpen,
    OperatorCancelled,
    CommandStaleShape,
    CommandNotRemoteOnly,
    TriageAmbiguous,
}

impl DeferredProofReplayBlocker {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RchTopologyPreflightFailed => "rch.topology_preflight_failed",
            Self::RchWorkerPressure => "rch.worker_pressure",
            Self::RchLocalFallback => "rch.local_fallback",
            Self::RchWorkerNull => "rch.worker_null",
            Self::RchNoAdmissibleWorkers => "rch.no_admissible_workers",
            Self::RchExit143 => "rch.exit_143",
            Self::RchRemoteTimeout => "rch.remote_timeout",
            Self::RchStuckDetectorCancelled => "rch.stuck_detector_cancelled",
            Self::RchRemoteNotConfirmed => "rch.remote_not_confirmed",
            Self::OverlapDirtyPaths => "overlap.dirty_paths",
            Self::PrereqBeadOpen => "prereq.bead_open",
            Self::OperatorCancelled => "operator.cancelled",
            Self::CommandStaleShape => "command.stale_shape",
            Self::CommandNotRemoteOnly => "command.not_remote_only",
            Self::TriageAmbiguous => "triage.ambiguous",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredProofReplayClassification {
    pub decision: DeferredProofReplayDecision,
    pub blockers: Vec<DeferredProofReplayBlocker>,
}

impl DeferredProofReplayClassification {
    #[must_use]
    pub fn blocker_strings(&self) -> Vec<String> {
        self.blockers
            .iter()
            .map(|blocker| blocker.as_str().to_string())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofReplayDecisionRecord {
    pub schema_version: u16,
    pub contract_id: String,
    pub bead_id: String,
    pub receipt_id: String,
    pub command_shape_version: String,
    pub decision: DeferredProofReplayDecision,
    pub blockers: Vec<String>,
    pub material_remote_required: bool,
    pub target_dir: Option<String>,
    pub selected_worker: Option<String>,
    pub remote_exit_status: Option<i32>,
    pub dry_run: bool,
    pub replay_allowed_now: bool,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofCommandInvocation {
    pub argv: Vec<String>,
    pub env: Vec<DeferredProofEnvVar>,
}

impl DeferredProofCommandInvocation {
    #[must_use]
    pub fn from_receipt(receipt: &DeferredProofReceipt) -> Self {
        Self {
            argv: receipt.command.argv.clone(),
            env: receipt.command.env.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredProofProcessOutput {
    pub exit_status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl DeferredProofProcessOutput {
    #[must_use]
    pub fn combined_lossy(&self) -> String {
        let mut combined = String::from_utf8_lossy(&self.stdout).to_string();
        combined.push('\n');
        combined.push_str(&String::from_utf8_lossy(&self.stderr));
        combined
    }
}

pub trait DeferredProofCommandExecutor {
    fn run(
        &self,
        invocation: &DeferredProofCommandInvocation,
    ) -> io::Result<DeferredProofProcessOutput>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StdDeferredProofCommandExecutor;

impl DeferredProofCommandExecutor for StdDeferredProofCommandExecutor {
    fn run(
        &self,
        invocation: &DeferredProofCommandInvocation,
    ) -> io::Result<DeferredProofProcessOutput> {
        let Some(program) = invocation.argv.first() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty deferred-proof command argv",
            ));
        };

        if program != "rch" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "deferred-proof live replay only executes the rch binary",
            ));
        }

        let mut command = Command::new("rch");
        command.args(invocation.argv.iter().skip(1));
        for env in &invocation.env {
            command.env(&env.name, &env.value);
        }
        let output = command.output()?;
        Ok(DeferredProofProcessOutput {
            exit_status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeferredProofReplayAttemptOutcome {
    RemoteProofPassed,
    RemoteProofFailed,
    BlockedLocalFallback,
    BlockedWorkerNull,
    BlockedNoAdmissibleWorkers,
    BlockedExit143,
    BlockedRemoteTimeout,
    BlockedStuckDetectorCancelled,
    BlockedTopologyPreflight,
    BlockedRemoteNotConfirmed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredProofReplayAttemptRecord {
    pub schema_version: u16,
    pub contract_id: String,
    pub bead_id: String,
    pub receipt_id: String,
    pub source_receipt_digest: String,
    pub source_text_sha256: Option<String>,
    pub command_argv: Vec<String>,
    pub command_env: Vec<DeferredProofEnvVar>,
    pub target_dir: Option<String>,
    pub selected_worker: Option<String>,
    pub rch_job_id: Option<String>,
    pub remote_exit_status: Option<i32>,
    pub outcome: DeferredProofReplayAttemptOutcome,
    pub blockers: Vec<String>,
    pub remote_cargo_reached: bool,
    pub local_fallback_detected: bool,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
    pub attempt_record_path: Option<String>,
    pub note: String,
}

#[derive(Debug)]
pub enum DeferredProofReplayError {
    EmptyReceiptSet,
    NoRemoteCandidate,
    NonRemoteCandidate { receipt_id: String },
    Io(io::Error),
    Serialize(serde_json::Error),
}

impl fmt::Display for DeferredProofReplayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyReceiptSet => write!(formatter, "deferred proof receipt set is empty"),
            Self::NoRemoteCandidate => write!(
                formatter,
                "no canonical would_run_remote deferred proof receipt is available"
            ),
            Self::NonRemoteCandidate { receipt_id } => write!(
                formatter,
                "deferred proof receipt {receipt_id} is not a canonical remote replay candidate"
            ),
            Self::Io(error) => write!(formatter, "deferred proof replay I/O failed: {error}"),
            Self::Serialize(error) => {
                write!(
                    formatter,
                    "deferred proof replay serialization failed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for DeferredProofReplayError {}

impl From<io::Error> for DeferredProofReplayError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for DeferredProofReplayError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialize(error)
    }
}

pub struct DeferredProofReplayRunner<E = StdDeferredProofCommandExecutor> {
    executor: E,
}

impl Default for DeferredProofReplayRunner<StdDeferredProofCommandExecutor> {
    fn default() -> Self {
        Self {
            executor: StdDeferredProofCommandExecutor,
        }
    }
}

impl<E> DeferredProofReplayRunner<E>
where
    E: DeferredProofCommandExecutor,
{
    #[must_use]
    pub fn new(executor: E) -> Self {
        Self { executor }
    }

    #[must_use]
    pub fn explain(
        &self,
        receipts: &[DeferredProofReceipt],
    ) -> Vec<DeferredProofReplayDecisionRecord> {
        decision_records(receipts)
    }

    pub fn run_live_remote_once(
        &self,
        receipts: &[DeferredProofReceipt],
        artifact_dir: impl AsRef<Path>,
    ) -> Result<DeferredProofReplayAttemptRecord, DeferredProofReplayError> {
        if receipts.is_empty() {
            return Err(DeferredProofReplayError::EmptyReceiptSet);
        }

        let receipt = select_next_remote_receipt(receipts)
            .ok_or(DeferredProofReplayError::NoRemoteCandidate)?;
        let preflight = classify_deferred_proof_receipt(receipt);
        if preflight.decision != DeferredProofReplayDecision::WouldRunRemote {
            return Err(DeferredProofReplayError::NonRemoteCandidate {
                receipt_id: receipt.receipt_id.clone(),
            });
        }

        let invocation = DeferredProofCommandInvocation::from_receipt(receipt);
        let output = self.executor.run(&invocation)?;
        write_attempt_artifacts(receipt, &output, artifact_dir.as_ref())
    }
}

#[must_use]
pub fn classify_deferred_proof_receipt(
    receipt: &DeferredProofReceipt,
) -> DeferredProofReplayClassification {
    let command = &receipt.command;
    let coordination = &receipt.coordination;

    if coordination.operator_cancelled {
        return classification(
            DeferredProofReplayDecision::Cancelled,
            [DeferredProofReplayBlocker::OperatorCancelled],
        );
    }

    if !matches!(
        command.command_shape_version.as_str(),
        SHAPE_RCH_NO_SELF_HEALING | SHAPE_STATIC_VERIFIER
    ) {
        return classification(
            DeferredProofReplayDecision::RejectStaleCommand,
            [DeferredProofReplayBlocker::CommandStaleShape],
        );
    }

    if command.argv.is_empty()
        || receipt.proof.evidence_classification.as_deref() == Some("ambiguous")
    {
        return classification(
            DeferredProofReplayDecision::RequestTriage,
            [DeferredProofReplayBlocker::TriageAmbiguous],
        );
    }

    if !command.material_remote_required {
        if command.command_shape_version == SHAPE_STATIC_VERIFIER {
            return classification(DeferredProofReplayDecision::RunStaticNow, []);
        }
        return classification(
            DeferredProofReplayDecision::RequestTriage,
            [DeferredProofReplayBlocker::TriageAmbiguous],
        );
    }

    if !remote_only_command(command) {
        return classification(
            DeferredProofReplayDecision::RejectNonRemoteCommand,
            [DeferredProofReplayBlocker::CommandNotRemoteOnly],
        );
    }

    if !receipt.paths.dirty_paths_at_capture.is_empty() {
        return classification(
            DeferredProofReplayDecision::DeferDirtyOverlap,
            [DeferredProofReplayBlocker::OverlapDirtyPaths],
        );
    }

    if !coordination.prerequisite_beads.is_empty() {
        return classification(
            DeferredProofReplayDecision::DeferPrerequisite,
            [DeferredProofReplayBlocker::PrereqBeadOpen],
        );
    }

    if let Some(blocker) = admission_blocker(coordination.rch_admission_state.as_deref()) {
        return classification(DeferredProofReplayDecision::DeferRemoteBlocked, [blocker]);
    }

    if coordination.rch_admission_state.as_deref() == Some("admitted") {
        return classification(DeferredProofReplayDecision::WouldRunRemote, []);
    }

    classification(
        DeferredProofReplayDecision::RequestTriage,
        [DeferredProofReplayBlocker::TriageAmbiguous],
    )
}

#[must_use]
pub fn decision_record(receipt: &DeferredProofReceipt) -> DeferredProofReplayDecisionRecord {
    let classification = classify_deferred_proof_receipt(receipt);
    let decision = classification.decision;
    DeferredProofReplayDecisionRecord {
        schema_version: DEFERRED_PROOF_REPLAY_SCHEMA_VERSION,
        contract_id: DEFERRED_PROOF_REPLAY_DECISION_CONTRACT_ID.to_string(),
        bead_id: receipt.bead_id.clone(),
        receipt_id: receipt.receipt_id.clone(),
        command_shape_version: receipt.command.command_shape_version.clone(),
        decision,
        blockers: classification.blocker_strings(),
        material_remote_required: receipt.command.material_remote_required,
        target_dir: receipt.command.target_dir.clone(),
        selected_worker: receipt.coordination.selected_worker.clone(),
        remote_exit_status: None,
        dry_run: true,
        replay_allowed_now: decision == DeferredProofReplayDecision::RunStaticNow,
        note: note_for_decision(decision).to_string(),
    }
}

#[must_use]
pub fn decision_records(
    receipts: &[DeferredProofReceipt],
) -> Vec<DeferredProofReplayDecisionRecord> {
    receipts.iter().map(decision_record).collect()
}

#[must_use]
pub fn select_next_candidate(
    records: &[DeferredProofReplayDecisionRecord],
) -> Option<&DeferredProofReplayDecisionRecord> {
    [
        DeferredProofReplayDecision::RunStaticNow,
        DeferredProofReplayDecision::WouldRunRemote,
    ]
    .into_iter()
    .find_map(|decision| {
        records
            .iter()
            .filter(|record| record.decision == decision)
            .min_by(|left, right| {
                left.bead_id
                    .cmp(&right.bead_id)
                    .then_with(|| left.receipt_id.cmp(&right.receipt_id))
            })
    })
}

#[must_use]
pub fn select_next_remote_receipt(
    receipts: &[DeferredProofReceipt],
) -> Option<&DeferredProofReceipt> {
    receipts
        .iter()
        .filter(|receipt| {
            classify_deferred_proof_receipt(receipt).decision
                == DeferredProofReplayDecision::WouldRunRemote
        })
        .min_by(|left, right| {
            left.bead_id
                .cmp(&right.bead_id)
                .then_with(|| left.receipt_id.cmp(&right.receipt_id))
        })
}

#[must_use]
pub fn deferred_receipt_from_proof_intent_queue_entry(
    entry: &ProofIntentQueueEntry,
    rch_admission_state: Option<&str>,
) -> DeferredProofReceipt {
    let package = match &entry.intent.scope {
        ProofScope::Package { package } => Some(package.clone()),
        ProofScope::Workspace => None,
    };
    DeferredProofReceipt {
        bead_id: entry.intent.bead_id.clone().unwrap_or_default(),
        receipt_id: entry.intent.intent_id.clone(),
        source: Some(DeferredProofSource {
            source_text_sha256: Some(entry.intent.source_hash.clone()),
            ..Default::default()
        }),
        command: DeferredProofCommand {
            command_shape_version: if entry.intent.required_remote {
                SHAPE_RCH_NO_SELF_HEALING.to_string()
            } else {
                SHAPE_STATIC_VERIFIER.to_string()
            },
            argv: entry.command_argv.clone(),
            env: entry
                .command_env
                .iter()
                .map(|env| DeferredProofEnvVar {
                    name: env.name.clone(),
                    value: env.value.clone(),
                })
                .collect(),
            target_dir: entry.target_dir.clone(),
            material_remote_required: entry.intent.required_remote,
            ..Default::default()
        },
        proof: DeferredProofProof {
            expected_kind: Some(entry.intent.kind.as_str().to_string()),
            package,
            material_cargo_required: entry.intent.required_remote,
            evidence_classification: Some("deferred".to_string()),
            ..Default::default()
        },
        coordination: DeferredProofCoordination {
            rch_admission_state: Some(
                rch_admission_state
                    .unwrap_or(&entry.rch_admission_state)
                    .to_string(),
            ),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[must_use]
pub fn build_live_attempt_record(
    receipt: &DeferredProofReceipt,
    output: &DeferredProofProcessOutput,
) -> DeferredProofReplayAttemptRecord {
    let combined = output.combined_lossy();
    let lower = combined.to_ascii_lowercase();
    let local_fallback_detected = contains_local_fallback(&lower);
    let selected_worker = selected_worker_from_text(&combined)
        .or_else(|| receipt.coordination.selected_worker.clone());
    let remote_cargo_reached = selected_worker.is_some()
        && !local_fallback_detected
        && !contains_worker_null(&lower)
        && !contains_topology_preflight_failure(&lower);
    let (outcome, blockers) =
        classify_live_output(output.exit_status, &lower, remote_cargo_reached);

    DeferredProofReplayAttemptRecord {
        schema_version: DEFERRED_PROOF_REPLAY_SCHEMA_VERSION,
        contract_id: DEFERRED_PROOF_REPLAY_ATTEMPT_CONTRACT_ID.to_string(),
        bead_id: receipt.bead_id.clone(),
        receipt_id: receipt.receipt_id.clone(),
        source_receipt_digest: source_receipt_digest(receipt),
        source_text_sha256: receipt
            .source
            .as_ref()
            .and_then(|source| source.source_text_sha256.clone()),
        command_argv: receipt.command.argv.clone(),
        command_env: receipt.command.env.clone(),
        target_dir: receipt.command.target_dir.clone(),
        selected_worker,
        rch_job_id: rch_job_from_text(&combined)
            .or_else(|| receipt.coordination.rch_job_id.clone()),
        remote_exit_status: output.exit_status,
        outcome,
        blockers: blockers
            .iter()
            .map(|blocker| blocker.as_str().to_string())
            .collect(),
        remote_cargo_reached,
        local_fallback_detected,
        stdout_path: None,
        stderr_path: None,
        attempt_record_path: None,
        note: note_for_attempt(outcome).to_string(),
    }
}

pub fn write_attempt_artifacts(
    receipt: &DeferredProofReceipt,
    output: &DeferredProofProcessOutput,
    artifact_dir: &Path,
) -> Result<DeferredProofReplayAttemptRecord, DeferredProofReplayError> {
    fs::create_dir_all(artifact_dir)?;

    let safe_stem = safe_file_stem(&receipt.receipt_id);
    let stdout_path = artifact_dir.join(format!("{safe_stem}.stdout.txt"));
    let stderr_path = artifact_dir.join(format!("{safe_stem}.stderr.txt"));
    let attempt_path = artifact_dir.join(format!("{safe_stem}.attempt.json"));

    fs::write(&stdout_path, &output.stdout)?;
    fs::write(&stderr_path, &output.stderr)?;

    let mut record = build_live_attempt_record(receipt, output);
    record.stdout_path = Some(path_to_string(stdout_path));
    record.stderr_path = Some(path_to_string(stderr_path));
    record.attempt_record_path = Some(path_to_string(attempt_path.clone()));

    let bytes = serde_json::to_vec_pretty(&record)?;
    fs::write(attempt_path, bytes)?;
    Ok(record)
}

#[must_use]
pub fn remote_only_command(command: &DeferredProofCommand) -> bool {
    let argv = &command.argv;
    if argv.first().map(String::as_str) != Some("rch") {
        return false;
    }

    let Some(exec_index) = argv.iter().position(|arg| arg == "exec") else {
        return false;
    };
    if argv.get(exec_index + 1).map(String::as_str) != Some("--") {
        return false;
    }
    if !argv
        .iter()
        .take(exec_index)
        .any(|arg| arg == "--no-self-healing")
    {
        return false;
    }
    if env_value(command, "RCH_REQUIRE_REMOTE") != Some("1") {
        return false;
    }
    if env_value(command, "RCH_NO_SELF_HEALING") != Some("1") {
        return false;
    }

    if let Some(target_dir) = &command.target_dir {
        if !safe_target_dir(target_dir) {
            return false;
        }
        return argv
            .iter()
            .any(|arg| arg == &format!("CARGO_TARGET_DIR={target_dir}"));
    }

    true
}

fn classification<const N: usize>(
    decision: DeferredProofReplayDecision,
    blockers: [DeferredProofReplayBlocker; N],
) -> DeferredProofReplayClassification {
    DeferredProofReplayClassification {
        decision,
        blockers: blockers.into(),
    }
}

fn admission_blocker(state: Option<&str>) -> Option<DeferredProofReplayBlocker> {
    match state {
        Some("topology_preflight_failed") => {
            Some(DeferredProofReplayBlocker::RchTopologyPreflightFailed)
        }
        Some(
            "critical_pressure"
            | "no_admissible_workers"
            | "blocked_worker_pressure"
            | "insufficient_slots"
            | "telemetry_gap"
            | "active_project_exclusion"
            | "wait_rch",
        ) => Some(DeferredProofReplayBlocker::RchWorkerPressure),
        _ => None,
    }
}

fn env_value<'a>(command: &'a DeferredProofCommand, name: &str) -> Option<&'a str> {
    command
        .env
        .iter()
        .find(|env| env.name == name)
        .map(|env| env.value.as_str())
}

fn classify_live_output(
    exit_status: Option<i32>,
    lower_output: &str,
    remote_cargo_reached: bool,
) -> (
    DeferredProofReplayAttemptOutcome,
    Vec<DeferredProofReplayBlocker>,
) {
    if contains_topology_preflight_failure(lower_output) {
        return (
            DeferredProofReplayAttemptOutcome::BlockedTopologyPreflight,
            vec![DeferredProofReplayBlocker::RchTopologyPreflightFailed],
        );
    }
    if contains_local_fallback(lower_output) {
        return (
            DeferredProofReplayAttemptOutcome::BlockedLocalFallback,
            vec![DeferredProofReplayBlocker::RchLocalFallback],
        );
    }
    if contains_worker_null(lower_output) {
        return (
            DeferredProofReplayAttemptOutcome::BlockedWorkerNull,
            vec![DeferredProofReplayBlocker::RchWorkerNull],
        );
    }
    if lower_output.contains("no admissible worker")
        || lower_output.contains("no admissible workers")
    {
        return (
            DeferredProofReplayAttemptOutcome::BlockedNoAdmissibleWorkers,
            vec![DeferredProofReplayBlocker::RchNoAdmissibleWorkers],
        );
    }
    if exit_status == Some(143) || lower_output.contains("exit 143") {
        return (
            DeferredProofReplayAttemptOutcome::BlockedExit143,
            vec![DeferredProofReplayBlocker::RchExit143],
        );
    }
    if contains_remote_timeout(lower_output) {
        return (
            DeferredProofReplayAttemptOutcome::BlockedRemoteTimeout,
            vec![DeferredProofReplayBlocker::RchRemoteTimeout],
        );
    }
    if contains_stuck_detector_cancellation(lower_output) {
        return (
            DeferredProofReplayAttemptOutcome::BlockedStuckDetectorCancelled,
            vec![DeferredProofReplayBlocker::RchStuckDetectorCancelled],
        );
    }
    if !remote_cargo_reached {
        return (
            DeferredProofReplayAttemptOutcome::BlockedRemoteNotConfirmed,
            vec![DeferredProofReplayBlocker::RchRemoteNotConfirmed],
        );
    }
    if exit_status == Some(0) {
        return (
            DeferredProofReplayAttemptOutcome::RemoteProofPassed,
            Vec::new(),
        );
    }
    (
        DeferredProofReplayAttemptOutcome::RemoteProofFailed,
        Vec::new(),
    )
}

fn contains_local_fallback(lower_output: &str) -> bool {
    lower_output.contains("[rch] local")
        || lower_output.contains("running locally")
        || lower_output.contains("local fallback")
}

fn contains_worker_null(lower_output: &str) -> bool {
    lower_output.contains("worker=null")
        || lower_output.contains("\"worker\":null")
        || lower_output.contains("\"worker\": null")
}

fn contains_topology_preflight_failure(lower_output: &str) -> bool {
    lower_output.contains("topology preflight")
        || lower_output.contains("topology_preflight")
        || lower_output.contains("ln: already exists")
}

fn contains_remote_timeout(lower_output: &str) -> bool {
    lower_output.contains("[rch-e104]")
        || lower_output.contains("ssh command timed out")
        || lower_output.contains("command timed out after")
}

fn contains_stuck_detector_cancellation(lower_output: &str) -> bool {
    lower_output.contains("stuck_detector")
        || lower_output.contains("stuck detector")
        || lower_output.contains("cancelled by stuck")
        || lower_output.contains("canceled by stuck")
}

fn selected_worker_from_text(text: &str) -> Option<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric())
        .find(|token| {
            token.strip_prefix("vmi").is_some_and(|digits| {
                !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
            })
        })
        .map(ToString::to_string)
}

fn rch_job_from_text(text: &str) -> Option<String> {
    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .find(|token| {
            token.strip_prefix("j-").is_some_and(|digits| {
                !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit())
            })
        })
        .map(ToString::to_string)
}

fn note_for_decision(decision: DeferredProofReplayDecision) -> &'static str {
    match decision {
        DeferredProofReplayDecision::RunStaticNow => {
            "Runnable now: static verifier needs no RCH worker."
        }
        DeferredProofReplayDecision::WouldRunRemote => {
            "Eligible remote replay; live runner may execute exactly one remote RCH command."
        }
        DeferredProofReplayDecision::DeferRemoteBlocked => {
            "RCH admission blocked at capture; defer until remote RCH recovers."
        }
        DeferredProofReplayDecision::DeferDirtyOverlap => {
            "Dirty-tree overlap at capture; resolve ownership before replay."
        }
        DeferredProofReplayDecision::DeferPrerequisite => {
            "Prerequisite bead still open; complete it before replay."
        }
        DeferredProofReplayDecision::Cancelled => {
            "Operator cancelled this replay; never auto-queue."
        }
        DeferredProofReplayDecision::RejectStaleCommand => {
            "Non-canonical command shape; refresh before any replay."
        }
        DeferredProofReplayDecision::RejectNonRemoteCommand => {
            "Command is not remote-only; local fallback is reachable so it is never proof."
        }
        DeferredProofReplayDecision::RequestTriage => "Ambiguous receipt; request human triage.",
    }
}

fn note_for_attempt(outcome: DeferredProofReplayAttemptOutcome) -> &'static str {
    match outcome {
        DeferredProofReplayAttemptOutcome::RemoteProofPassed => {
            "Remote RCH worker reached Cargo/test and exited successfully."
        }
        DeferredProofReplayAttemptOutcome::RemoteProofFailed => {
            "Remote RCH worker reached Cargo/test but the proof command failed."
        }
        DeferredProofReplayAttemptOutcome::BlockedLocalFallback => {
            "RCH attempted a local fallback; no proof was minted."
        }
        DeferredProofReplayAttemptOutcome::BlockedWorkerNull => {
            "RCH reported worker=null before remote proof execution."
        }
        DeferredProofReplayAttemptOutcome::BlockedNoAdmissibleWorkers => {
            "RCH reported no admissible workers."
        }
        DeferredProofReplayAttemptOutcome::BlockedExit143 => {
            "RCH proof lane exited 143 before confirmed remote Cargo execution."
        }
        DeferredProofReplayAttemptOutcome::BlockedRemoteTimeout => {
            "RCH selected a remote worker and reached Cargo/test, but the remote command timed out before a terminal proof result."
        }
        DeferredProofReplayAttemptOutcome::BlockedStuckDetectorCancelled => {
            "RCH selected a remote worker/job but the stuck detector cancelled the proof lane before a terminal Cargo result."
        }
        DeferredProofReplayAttemptOutcome::BlockedTopologyPreflight => {
            "RCH selected a worker but failed remote topology preflight before Cargo/test."
        }
        DeferredProofReplayAttemptOutcome::BlockedRemoteNotConfirmed => {
            "Output did not prove that remote Cargo/test execution was reached."
        }
    }
}

fn source_receipt_digest(receipt: &DeferredProofReceipt) -> String {
    let bytes =
        serde_json::to_vec(receipt).unwrap_or_else(|_| receipt.receipt_id.as_bytes().to_vec());
    format!("sha256:{}", sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0f));
    }
    out
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble & 0x0f {
        value @ 0..=9 => (b'0' + value) as char,
        value => (b'a' + (value - 10)) as char,
    }
}

fn safe_target_dir(target: &str) -> bool {
    let Some(rest) = target.strip_prefix("/tmp/ft-") else {
        return false;
    };
    !rest.is_empty()
        && !rest.contains('/')
        && rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

fn safe_file_stem(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "receipt".to_string()
    } else {
        sanitized
    }
}

fn path_to_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proof_intent::{
        ProofIntent, ProofIntentEnvVar, ProofIntentQueueEntry, ProofKind, ProofRedactionPolicy,
        ProofScope,
    };

    fn env(name: &str, value: &str) -> DeferredProofEnvVar {
        DeferredProofEnvVar {
            name: name.to_string(),
            value: value.to_string(),
        }
    }

    fn remote_receipt(bead_id: &str) -> DeferredProofReceipt {
        DeferredProofReceipt {
            bead_id: bead_id.to_string(),
            receipt_id: format!("{bead_id}:comment-1"),
            command: DeferredProofCommand {
                command_shape_version: SHAPE_RCH_NO_SELF_HEALING.to_string(),
                argv: vec![
                    "rch".to_string(),
                    "--no-self-healing".to_string(),
                    "exec".to_string(),
                    "--".to_string(),
                    "env".to_string(),
                    format!("CARGO_TARGET_DIR=/tmp/{bead_id}-target"),
                    "cargo".to_string(),
                    "test".to_string(),
                    "-p".to_string(),
                    "frankenterm-core".to_string(),
                    "deferred_proof_replay".to_string(),
                ],
                env: vec![
                    env("RCH_REQUIRE_REMOTE", "1"),
                    env("RCH_NO_SELF_HEALING", "1"),
                ],
                target_dir: Some(format!("/tmp/{bead_id}-target")),
                material_remote_required: true,
                ..Default::default()
            },
            proof: DeferredProofProof {
                material_cargo_required: true,
                evidence_classification: Some("blocked_infra".to_string()),
                ..Default::default()
            },
            coordination: DeferredProofCoordination {
                rch_admission_state: Some("admitted".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn static_receipt(bead_id: &str) -> DeferredProofReceipt {
        DeferredProofReceipt {
            bead_id: bead_id.to_string(),
            receipt_id: format!("{bead_id}:comment-2"),
            command: DeferredProofCommand {
                command_shape_version: SHAPE_STATIC_VERIFIER.to_string(),
                argv: vec![
                    "bash".to_string(),
                    "tests/e2e/test_deferred_proof_receipt_contract.sh".to_string(),
                ],
                material_remote_required: false,
                ..Default::default()
            },
            proof: DeferredProofProof {
                material_cargo_required: false,
                evidence_classification: Some("static_only_clean".to_string()),
                ..Default::default()
            },
            coordination: DeferredProofCoordination {
                rch_admission_state: Some("not_required".to_string()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn queued_intent_entry(admission_state: &str) -> ProofIntentQueueEntry {
        let intent = ProofIntent::new(
            "RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-queued-proof-target cargo test -p frankenterm-core --lib proof_intent",
            ProofScope::Package {
                package: "frankenterm-core".to_string(),
            },
            ProofKind::Test,
            "sha256:queued-tree",
            None,
            true,
            Some("ft-queued".to_string()),
            None,
            ProofRedactionPolicy::Standard,
            1_704_000_000_000,
        );
        ProofIntentQueueEntry::new(
            intent,
            vec![
                "rch".to_string(),
                "--no-self-healing".to_string(),
                "exec".to_string(),
                "--".to_string(),
                "env".to_string(),
                "CARGO_TARGET_DIR=/tmp/ft-queued-proof-target".to_string(),
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "frankenterm-core".to_string(),
                "--lib".to_string(),
                "proof_intent".to_string(),
            ],
            vec![
                ProofIntentEnvVar {
                    name: "RCH_REQUIRE_REMOTE".to_string(),
                    value: "1".to_string(),
                },
                ProofIntentEnvVar {
                    name: "RCH_NO_SELF_HEALING".to_string(),
                    value: "1".to_string(),
                },
            ],
            Some("/tmp/ft-queued-proof-target".to_string()),
            admission_state,
            1_704_000_000_000,
        )
    }

    #[derive(Debug, Clone)]
    struct FakeExecutor {
        output: DeferredProofProcessOutput,
    }

    impl DeferredProofCommandExecutor for FakeExecutor {
        fn run(
            &self,
            _invocation: &DeferredProofCommandInvocation,
        ) -> io::Result<DeferredProofProcessOutput> {
            Ok(self.output.clone())
        }
    }

    #[test]
    fn std_executor_rejects_non_rch_program_before_spawn() {
        let invocation = DeferredProofCommandInvocation {
            argv: vec![
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "frankenterm-core".to_string(),
            ],
            env: Vec::new(),
        };

        let err = StdDeferredProofCommandExecutor
            .run(&invocation)
            .expect_err("non-rch executable is rejected before spawn");

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn dry_run_prefers_static_over_remote_candidate() {
        let remote = remote_receipt("ft-rdy1");
        let static_one = static_receipt("ft-stat1");
        let runner = DeferredProofReplayRunner::new(FakeExecutor {
            output: DeferredProofProcessOutput {
                exit_status: Some(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        });

        let records = runner.explain(&[remote, static_one]);
        let next = select_next_candidate(&records).expect("static candidate selected");

        assert_eq!(next.bead_id, "ft-stat1");
        assert_eq!(next.decision, DeferredProofReplayDecision::RunStaticNow);
        assert!(next.replay_allowed_now);
        assert!(next.remote_exit_status.is_none());
    }

    #[test]
    fn local_cargo_shape_is_rejected_even_when_forged_eligible() {
        let mut receipt = remote_receipt("ft-local1");
        receipt.command.argv = vec![
            "cargo".to_string(),
            "test".to_string(),
            "-p".to_string(),
            "frankenterm-core".to_string(),
        ];

        let record = decision_record(&receipt);

        assert_eq!(
            record.decision,
            DeferredProofReplayDecision::RejectNonRemoteCommand
        );
        assert_eq!(record.blockers, vec!["command.not_remote_only"]);
        assert!(!record.replay_allowed_now);
    }

    #[test]
    fn topology_preflight_keeps_selected_worker_and_distinct_blocker() {
        let mut receipt = remote_receipt("ft-topo1");
        receipt.coordination.rch_admission_state = Some("topology_preflight_failed".to_string());
        receipt.coordination.selected_worker = Some("vmi1227854".to_string());

        let record = decision_record(&receipt);

        assert_eq!(
            record.decision,
            DeferredProofReplayDecision::DeferRemoteBlocked
        );
        assert_eq!(record.blockers, vec!["rch.topology_preflight_failed"]);
        assert_eq!(record.selected_worker.as_deref(), Some("vmi1227854"));
    }

    #[test]
    fn live_attempt_blocks_local_fallback_instead_of_minting_proof() {
        let receipt = remote_receipt("ft-lf1");
        let output = DeferredProofProcessOutput {
            exit_status: Some(0),
            stdout: b"[RCH] local fallback refused\nworker=null\n".to_vec(),
            stderr: Vec::new(),
        };

        let record = build_live_attempt_record(&receipt, &output);

        assert_eq!(
            record.outcome,
            DeferredProofReplayAttemptOutcome::BlockedLocalFallback
        );
        assert!(record.local_fallback_detected);
        assert!(!record.remote_cargo_reached);
        assert_eq!(record.blockers, vec!["rch.local_fallback"]);
    }

    #[test]
    fn queued_intent_receipt_respects_admission_state() {
        let deferred = queued_intent_entry("wait_rch");
        let deferred_receipt = deferred_receipt_from_proof_intent_queue_entry(&deferred, None);
        let deferred_record = decision_record(&deferred_receipt);
        assert_eq!(
            deferred_record.decision,
            DeferredProofReplayDecision::DeferRemoteBlocked
        );
        assert_eq!(deferred_record.blockers, vec!["rch.worker_pressure"]);

        let admitted_receipt =
            deferred_receipt_from_proof_intent_queue_entry(&deferred, Some("admitted"));
        let admitted_record = decision_record(&admitted_receipt);
        assert_eq!(
            admitted_record.decision,
            DeferredProofReplayDecision::WouldRunRemote
        );
        assert!(admitted_record.blockers.is_empty());
        assert!(!admitted_record.replay_allowed_now);
    }

    #[test]
    fn live_attempt_passes_only_with_confirmed_remote_worker() {
        let receipt = remote_receipt("ft-ok1");
        let output = DeferredProofProcessOutput {
            exit_status: Some(0),
            stdout: b"job j-29862592113541150 selected worker vmi1149989\ncargo test ok\n".to_vec(),
            stderr: Vec::new(),
        };

        let record = build_live_attempt_record(&receipt, &output);

        assert_eq!(
            record.outcome,
            DeferredProofReplayAttemptOutcome::RemoteProofPassed
        );
        assert_eq!(record.selected_worker.as_deref(), Some("vmi1149989"));
        assert_eq!(record.rch_job_id.as_deref(), Some("j-29862592113541150"));
        assert!(record.remote_cargo_reached);
        assert!(record.blockers.is_empty());
        assert!(record.source_receipt_digest.starts_with("sha256:"));
    }

    #[test]
    fn live_attempt_blocks_remote_timeout_without_code_failure() {
        let receipt = remote_receipt("ft-timeout1");
        let output = DeferredProofProcessOutput {
            exit_status: Some(124),
            stdout: b"[RCH] remote vmi1293453 failed [RCH-E104] SSH command timed out after 1800s\njob j-29871232832766070 reached remote Cargo\n".to_vec(),
            stderr: Vec::new(),
        };

        let record = build_live_attempt_record(&receipt, &output);

        assert_eq!(
            record.outcome,
            DeferredProofReplayAttemptOutcome::BlockedRemoteTimeout
        );
        assert_eq!(record.selected_worker.as_deref(), Some("vmi1293453"));
        assert_eq!(record.rch_job_id.as_deref(), Some("j-29871232832766070"));
        assert!(record.remote_cargo_reached);
        assert!(!record.local_fallback_detected);
        assert_eq!(record.blockers, vec!["rch.remote_timeout"]);
    }

    #[test]
    fn live_attempt_blocks_stuck_detector_cancellation_without_code_failure() {
        let receipt = remote_receipt("ft-stuck1");
        let output = DeferredProofProcessOutput {
            exit_status: Some(130),
            stdout: br#"[W] Worker: vmi1152480 | Job: j-29884604911452440
{"cancellation":{"origin":"stuck_detector","reason_code":"stuck_detector"}}
build cancelled by stuck_detector before terminal Cargo result
"#
            .to_vec(),
            stderr: Vec::new(),
        };

        let record = build_live_attempt_record(&receipt, &output);

        assert_eq!(
            record.outcome,
            DeferredProofReplayAttemptOutcome::BlockedStuckDetectorCancelled
        );
        assert_eq!(record.selected_worker.as_deref(), Some("vmi1152480"));
        assert_eq!(record.rch_job_id.as_deref(), Some("j-29884604911452440"));
        assert!(record.remote_cargo_reached);
        assert!(!record.local_fallback_detected);
        assert_eq!(record.blockers, vec!["rch.stuck_detector_cancelled"]);
    }
}
