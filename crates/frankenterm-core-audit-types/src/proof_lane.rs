#![allow(clippy::module_name_repetitions)]
#![allow(clippy::struct_excessive_bools)]

//! Proof-lane evidence records and report summaries for `ft-tn6cw.3`.
//!
//! The types in this module are intentionally leaf-clean DTOs. They make proof
//! closeout explicit enough that sync logs, worker selection, local fallbacks,
//! and real remote Cargo results cannot be collapsed into the same operator
//! claim.
//!
//! Proof-doctor verdicts may be attached as compact snapshots, but they do not
//! replace the ledger's pass/fail invariants. The snapshot lets release reports
//! explain why an attempt is dirty-tree blocked or inconclusive using the same
//! taxonomy operators saw at runtime, while `PASS` still requires retained
//! remote Cargo/rustc/test evidence on the ledger record itself.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::proof_doctor::{
    ProofDoctorBlockerKind, ProofDoctorPhase, ProofDoctorStatus, ProofDoctorToolVersionState,
    ProofDoctorVerdict,
};

/// Proof-lane ledger schema version implemented by this module.
pub const PROOF_LANE_SCHEMA_VERSION: u32 = 2;

/// Terminal and intermediate proof states from the `ft-tn6cw.2` contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ProofState {
    /// Required proof has not been attempted.
    NotRun,
    /// Remote Cargo or rustc started, but no terminal verdict exists yet.
    ReachedRemoteCargo,
    /// Remote Cargo or rustc found a source, config, feature, or lint error.
    SourceCompileFail,
    /// Remote test, bench, or E2E assertions failed.
    TestFail,
    /// Required proof completed successfully for the claimed scope.
    Pass,
    /// Infrastructure failed before Cargo or rustc started.
    InfraBlockedPreCargo,
    /// Cargo or rustc started, but infrastructure prevented complete proof.
    InfraBlockedPostCargo,
    /// Local or off-policy execution was offered as remote proof.
    LocalInvalid,
    /// Proof was intentionally skipped and must not be promoted to proven.
    SkippedNotProven,
    /// Evidence is missing, contradictory, or too incomplete to classify.
    Inconclusive,
}

impl ProofState {
    /// Return true when no later state is expected for this attempt.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::NotRun | Self::ReachedRemoteCargo)
    }

    /// Return true when this state is a valid source verdict.
    #[must_use]
    pub const fn has_source_verdict(self) -> bool {
        matches!(self, Self::SourceCompileFail | Self::TestFail | Self::Pass)
    }

    /// Return true when this state can support source-bead closure.
    #[must_use]
    pub const fn can_support_source_closeout(self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Stable report bucket for operator summaries.
    #[must_use]
    pub const fn report_bucket(self) -> ProofReportBucket {
        match self {
            Self::SourceCompileFail | Self::TestFail => ProofReportBucket::SourceRed,
            Self::Pass => ProofReportBucket::RemoteProofPassed,
            Self::InfraBlockedPreCargo => ProofReportBucket::PreCargoInfrastructureBlocker,
            Self::InfraBlockedPostCargo => ProofReportBucket::PostCargoInfrastructureBlocker,
            Self::LocalInvalid => ProofReportBucket::InvalidLocalProof,
            Self::SkippedNotProven => ProofReportBucket::SkippedNotProven,
            Self::Inconclusive => ProofReportBucket::InconclusiveEvidence,
            Self::NotRun | Self::ReachedRemoteCargo => ProofReportBucket::MissingEvidence,
        }
    }
}

/// Proof scope for a ledger record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofScope {
    /// Static documentation or diff-only proof.
    DocsStatic,
    /// Cargo check lane.
    CargoCheck,
    /// Cargo clippy lane.
    CargoClippy,
    /// Cargo test lane.
    CargoTest,
    /// Cargo bench lane.
    CargoBench,
    /// End-to-end harness lane.
    E2e,
    /// Release gate or closeout harness.
    ReleaseGate,
    /// Target hardware or high-scale swarm proof lane.
    HighScale,
}

/// Backend required or observed for a proof record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofBackend {
    /// RCH remote execution.
    Rch,
    /// Local shell or local tool execution.
    LocalShell,
    /// CI-managed execution.
    Ci,
    /// No execution backend was required.
    None,
    /// Backend could not be determined from evidence.
    Unknown,
}

/// Artifact retrieval state for a proof attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRetrievalStatus {
    /// No artifact retrieval applies to this lane.
    NotApplicable,
    /// Artifact retrieval never started.
    NotStarted,
    /// Required artifacts were retained.
    Complete,
    /// Some evidence was retained, but the bundle is incomplete.
    Partial,
    /// Retrieval stalled after material execution.
    Stalled,
    /// Retrieval failed.
    Failed,
}

/// Hardware predicate attached to high-scale proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofHardwarePredicate {
    /// 64-core / 256 GiB predicate was met.
    ProvenPredicateMet,
    /// Predicate was not met; high-scale claim is skipped, not proven.
    SkippedNotProven,
    /// Reduced remote proof only.
    RemoteReduced,
    /// Reduced local proof only.
    LocalReduced,
    /// Predicate is unknown or not recorded.
    Unknown,
}

/// Redaction status for ledger artifacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofRedactionStatus {
    /// No sensitive data was present.
    NoneNeeded,
    /// Data was redacted before retention.
    Redacted,
    /// Required redaction evidence is missing.
    UnsafeMissing,
    /// Redaction status is unknown.
    Unknown,
}

/// Stable operator grouping for reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofReportBucket {
    /// Source compile/test failures.
    SourceRed,
    /// Valid remote proof passed.
    RemoteProofPassed,
    /// RCH or environment blocked proof before Cargo.
    PreCargoInfrastructureBlocker,
    /// Infrastructure blocked proof after Cargo started.
    PostCargoInfrastructureBlocker,
    /// Dirty tree or active ownership blocked the proof lane.
    DirtyTreeBlocked,
    /// Local/off-policy proof attempt.
    InvalidLocalProof,
    /// Intentional skip that must not be promoted to proven.
    SkippedNotProven,
    /// Retained evidence cannot support a stronger classification.
    InconclusiveEvidence,
    /// Not run, in-flight, or inconclusive evidence.
    MissingEvidence,
}

impl ProofReportBucket {
    /// Stable string key for summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceRed => "source_red",
            Self::RemoteProofPassed => "remote_proof_passed",
            Self::PreCargoInfrastructureBlocker => "pre_cargo_infra_blocked",
            Self::PostCargoInfrastructureBlocker => "post_cargo_infra_blocked",
            Self::DirtyTreeBlocked => "dirty_tree_blocked",
            Self::InvalidLocalProof => "local_invalid",
            Self::SkippedNotProven => "skipped_not_proven",
            Self::InconclusiveEvidence => "inconclusive",
            Self::MissingEvidence => "missing_evidence",
        }
    }
}

/// Compact proof-doctor snapshot retained by a proof ledger record.
///
/// Release and closeout surfaces use this as a reference to the runtime
/// proof-doctor verdict instead of reclassifying the same evidence locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorLedgerSnapshot {
    /// Source proof-doctor verdict id.
    pub verdict_id: String,
    /// Source proof-doctor status.
    pub status: ProofDoctorStatus,
    /// Proof-doctor phase that produced the verdict.
    pub phase: ProofDoctorPhase,
    /// Primary proof-doctor reason code.
    pub reason_code: String,
    /// Primary blocker kind, when the verdict had a blocker.
    pub blocker_kind: Option<ProofDoctorBlockerKind>,
    /// Installed/patched/stale tool state seen by proof-doctor.
    pub tool_version_state: ProofDoctorToolVersionState,
    /// Whether proof-doctor evidence reached remote Cargo.
    pub remote_cargo_reached: bool,
    /// Redaction-safe affected paths from the primary blocker.
    pub affected_paths: Vec<String>,
    /// Operator-facing proof-doctor summary.
    pub operator_summary: String,
    /// Operator-facing next action selected by proof-doctor.
    pub next_action: String,
}

impl ProofDoctorLedgerSnapshot {
    /// Copy the release-report-safe fields from a proof-doctor verdict.
    #[must_use]
    pub fn from_verdict(verdict: &ProofDoctorVerdict) -> Self {
        let primary_blocker = verdict.blockers.first();
        let reason_code = primary_blocker.map_or_else(
            || {
                verdict.ledger_projection.as_ref().map_or_else(
                    || "proof.no_blocker".to_string(),
                    |projection| projection.reason_code.clone(),
                )
            },
            |blocker| blocker.reason_code.clone(),
        );

        Self {
            verdict_id: verdict.verdict_id.clone(),
            status: verdict.status,
            phase: verdict.phase,
            reason_code,
            blocker_kind: primary_blocker.map(|blocker| blocker.blocker_kind),
            tool_version_state: verdict.evidence.tool_version_state,
            remote_cargo_reached: verdict.evidence.remote_cargo_reached,
            affected_paths: primary_blocker
                .map_or_else(Vec::new, |blocker| blocker.affected_paths.clone()),
            operator_summary: verdict.operator_summary.clone(),
            next_action: verdict.next_action.message.clone(),
        }
    }

    /// Report bucket implied by proof-doctor, falling back to the ledger state
    /// for advisory runnable verdicts.
    #[must_use]
    pub const fn report_bucket(&self, fallback: ProofReportBucket) -> ProofReportBucket {
        match self.status {
            ProofDoctorStatus::Runnable => fallback,
            ProofDoctorStatus::Passed => ProofReportBucket::RemoteProofPassed,
            ProofDoctorStatus::SourceBlocked | ProofDoctorStatus::TestBlocked => {
                ProofReportBucket::SourceRed
            }
            ProofDoctorStatus::InfraBlocked => {
                if self.remote_cargo_reached {
                    ProofReportBucket::PostCargoInfrastructureBlocker
                } else {
                    ProofReportBucket::PreCargoInfrastructureBlocker
                }
            }
            ProofDoctorStatus::DirtyTreeBlocked | ProofDoctorStatus::OwnershipBlocked => {
                ProofReportBucket::DirtyTreeBlocked
            }
            ProofDoctorStatus::Invalid => ProofReportBucket::InvalidLocalProof,
            ProofDoctorStatus::SkippedNotProven => ProofReportBucket::SkippedNotProven,
            ProofDoctorStatus::Inconclusive => ProofReportBucket::InconclusiveEvidence,
        }
    }
}

/// Machine-readable record for one material proof attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofAttemptRecord {
    /// Record schema version.
    pub schema_version: u32,
    /// Stable id or hash for this attempt.
    pub proof_id: String,
    /// Owning Beads issue id.
    pub bead_id: String,
    /// Optional parent epic or proof program.
    pub parent_bead_id: Option<String>,
    /// RFC3339 start timestamp.
    pub attempted_at_utc: String,
    /// Optional RFC3339 completion timestamp.
    pub finished_at_utc: Option<String>,
    /// Agent or operator that ran the attempt.
    pub agent_name: String,
    /// Working directory.
    pub cwd: String,
    /// Exact argv, not shell prose.
    pub command: Vec<String>,
    /// Declared `CARGO_TARGET_DIR`, when present.
    pub declared_target_dir: Option<String>,
    /// Scope being proven.
    pub proof_scope: ProofScope,
    /// Backend required by the lane.
    pub required_backend: ProofBackend,
    /// Backend observed in retained evidence.
    pub observed_backend: ProofBackend,
    /// RCH version string, when known.
    pub rch_version: Option<String>,
    /// Redaction-safe RCH config fingerprint.
    pub rch_config_fingerprint: Option<String>,
    /// Selected worker id, when known.
    pub selected_worker: Option<String>,
    /// Worker probe or status artifact path.
    pub worker_probe_artifact: Option<String>,
    /// RCH sync duration, when known.
    pub sync_duration_ms: Option<u64>,
    /// Remote command duration, when known.
    pub remote_command_duration_ms: Option<u64>,
    /// Wrapper or harness exit code.
    pub wrapper_exit_code: Option<i32>,
    /// Remote command exit code.
    pub remote_exit_code: Option<i32>,
    /// True only when evidence shows remote Cargo/rustc started.
    pub remote_cargo_reached: bool,
    /// True when local Cargo ran or may have run through off-policy fallback.
    pub local_cargo_detected: bool,
    /// True when rustc or build execution started.
    pub rustc_reached: bool,
    /// True when test, bench, or E2E assertions started.
    pub test_binary_started: bool,
    /// Artifact retrieval state.
    pub artifact_retrieval_status: ArtifactRetrievalStatus,
    /// Terminal or intermediate proof state.
    pub state: ProofState,
    /// Stable reason code.
    pub reason_code: String,
    /// Short operator-facing interpretation.
    pub summary: String,
    /// Retained logs, reports, or fixture paths.
    pub artifact_paths: Vec<String>,
    /// High-scale or reduced-mode predicate.
    pub hardware_predicate: Option<ProofHardwarePredicate>,
    /// Explicit claims this record can support.
    pub claims_allowed: Vec<String>,
    /// Next action for the operator or agent.
    pub next_action: String,
    /// Redaction status for referenced artifacts.
    pub redaction_status: ProofRedactionStatus,
    /// Optional proof-doctor verdict snapshot that justified the handoff.
    pub proof_doctor: Option<ProofDoctorLedgerSnapshot>,
}

impl ProofAttemptRecord {
    /// Create a record with required identity and classification fields.
    #[must_use]
    pub fn new(
        proof_id: impl Into<String>,
        bead_id: impl Into<String>,
        state: ProofState,
        reason_code: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            schema_version: PROOF_LANE_SCHEMA_VERSION,
            proof_id: proof_id.into(),
            bead_id: bead_id.into(),
            parent_bead_id: None,
            attempted_at_utc: String::new(),
            finished_at_utc: None,
            agent_name: String::new(),
            cwd: String::new(),
            command: Vec::new(),
            declared_target_dir: None,
            proof_scope: ProofScope::CargoTest,
            required_backend: ProofBackend::Rch,
            observed_backend: ProofBackend::Unknown,
            rch_version: None,
            rch_config_fingerprint: None,
            selected_worker: None,
            worker_probe_artifact: None,
            sync_duration_ms: None,
            remote_command_duration_ms: None,
            wrapper_exit_code: None,
            remote_exit_code: None,
            remote_cargo_reached: false,
            local_cargo_detected: false,
            rustc_reached: false,
            test_binary_started: false,
            artifact_retrieval_status: ArtifactRetrievalStatus::NotStarted,
            state,
            reason_code: reason_code.into(),
            summary: summary.into(),
            artifact_paths: Vec::new(),
            hardware_predicate: None,
            claims_allowed: Vec::new(),
            next_action: String::new(),
            redaction_status: ProofRedactionStatus::Unknown,
            proof_doctor: None,
        }
    }

    /// Attach a compact proof-doctor verdict snapshot to this record.
    #[must_use]
    pub fn with_proof_doctor_verdict(mut self, verdict: &ProofDoctorVerdict) -> Self {
        self.proof_doctor = Some(ProofDoctorLedgerSnapshot::from_verdict(verdict));
        self
    }

    /// Return the report bucket for this record.
    #[must_use]
    pub fn report_bucket(&self) -> ProofReportBucket {
        let fallback = self.state.report_bucket();
        self.proof_doctor
            .as_ref()
            .map_or(fallback, |snapshot| snapshot.report_bucket(fallback))
    }

    /// Whether this record can support closing a source implementation bead.
    #[must_use]
    pub fn safe_to_close_source_bead(&self) -> bool {
        self.state.can_support_source_closeout()
            && !self.local_cargo_detected
            && backend_requirement_satisfied(self.required_backend, self.observed_backend)
            && (self.required_backend != ProofBackend::Rch || self.remote_cargo_reached)
            && (!proof_scope_requires_rustc(self.proof_scope) || self.rustc_reached)
            && (!proof_scope_requires_assertions(self.proof_scope) || self.test_binary_started)
            && self.artifact_retrieval_status == ArtifactRetrievalStatus::Complete
            && matches!(
                self.redaction_status,
                ProofRedactionStatus::NoneNeeded | ProofRedactionStatus::Redacted
            )
    }

    /// Return true when this record can support a high-scale proven claim.
    #[must_use]
    pub fn allows_high_scale_claim(&self) -> bool {
        self.safe_to_close_source_bead()
            && self.hardware_predicate == Some(ProofHardwarePredicate::ProvenPredicateMet)
    }

    /// Join argv for operator display.
    #[must_use]
    pub fn command_display(&self) -> String {
        self.command.join(" ")
    }
}

/// Validation severity for one record finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofFindingSeverity {
    /// The record violates truthfulness invariants.
    Error,
    /// The record is usable, but less complete than ideal.
    Warning,
}

/// Validation finding for a proof record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofLedgerFinding {
    /// Affected proof id.
    pub proof_id: String,
    /// Affected bead id.
    pub bead_id: String,
    /// Finding severity.
    pub severity: ProofFindingSeverity,
    /// Stable reason code for the finding.
    pub reason_code: String,
    /// Operator-facing message.
    pub message: String,
}

impl ProofLedgerFinding {
    #[must_use]
    fn error(record: &ProofAttemptRecord, reason_code: &str, message: &str) -> Self {
        Self {
            proof_id: record.proof_id.clone(),
            bead_id: record.bead_id.clone(),
            severity: ProofFindingSeverity::Error,
            reason_code: reason_code.to_string(),
            message: message.to_string(),
        }
    }

    #[must_use]
    fn warning(record: &ProofAttemptRecord, reason_code: &str, message: &str) -> Self {
        Self {
            proof_id: record.proof_id.clone(),
            bead_id: record.bead_id.clone(),
            severity: ProofFindingSeverity::Warning,
            reason_code: reason_code.to_string(),
            message: message.to_string(),
        }
    }
}

/// Validate one record against proof-lane truthfulness invariants.
#[must_use]
pub fn validate_proof_record(record: &ProofAttemptRecord) -> Vec<ProofLedgerFinding> {
    let mut findings = Vec::new();

    if record.schema_version != PROOF_LANE_SCHEMA_VERSION {
        findings.push(ProofLedgerFinding::error(
            record,
            "schema_version_mismatch",
            "proof record schema_version does not match the proof-lane schema",
        ));
    }

    if record.proof_id.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_proof_id",
            "proof record must carry a stable proof_id",
        ));
    }

    if record.bead_id.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_bead_id",
            "proof record must carry the owning bead_id",
        ));
    }

    if record.reason_code.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_reason_code",
            "proof record must carry a stable reason_code",
        ));
    }

    if record.summary.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_summary",
            "proof record must carry an operator-facing summary",
        ));
    }

    if record.state != ProofState::NotRun && record.attempted_at_utc.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_attempt_timestamp",
            "attempted proof records must carry attempted_at_utc",
        ));
    }

    if record.state.is_terminal()
        && !record
            .finished_at_utc
            .as_deref()
            .is_some_and(|timestamp| !timestamp.trim().is_empty())
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_finish_timestamp",
            "terminal proof records must carry finished_at_utc",
        ));
    }

    if record.required_backend != ProofBackend::None && record.command.is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_command",
            "proof records that require execution must retain the exact argv",
        ));
    }

    if record.state.has_source_verdict()
        && !backend_requirement_satisfied(record.required_backend, record.observed_backend)
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "source_verdict_backend_mismatch",
            "source/test verdict backend must match the lane's required backend",
        ));
    }

    if record.state == ProofState::Pass && record.local_cargo_detected {
        findings.push(ProofLedgerFinding::error(
            record,
            "pass_with_local_cargo_detected",
            "PASS cannot be claimed when local Cargo or fail-open execution was detected",
        ));
    }

    if record.state == ProofState::Pass
        && record.required_backend == ProofBackend::Rch
        && !record.remote_cargo_reached
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "pass_without_remote_cargo",
            "RCH-required PASS needs positive remote Cargo evidence",
        ));
    }

    if matches!(
        record.state,
        ProofState::SourceCompileFail | ProofState::TestFail
    ) && record.required_backend == ProofBackend::Rch
        && !record.remote_cargo_reached
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "source_verdict_without_remote_cargo",
            "source/test verdict needs positive remote Cargo evidence",
        ));
    }

    if record.state == ProofState::Pass
        && proof_scope_requires_rustc(record.proof_scope)
        && !record.rustc_reached
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "pass_without_rustc",
            "PASS for a Rust proof lane requires positive rustc/build execution evidence",
        ));
    }

    if record.state == ProofState::Pass
        && proof_scope_requires_assertions(record.proof_scope)
        && !record.test_binary_started
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "pass_without_assertion_execution",
            "PASS for a test, bench, E2E, release, or high-scale lane requires assertion execution evidence",
        ));
    }

    if record.state == ProofState::Pass
        && record.redaction_status == ProofRedactionStatus::UnsafeMissing
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "pass_with_unsafe_redaction",
            "PASS cannot support closeout when required artifact redaction evidence is missing",
        ));
    }

    if record.state == ProofState::Pass && record.redaction_status == ProofRedactionStatus::Unknown
    {
        findings.push(ProofLedgerFinding::warning(
            record,
            "pass_with_unknown_redaction",
            "PASS record does not state whether retained artifacts needed redaction",
        ));
    }

    if record.state == ProofState::InfraBlockedPreCargo && record.remote_cargo_reached {
        findings.push(ProofLedgerFinding::error(
            record,
            "pre_cargo_blocker_reached_remote_cargo",
            "pre-Cargo blocker cannot also claim remote Cargo was reached",
        ));
    }

    if record.state == ProofState::LocalInvalid && record.claims_allowed.iter().any(is_proven_claim)
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "local_invalid_allows_proven_claim",
            "LOCAL_INVALID records cannot allow proven or passed claims",
        ));
    }

    if record.state == ProofState::SkippedNotProven
        && record.claims_allowed.iter().any(is_proven_claim)
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "skipped_not_proven_allows_proven_claim",
            "SKIPPED_NOT_PROVEN records cannot allow proven or passed claims",
        ));
    }

    if record.state == ProofState::Pass
        && record.artifact_retrieval_status != ArtifactRetrievalStatus::Complete
    {
        findings.push(ProofLedgerFinding::warning(
            record,
            "pass_with_incomplete_artifacts",
            "PASS record does not have complete artifact retrieval",
        ));
    }

    validate_proof_doctor_snapshot(record, &mut findings);

    findings
}

fn validate_proof_doctor_snapshot(
    record: &ProofAttemptRecord,
    findings: &mut Vec<ProofLedgerFinding>,
) {
    let Some(snapshot) = &record.proof_doctor else {
        return;
    };

    if snapshot.verdict_id.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_proof_doctor_verdict_id",
            "proof-doctor snapshots must retain the source verdict id",
        ));
    }

    if snapshot.reason_code.trim().is_empty() {
        findings.push(ProofLedgerFinding::error(
            record,
            "missing_proof_doctor_reason_code",
            "proof-doctor snapshots must retain the source reason code",
        ));
    }

    if !proof_doctor_status_matches_state(snapshot.status, record.state) {
        findings.push(ProofLedgerFinding::error(
            record,
            "proof_doctor_status_state_mismatch",
            "proof-doctor snapshot status does not match the ledger proof state",
        ));
    }
}

fn proof_doctor_status_matches_state(status: ProofDoctorStatus, state: ProofState) -> bool {
    match status {
        ProofDoctorStatus::Runnable => {
            matches!(state, ProofState::NotRun | ProofState::ReachedRemoteCargo)
        }
        ProofDoctorStatus::Passed => state == ProofState::Pass,
        ProofDoctorStatus::SourceBlocked => state == ProofState::SourceCompileFail,
        ProofDoctorStatus::TestBlocked => state == ProofState::TestFail,
        ProofDoctorStatus::InfraBlocked => matches!(
            state,
            ProofState::InfraBlockedPreCargo | ProofState::InfraBlockedPostCargo
        ),
        ProofDoctorStatus::DirtyTreeBlocked
        | ProofDoctorStatus::OwnershipBlocked
        | ProofDoctorStatus::Inconclusive => state == ProofState::Inconclusive,
        ProofDoctorStatus::Invalid => state == ProofState::LocalInvalid,
        ProofDoctorStatus::SkippedNotProven => state == ProofState::SkippedNotProven,
    }
}

fn is_proven_claim(claim: &String) -> bool {
    let normalized = claim.to_ascii_lowercase();
    normalized.contains("proven") || normalized.contains("passed") || normalized.contains("green")
}

fn backend_requirement_satisfied(required: ProofBackend, observed: ProofBackend) -> bool {
    match required {
        ProofBackend::None => matches!(observed, ProofBackend::None | ProofBackend::Unknown),
        ProofBackend::Unknown => true,
        _ => observed == required,
    }
}

fn proof_scope_requires_rustc(scope: ProofScope) -> bool {
    matches!(
        scope,
        ProofScope::CargoCheck
            | ProofScope::CargoClippy
            | ProofScope::CargoTest
            | ProofScope::CargoBench
            | ProofScope::ReleaseGate
            | ProofScope::HighScale
    )
}

fn proof_scope_requires_assertions(scope: ProofScope) -> bool {
    matches!(
        scope,
        ProofScope::CargoTest
            | ProofScope::CargoBench
            | ProofScope::E2e
            | ProofScope::ReleaseGate
            | ProofScope::HighScale
    )
}

/// Per-bead summary row for proof reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofBeadSummary {
    /// Bead id.
    pub bead_id: String,
    /// Latest or most severe proof state seen for the bead.
    pub state: ProofState,
    /// Report bucket for the selected state.
    pub bucket: ProofReportBucket,
    /// Reason code from the selected record.
    pub reason_code: String,
    /// Operator-facing summary from the selected record.
    pub summary: String,
    /// Next action from the selected record.
    pub next_action: String,
    /// Proof-doctor snapshot for operator-facing blocker display.
    pub proof_doctor: Option<ProofDoctorLedgerSnapshot>,
}

/// Aggregate proof-lane report for operator and Beads closeout surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofLaneReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Number of records summarized.
    pub total_records: u64,
    /// Counts by report bucket key.
    pub by_bucket: BTreeMap<String, u64>,
    /// Counts by proof state.
    pub by_state: BTreeMap<ProofState, u64>,
    /// Counts by reason code.
    pub by_reason_code: BTreeMap<String, u64>,
    /// Counts by attached proof-doctor status.
    pub by_proof_doctor_status: BTreeMap<ProofDoctorStatus, u64>,
    /// Per-bead selected summaries.
    pub beads: Vec<ProofBeadSummary>,
    /// Validation findings.
    pub findings: Vec<ProofLedgerFinding>,
    /// Concise operator summary.
    pub operator_summary: String,
}

impl ProofLaneReport {
    /// Build an aggregate report from proof records.
    #[must_use]
    pub fn from_records(records: &[ProofAttemptRecord]) -> Self {
        let mut by_bucket = BTreeMap::new();
        let mut by_state = BTreeMap::new();
        let mut by_reason_code = BTreeMap::new();
        let mut by_proof_doctor_status = BTreeMap::new();
        let mut by_bead = BTreeMap::<String, &ProofAttemptRecord>::new();
        let mut findings = Vec::new();

        for record in records {
            *by_bucket
                .entry(record.report_bucket().as_str().to_string())
                .or_insert(0) += 1;
            *by_state.entry(record.state).or_insert(0) += 1;
            *by_reason_code
                .entry(record.reason_code.clone())
                .or_insert(0) += 1;
            if let Some(snapshot) = &record.proof_doctor {
                *by_proof_doctor_status.entry(snapshot.status).or_insert(0) += 1;
            }
            findings.extend(validate_proof_record(record));

            by_bead
                .entry(record.bead_id.clone())
                .and_modify(|selected| {
                    if proof_record_rank(record) > proof_record_rank(selected) {
                        *selected = record;
                    }
                })
                .or_insert(record);
        }

        let beads = by_bead
            .into_iter()
            .map(|(bead_id, record)| ProofBeadSummary {
                bead_id,
                state: record.state,
                bucket: record.report_bucket(),
                reason_code: record.reason_code.clone(),
                summary: record.summary.clone(),
                next_action: record.next_action.clone(),
                proof_doctor: record.proof_doctor.clone(),
            })
            .collect::<Vec<_>>();

        let operator_summary = operator_summary(records.len(), &by_bucket, findings.len());

        Self {
            schema_version: PROOF_LANE_SCHEMA_VERSION,
            total_records: records.len() as u64,
            by_bucket,
            by_state,
            by_reason_code,
            by_proof_doctor_status,
            beads,
            findings,
            operator_summary,
        }
    }

    /// Count records in a bucket.
    #[must_use]
    pub fn bucket_count(&self, bucket: ProofReportBucket) -> u64 {
        self.by_bucket.get(bucket.as_str()).copied().unwrap_or(0)
    }

    /// Whether any validation finding is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == ProofFindingSeverity::Error)
    }
}

fn proof_state_rank(state: ProofState) -> u8 {
    match state {
        ProofState::LocalInvalid => 90,
        ProofState::InfraBlockedPreCargo => 80,
        ProofState::InfraBlockedPostCargo => 70,
        ProofState::SourceCompileFail | ProofState::TestFail => 60,
        ProofState::Pass => 50,
        ProofState::SkippedNotProven => 40,
        ProofState::Inconclusive => 30,
        ProofState::ReachedRemoteCargo => 20,
        ProofState::NotRun => 10,
    }
}

fn proof_record_rank(record: &ProofAttemptRecord) -> u8 {
    match record.report_bucket() {
        ProofReportBucket::InvalidLocalProof => 90,
        ProofReportBucket::DirtyTreeBlocked => 85,
        ProofReportBucket::PreCargoInfrastructureBlocker => 80,
        ProofReportBucket::PostCargoInfrastructureBlocker => 70,
        ProofReportBucket::SourceRed => 60,
        ProofReportBucket::RemoteProofPassed => 50,
        ProofReportBucket::SkippedNotProven => 40,
        ProofReportBucket::InconclusiveEvidence => 30,
        ProofReportBucket::MissingEvidence => proof_state_rank(record.state),
    }
}

fn operator_summary(
    total_records: usize,
    by_bucket: &BTreeMap<String, u64>,
    finding_count: usize,
) -> String {
    let pass = by_bucket
        .get(ProofReportBucket::RemoteProofPassed.as_str())
        .copied()
        .unwrap_or(0);
    let source_red = by_bucket
        .get(ProofReportBucket::SourceRed.as_str())
        .copied()
        .unwrap_or(0);
    let pre_cargo = by_bucket
        .get(ProofReportBucket::PreCargoInfrastructureBlocker.as_str())
        .copied()
        .unwrap_or(0);
    let post_cargo = by_bucket
        .get(ProofReportBucket::PostCargoInfrastructureBlocker.as_str())
        .copied()
        .unwrap_or(0);
    let dirty_tree = by_bucket
        .get(ProofReportBucket::DirtyTreeBlocked.as_str())
        .copied()
        .unwrap_or(0);
    let local_invalid = by_bucket
        .get(ProofReportBucket::InvalidLocalProof.as_str())
        .copied()
        .unwrap_or(0);
    let skipped = by_bucket
        .get(ProofReportBucket::SkippedNotProven.as_str())
        .copied()
        .unwrap_or(0);
    let missing = by_bucket
        .get(ProofReportBucket::MissingEvidence.as_str())
        .copied()
        .unwrap_or(0);
    let inconclusive = by_bucket
        .get(ProofReportBucket::InconclusiveEvidence.as_str())
        .copied()
        .unwrap_or(0);

    format!(
        "records={total_records}; pass={pass}; source_red={source_red}; \
         pre_cargo_blocked={pre_cargo}; post_cargo_blocked={post_cargo}; \
         dirty_tree_blocked={dirty_tree}; local_invalid={local_invalid}; \
         skipped_not_proven={skipped}; inconclusive={inconclusive}; \
         missing_evidence={missing}; findings={finding_count}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::proof_doctor::{
        ProofDoctorDirtyPath, ProofDoctorEvidence, ProofDoctorOwner, ProofDoctorPreflightInput,
        classify_proof_doctor,
    };

    fn base_record(state: ProofState) -> ProofAttemptRecord {
        let mut record = ProofAttemptRecord::new("proof-1", "ft-test", state, "reason", "summary");
        record.attempted_at_utc = "2026-05-05T00:00:00Z".into();
        if state.is_terminal() {
            record.finished_at_utc = Some("2026-05-05T00:00:01Z".into());
        }
        record.agent_name = "OliveChapel".into();
        record.cwd = "/Users/jemanuel/projects/frankenterm".into();
        record.command = vec![
            "rch".into(),
            "exec".into(),
            "--".into(),
            "cargo".into(),
            "test".into(),
        ];
        record.declared_target_dir = Some("/tmp/ft-test-target".into());
        record.observed_backend = ProofBackend::Rch;
        record.artifact_paths = vec!["tests/e2e/artifacts/proof/ft-test/summary.json".into()];
        record.next_action = "review report".into();
        record.redaction_status = ProofRedactionStatus::NoneNeeded;
        record
    }

    fn base_doctor_input() -> ProofDoctorPreflightInput {
        ProofDoctorPreflightInput {
            bead_id: Some("ft-wik9p.4".to_string()),
            parent_bead_id: Some("ft-wik9p".to_string()),
            agent_name: "OliveChapel".to_string(),
            repo_path: "/Users/jemanuel/projects/frankenterm".to_string(),
            git_head: "4840b84d7".to_string(),
            branch: "main".to_string(),
            generated_at_utc: "2026-05-05T14:00:00Z".to_string(),
            intended_command: vec![
                "rch".to_string(),
                "exec".to_string(),
                "--".to_string(),
                "env".to_string(),
                "CARGO_TARGET_DIR=/tmp/ft-wik9p4-target".to_string(),
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "frankenterm-core-audit-types".to_string(),
                "proof_lane".to_string(),
            ],
            intended_target_dir: Some("/tmp/ft-wik9p4-target".to_string()),
            intended_scope: ProofScope::CargoTest,
            required_backend: ProofBackend::Rch,
            phase: ProofDoctorPhase::TerminalClassified,
            proof_path_prefixes: vec!["crates/frankenterm-core-audit-types/src".to_string()],
            evidence: ProofDoctorEvidence::default(),
        }
    }

    fn record_from_doctor(input: &ProofDoctorPreflightInput) -> ProofAttemptRecord {
        let verdict = classify_proof_doctor(input);
        let projection = verdict
            .ledger_projection
            .as_ref()
            .expect("proof doctor always projects into proof-lane vocabulary");
        let mut record = base_record(projection.state);
        record.proof_id = verdict.verdict_id.clone();
        record.bead_id = verdict.bead_id.clone().expect("test input has bead");
        record.parent_bead_id = verdict.parent_bead_id.clone();
        record.reason_code = projection.reason_code.clone();
        record.summary = projection.summary.clone();
        record.remote_cargo_reached = verdict.evidence.remote_cargo_reached;
        record.rustc_reached = verdict.evidence.rustc_reached;
        record.test_binary_started = verdict.evidence.test_binary_started;
        record.remote_exit_code = verdict.evidence.remote_exit_code;
        record.wrapper_exit_code = verdict.evidence.wrapper_exit_code;
        record.artifact_retrieval_status = verdict.evidence.artifact_retrieval_status;
        record = record.with_proof_doctor_verdict(&verdict);
        record
    }

    #[test]
    fn proof_state_terminal_flags_cover_contract() {
        assert!(!ProofState::NotRun.is_terminal());
        assert!(!ProofState::ReachedRemoteCargo.is_terminal());
        assert!(ProofState::Pass.is_terminal());
        assert!(ProofState::InfraBlockedPreCargo.is_terminal());
        assert!(ProofState::LocalInvalid.is_terminal());
    }

    #[test]
    fn proof_state_report_buckets_cover_contract() {
        let cases = [
            (ProofState::NotRun, ProofReportBucket::MissingEvidence),
            (
                ProofState::ReachedRemoteCargo,
                ProofReportBucket::MissingEvidence,
            ),
            (ProofState::SourceCompileFail, ProofReportBucket::SourceRed),
            (ProofState::TestFail, ProofReportBucket::SourceRed),
            (ProofState::Pass, ProofReportBucket::RemoteProofPassed),
            (
                ProofState::InfraBlockedPreCargo,
                ProofReportBucket::PreCargoInfrastructureBlocker,
            ),
            (
                ProofState::InfraBlockedPostCargo,
                ProofReportBucket::PostCargoInfrastructureBlocker,
            ),
            (
                ProofState::LocalInvalid,
                ProofReportBucket::InvalidLocalProof,
            ),
            (
                ProofState::SkippedNotProven,
                ProofReportBucket::SkippedNotProven,
            ),
            (
                ProofState::Inconclusive,
                ProofReportBucket::InconclusiveEvidence,
            ),
        ];

        for (state, bucket) in cases {
            assert_eq!(state.report_bucket(), bucket);
        }
    }

    #[test]
    fn pass_records_group_as_remote_proof_and_allow_closeout() {
        let mut record = base_record(ProofState::Pass);
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.test_binary_started = true;
        record.observed_backend = ProofBackend::Rch;
        record.wrapper_exit_code = Some(0);
        record.remote_exit_code = Some(0);
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        record.claims_allowed = vec!["focused_remote_test_passed".into()];

        assert_eq!(record.report_bucket(), ProofReportBucket::RemoteProofPassed);
        assert!(record.safe_to_close_source_bead());
        assert!(validate_proof_record(&record).is_empty());
    }

    #[test]
    fn pre_cargo_timeout_groups_as_infra_blocker() {
        let mut record = base_record(ProofState::InfraBlockedPreCargo);
        record.reason_code = "proof.infra.pre_cargo.rch_timeout_wrapper".into();
        record.selected_worker = Some("vmi1152480".into());
        record.sync_duration_ms = Some(180_611);
        record.wrapper_exit_code = Some(127);
        record.remote_cargo_reached = false;
        record.summary = "timeout failed to execute process before Cargo".into();

        assert_eq!(
            record.report_bucket(),
            ProofReportBucket::PreCargoInfrastructureBlocker
        );
        assert!(!record.safe_to_close_source_bead());
        assert!(validate_proof_record(&record).is_empty());
    }

    #[test]
    fn post_cargo_remote_timeout_groups_as_infra_blocker() {
        let mut record = base_record(ProofState::InfraBlockedPostCargo);
        record.reason_code = "proof.infra.post_cargo.rch_remote_timeout_1800s".into();
        record.selected_worker = Some("vmi1156319".into());
        record.sync_duration_ms = Some(202_721);
        record.remote_command_duration_ms = Some(1_800_000);
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Stalled;
        record.summary = "remote Cargo reached; SSH command timed out after 1800s".into();

        assert_eq!(
            record.report_bucket(),
            ProofReportBucket::PostCargoInfrastructureBlocker
        );
        assert!(!record.safe_to_close_source_bead());
        assert!(validate_proof_record(&record).is_empty());
    }

    #[test]
    fn shell_wrapped_attempt_groups_as_local_invalid() {
        let mut record = base_record(ProofState::LocalInvalid);
        record.command = vec![
            "rch".into(),
            "exec".into(),
            "--".into(),
            "env".into(),
            "CARGO_TARGET_DIR=/tmp/ft-luq3w-target".into(),
            "bash".into(),
            "-lc".into(),
            "cargo test -p frankenterm-core --lib auto_tune".into(),
        ];
        record.reason_code = "proof.local_invalid.shell_wrapped_cargo".into();
        record.local_cargo_detected = true;
        record.observed_backend = ProofBackend::LocalShell;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::NotApplicable;

        assert_eq!(record.report_bucket(), ProofReportBucket::InvalidLocalProof);
        assert!(!record.safe_to_close_source_bead());
        assert!(validate_proof_record(&record).is_empty());
        assert!(record.command_display().contains("bash -lc cargo test"));
    }

    #[test]
    fn source_compile_fail_is_source_red() {
        let mut record = base_record(ProofState::SourceCompileFail);
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.wrapper_exit_code = Some(101);
        record.remote_exit_code = Some(101);
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        assert_eq!(record.report_bucket(), ProofReportBucket::SourceRed);
        assert!(record.state.has_source_verdict());
        assert!(!record.safe_to_close_source_bead());
        assert!(validate_proof_record(&record).is_empty());
    }

    #[test]
    fn mixed_report_counts_required_buckets() {
        let mut pass = base_record(ProofState::Pass);
        pass.proof_id = "pass".into();
        pass.bead_id = "ft-pass".into();
        pass.remote_cargo_reached = true;
        pass.rustc_reached = true;
        pass.test_binary_started = true;
        pass.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let mut pre_cargo = base_record(ProofState::InfraBlockedPreCargo);
        pre_cargo.proof_id = "pre".into();
        pre_cargo.bead_id = "ft-pre".into();

        let mut local = base_record(ProofState::LocalInvalid);
        local.proof_id = "local".into();
        local.bead_id = "ft-local".into();
        local.local_cargo_detected = true;
        local.artifact_retrieval_status = ArtifactRetrievalStatus::NotApplicable;

        let mut source = base_record(ProofState::SourceCompileFail);
        source.proof_id = "source".into();
        source.bead_id = "ft-source".into();
        source.remote_cargo_reached = true;

        let report = ProofLaneReport::from_records(&[pass, pre_cargo, local, source]);

        assert_eq!(report.total_records, 4);
        assert_eq!(report.bucket_count(ProofReportBucket::RemoteProofPassed), 1);
        assert_eq!(
            report.bucket_count(ProofReportBucket::PreCargoInfrastructureBlocker),
            1
        );
        assert_eq!(report.bucket_count(ProofReportBucket::InvalidLocalProof), 1);
        assert_eq!(report.bucket_count(ProofReportBucket::SourceRed), 1);
        assert!(report.operator_summary.contains("local_invalid=1"));
    }

    #[test]
    fn proof_doctor_snapshots_surface_release_report_buckets() {
        let mut pass_input = base_doctor_input();
        pass_input.evidence.remote_cargo_reached = true;
        pass_input.evidence.rustc_reached = true;
        pass_input.evidence.test_binary_started = true;
        pass_input.evidence.remote_exit_code = Some(0);
        pass_input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        let mut pass = record_from_doctor(&pass_input);
        pass.proof_id = "doctor-pass".into();
        pass.bead_id = "ft-pass".into();

        let mut source_input = base_doctor_input();
        source_input.evidence.remote_cargo_reached = true;
        source_input.evidence.rustc_reached = true;
        source_input.evidence.remote_exit_code = Some(101);
        source_input.evidence.diagnostic_paths =
            vec!["crates/frankenterm-core/src/storage.rs".into()];
        source_input.evidence.diagnostic_summary =
            Some("remote rustc reported a first-party compile error".into());
        let mut source = record_from_doctor(&source_input);
        source.proof_id = "doctor-source".into();
        source.bead_id = "ft-source".into();

        let mut infra_input = base_doctor_input();
        infra_input.evidence.selected_worker = Some("vmi1152480".into());
        infra_input.evidence.sync_duration_ms = Some(164_000);
        infra_input.evidence.wrapper_exit_code = Some(127);
        let mut infra = record_from_doctor(&infra_input);
        infra.proof_id = "doctor-infra".into();
        infra.bead_id = "ft-infra".into();

        let mut dirty_input = base_doctor_input();
        dirty_input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/proof_lane.rs".into(),
            status: "M".into(),
            affects_proof: true,
            owner: Some(ProofDoctorOwner::Bead {
                bead_id: "ft-wik9p.5".into(),
                assignee: Some("MagentaFalcon".into()),
            }),
        });
        let mut dirty = record_from_doctor(&dirty_input);
        dirty.proof_id = "doctor-dirty".into();
        dirty.bead_id = "ft-dirty".into();

        let mut inconclusive_input = base_doctor_input();
        inconclusive_input.phase = ProofDoctorPhase::LaunchObserved;
        inconclusive_input.evidence.selected_worker = Some("vmi1153651".into());
        inconclusive_input.evidence.sync_duration_ms = Some(139_000);
        let mut inconclusive = record_from_doctor(&inconclusive_input);
        inconclusive.proof_id = "doctor-inconclusive".into();
        inconclusive.bead_id = "ft-inconclusive".into();

        let report = ProofLaneReport::from_records(&[
            pass,
            source,
            infra,
            dirty.clone(),
            inconclusive.clone(),
        ]);

        assert_eq!(report.bucket_count(ProofReportBucket::RemoteProofPassed), 1);
        assert_eq!(report.bucket_count(ProofReportBucket::SourceRed), 1);
        assert_eq!(
            report.bucket_count(ProofReportBucket::PreCargoInfrastructureBlocker),
            1
        );
        assert_eq!(report.bucket_count(ProofReportBucket::DirtyTreeBlocked), 1);
        assert_eq!(
            report.bucket_count(ProofReportBucket::InconclusiveEvidence),
            1
        );
        assert_eq!(
            report
                .by_proof_doctor_status
                .get(&ProofDoctorStatus::DirtyTreeBlocked),
            Some(&1)
        );
        assert!(report.operator_summary.contains("dirty_tree_blocked=1"));
        assert!(report.operator_summary.contains("inconclusive=1"));
        assert!(
            dirty
                .proof_doctor
                .as_ref()
                .is_some_and(|snapshot| snapshot.verdict_id.contains("proof-doctor:"))
        );
        assert_eq!(
            inconclusive.report_bucket(),
            ProofReportBucket::InconclusiveEvidence
        );
    }

    #[test]
    fn proof_doctor_dirty_tree_beats_older_pass_for_bead_summary() {
        let mut pass = base_record(ProofState::Pass);
        pass.proof_id = "older-pass".into();
        pass.bead_id = "ft-wik9p.4".into();
        pass.remote_cargo_reached = true;
        pass.rustc_reached = true;
        pass.test_binary_started = true;
        pass.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let mut dirty_input = base_doctor_input();
        dirty_input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/proof_lane.rs".into(),
            status: "M".into(),
            affects_proof: true,
            owner: Some(ProofDoctorOwner::Reservation {
                agent_name: "MagentaFalcon".into(),
                path_pattern: "crates/frankenterm-core-audit-types/src/proof_handoff.rs".into(),
            }),
        });
        let dirty = record_from_doctor(&dirty_input);

        let report = ProofLaneReport::from_records(&[pass, dirty]);

        assert_eq!(report.beads.len(), 1);
        assert_eq!(report.beads[0].bucket, ProofReportBucket::DirtyTreeBlocked);
        assert_eq!(
            report.beads[0]
                .proof_doctor
                .as_ref()
                .map(|snapshot| snapshot.status),
            Some(ProofDoctorStatus::DirtyTreeBlocked)
        );
    }

    #[test]
    fn proof_doctor_pass_snapshot_does_not_bypass_ledger_validation() {
        let mut pass_input = base_doctor_input();
        pass_input.evidence.remote_cargo_reached = true;
        pass_input.evidence.rustc_reached = true;
        pass_input.evidence.test_binary_started = true;
        pass_input.evidence.remote_exit_code = Some(0);
        pass_input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        let mut record = record_from_doctor(&pass_input);
        record.remote_cargo_reached = false;

        let findings = validate_proof_record(&record);

        assert!(
            findings
                .iter()
                .any(|finding| finding.reason_code == "pass_without_remote_cargo")
        );
    }

    #[test]
    fn invalid_pass_without_remote_cargo_emits_error() {
        let mut record = base_record(ProofState::Pass);
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        record.rustc_reached = true;
        record.test_binary_started = true;

        let findings = validate_proof_record(&record);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].reason_code, "pass_without_remote_cargo");
        assert_eq!(findings[0].severity, ProofFindingSeverity::Error);
    }

    #[test]
    fn pass_with_backend_mismatch_is_not_safe_to_close() {
        let mut record = base_record(ProofState::Pass);
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.test_binary_started = true;
        record.observed_backend = ProofBackend::LocalShell;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let findings = validate_proof_record(&record);

        assert!(!record.safe_to_close_source_bead());
        assert!(
            findings
                .iter()
                .any(|finding| finding.reason_code == "source_verdict_backend_mismatch")
        );
    }

    #[test]
    fn pass_without_execution_flags_is_not_safe_to_close() {
        let mut record = base_record(ProofState::Pass);
        record.remote_cargo_reached = true;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let findings = validate_proof_record(&record);

        assert!(!record.safe_to_close_source_bead());
        assert!(
            findings
                .iter()
                .any(|finding| finding.reason_code == "pass_without_rustc")
        );
        assert!(
            findings
                .iter()
                .any(|finding| { finding.reason_code == "pass_without_assertion_execution" })
        );
    }

    #[test]
    fn pass_with_unsafe_redaction_is_not_safe_to_close() {
        let mut record = base_record(ProofState::Pass);
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.test_binary_started = true;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        record.redaction_status = ProofRedactionStatus::UnsafeMissing;

        let findings = validate_proof_record(&record);

        assert!(!record.safe_to_close_source_bead());
        assert!(
            findings
                .iter()
                .any(|finding| finding.reason_code == "pass_with_unsafe_redaction")
        );
    }

    #[test]
    fn skipped_not_proven_cannot_allow_high_scale_claim() {
        let mut record = base_record(ProofState::SkippedNotProven);
        record.hardware_predicate = Some(ProofHardwarePredicate::SkippedNotProven);
        record.claims_allowed = vec!["high_scale_proven".into()];

        let findings = validate_proof_record(&record);

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].reason_code,
            "skipped_not_proven_allows_proven_claim"
        );
        assert!(!record.allows_high_scale_claim());
    }
}
