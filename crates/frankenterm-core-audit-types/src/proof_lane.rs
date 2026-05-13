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

use std::{collections::BTreeMap, path::Path};

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
    /// Cargo build lane.
    CargoBuild,
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

/// Release/closeout evidence class for operator-facing performance claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofEvidenceClass {
    /// High-scale hardware predicate and proof invariants both passed.
    HighScaleProven,
    /// High-scale hardware predicate was observed, but proof cannot support it.
    HighScaleNotProven,
    /// High-scale lane was explicitly skipped and must not be promoted.
    HighScaleSkippedNotProven,
    /// Reduced remote proof only.
    RemoteReduced,
    /// Reduced local proof only.
    LocalReduced,
    /// No explicit hardware/evidence class was retained.
    Unknown,
}

impl ProofEvidenceClass {
    /// Stable key for release and closeout summaries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HighScaleProven => "high_scale_proven",
            Self::HighScaleNotProven => "high_scale_not_proven",
            Self::HighScaleSkippedNotProven => "high_scale_skipped_not_proven",
            Self::RemoteReduced => "remote_reduced",
            Self::LocalReduced => "local_reduced",
            Self::Unknown => "unknown",
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

    /// Project a proof-doctor verdict into the durable proof-lane record schema.
    ///
    /// The resulting record still needs [`validate_proof_record`] before it is
    /// used for closeout or persisted as proof evidence. This mapper copies the
    /// observed proof-doctor evidence without strengthening it: missing Bead ids,
    /// incomplete artifacts, local fallback, or unknown redaction remain visible
    /// to validation instead of being papered over here.
    #[must_use]
    pub fn from_proof_doctor_verdict(
        verdict: &ProofDoctorVerdict,
        redaction_status: ProofRedactionStatus,
    ) -> Self {
        let projection = verdict
            .ledger_projection
            .as_ref()
            .map_or_else(inconclusive_projection, Clone::clone);
        let projected_state = projection.state;
        let mut record = Self::new(
            verdict.verdict_id.clone(),
            verdict.bead_id.clone().unwrap_or_default(),
            projected_state,
            projection.reason_code,
            projection.summary,
        );

        record.parent_bead_id.clone_from(&verdict.parent_bead_id);
        if projected_state != ProofState::NotRun {
            record
                .attempted_at_utc
                .clone_from(&verdict.generated_at_utc);
        }
        if projected_state.is_terminal() {
            record.finished_at_utc = Some(verdict.generated_at_utc.clone());
        }
        record.agent_name.clone_from(&verdict.agent_name);
        record.cwd.clone_from(&verdict.repo_path);
        record.command.clone_from(&verdict.intended_command);
        record
            .declared_target_dir
            .clone_from(&verdict.intended_target_dir);
        record.proof_scope = verdict.intended_scope;
        record.required_backend = verdict.required_backend;
        record.observed_backend = observed_backend_from_verdict(verdict);
        record.rch_version.clone_from(&verdict.evidence.rch_version);
        record
            .selected_worker
            .clone_from(&verdict.evidence.selected_worker);
        record
            .worker_probe_artifact
            .clone_from(&verdict.evidence.worker_probe_artifact);
        record.sync_duration_ms = verdict.evidence.sync_duration_ms;
        record.remote_command_duration_ms = verdict.evidence.remote_command_duration_ms;
        record.wrapper_exit_code = verdict.evidence.wrapper_exit_code;
        record.remote_exit_code = verdict.evidence.remote_exit_code;
        record.remote_cargo_reached = verdict.evidence.remote_cargo_reached;
        record.local_cargo_detected =
            verdict.evidence.local_cargo_detected || verdict.evidence.fail_open_detected;
        record.rustc_reached = verdict.evidence.rustc_reached;
        record.test_binary_started = verdict.evidence.test_binary_started;
        record.artifact_retrieval_status = verdict.evidence.artifact_retrieval_status;
        record
            .artifact_paths
            .clone_from(&verdict.evidence.artifact_paths);
        record.next_action.clone_from(&verdict.next_action.message);
        record.redaction_status = redaction_status;
        record = record.with_proof_doctor_verdict(verdict);

        if record.safe_to_close_source_bead() {
            record
                .claims_allowed
                .push("focused_remote_proof_passed".to_string());
        }

        record
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

fn inconclusive_projection() -> crate::proof_doctor::ProofAttemptProjection {
    crate::proof_doctor::ProofAttemptProjection {
        state: ProofState::Inconclusive,
        reason_code: "proof.doctor.missing_projection".to_string(),
        summary: "Proof-doctor did not emit a proof-lane projection.".to_string(),
        safe_to_close: false,
    }
}

fn observed_backend_from_verdict(verdict: &ProofDoctorVerdict) -> ProofBackend {
    if verdict.evidence.local_cargo_detected || verdict.evidence.fail_open_detected {
        ProofBackend::LocalShell
    } else if verdict.required_backend == ProofBackend::None {
        ProofBackend::None
    } else if verdict.evidence.remote_cargo_reached
        || verdict.evidence.selected_worker.is_some()
        || verdict.evidence.sync_duration_ms.is_some()
    {
        ProofBackend::Rch
    } else {
        ProofBackend::Unknown
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
        && record
            .finished_at_utc
            .as_deref()
            .is_none_or(|timestamp| timestamp.trim().is_empty())
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

    if record.state == ProofState::LocalInvalid
        && record
            .claims_allowed
            .iter()
            .any(|claim| is_proven_claim(claim))
    {
        findings.push(ProofLedgerFinding::error(
            record,
            "local_invalid_allows_proven_claim",
            "LOCAL_INVALID records cannot allow proven or passed claims",
        ));
    }

    if record.state == ProofState::SkippedNotProven
        && record
            .claims_allowed
            .iter()
            .any(|claim| is_proven_claim(claim))
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

/// Lint proposed Beads closeout text and retained proof artifacts.
#[must_use]
pub fn lint_proof_closeout(input: &ProofCloseoutLintInput) -> ProofCloseoutLintReport {
    let mut findings = Vec::new();
    lint_closeout_artifact_availability(input, &mut findings);
    lint_closeout_records(input, &mut findings);
    lint_closeout_text(input, &mut findings);

    let proof_records_analyzed = input
        .artifacts
        .iter()
        .map(|artifact| artifact.records.len() as u64)
        .sum();
    let artifact_paths = input
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact_path.clone())
        .collect::<Vec<_>>();
    let supporting_record = input
        .artifacts
        .iter()
        .flat_map(|artifact| {
            artifact
                .records
                .iter()
                .map(|record| (artifact.artifact_path.as_str(), record))
        })
        .find(|(_, record)| closeout_record_supports_green_claim(record));
    let text_supports_green_claim = closeout_text_supports_green_claim(input);
    let has_errors = findings
        .iter()
        .any(|finding| finding.severity == ProofFindingSeverity::Error);
    let closeout_eligible =
        !has_errors && (supporting_record.is_some() || text_supports_green_claim);
    let suggested_beads_wording = closeout_suggested_wording(
        input,
        supporting_record,
        text_supports_green_claim,
        closeout_eligible,
    );
    let operator_summary = closeout_lint_operator_summary(
        closeout_eligible,
        proof_records_analyzed,
        findings.len(),
        has_errors,
    );

    ProofCloseoutLintReport {
        schema_version: PROOF_CLOSEOUT_LINTER_SCHEMA_VERSION,
        bead_id: input.bead_id.clone(),
        closeout_eligible,
        proof_records_analyzed,
        artifact_paths,
        findings,
        suggested_beads_wording,
        operator_summary,
    }
}

fn lint_closeout_artifact_availability(
    input: &ProofCloseoutLintInput,
    findings: &mut Vec<ProofCloseoutLintFinding>,
) {
    for artifact in &input.artifacts {
        if let Some(error) = artifact.read_error.as_deref() {
            findings.push(ProofCloseoutLintFinding::error(
                "proof.closeout.artifact_unavailable",
                &format!(
                    "Closeout cites artifact `{}` but it could not be read or parsed: {error}",
                    artifact.artifact_path
                ),
                "Proof-doctor: inconclusive; reason proof.artifact.retention_failed; closeout blocked until the retained artifact is readable.",
                vec![artifact.artifact_path.clone()],
            ));
        }
    }
}

fn lint_closeout_records(
    input: &ProofCloseoutLintInput,
    findings: &mut Vec<ProofCloseoutLintFinding>,
) {
    for artifact in &input.artifacts {
        for record in &artifact.records {
            for finding in validate_proof_record(record) {
                findings.push(ProofCloseoutLintFinding {
                    severity: finding.severity,
                    reason_code: format!("proof.closeout.{}", finding.reason_code),
                    message: finding.message,
                    suggested_beads_wording: closeout_record_blocked_wording(
                        record,
                        &artifact.artifact_path,
                    ),
                    evidence_keys: vec![artifact.artifact_path.clone(), record.proof_id.clone()],
                });
            }

            if record.state == ProofState::Pass
                && input.required_backend == ProofBackend::Rch
                && record.selected_worker.as_deref().is_none_or(str::is_empty)
            {
                findings.push(ProofCloseoutLintFinding::error(
                    "proof.closeout.missing_selected_worker",
                    "Remote proof pass closeout must retain the selected RCH worker.",
                    "Proof-doctor: inconclusive; reason proof.closeout.missing_selected_worker; remote Cargo reached; closeout blocked until the selected worker is retained.",
                    vec![artifact.artifact_path.clone(), record.proof_id.clone()],
                ));
            }

            if record.state == ProofState::Pass && !record.safe_to_close_source_bead() {
                findings.push(ProofCloseoutLintFinding::error(
                    "proof.closeout.pass_record_not_closeout_safe",
                    "Proof record is PASS but does not satisfy closeout safety invariants.",
                    &closeout_record_blocked_wording(record, &artifact.artifact_path),
                    vec![artifact.artifact_path.clone(), record.proof_id.clone()],
                ));
            }
        }
    }
}

fn lint_closeout_text(
    input: &ProofCloseoutLintInput,
    findings: &mut Vec<ProofCloseoutLintFinding>,
) {
    let text = input.closeout_text.as_deref().unwrap_or_default();
    let normalized = normalize_closeout_text(text);
    let green_claim = closeout_text_claims_green(&normalized);
    let explicit_non_applicable = closeout_text_says_non_applicable(&normalized);
    let has_records = input
        .artifacts
        .iter()
        .any(|artifact| !artifact.records.is_empty());
    let has_artifact_paths = !input.artifacts.is_empty();

    if !has_records && !has_artifact_paths && !explicit_non_applicable {
        findings.push(ProofCloseoutLintFinding::error(
            "proof.closeout.missing_artifact_path",
            "Proof closeout has no retained proof record or artifact path.",
            "Proof-doctor: inconclusive; reason proof.closeout.missing_artifact_path; closeout blocked until retained artifacts are cited.",
            vec!["artifact_paths".to_string()],
        ));
    }

    if !green_claim {
        return;
    }

    if input.required_backend == ProofBackend::Rch
        && closeout_text_mentions_local_fallback(&normalized)
    {
        findings.push(ProofCloseoutLintFinding::error(
            "proof.closeout.local_fallback_claimed_as_proof",
            "Closeout text promotes local Cargo or local fallback as proof for an RCH-required lane.",
            "Proof-doctor: invalid; reason proof.command.local_cargo_invalid; remote Cargo not reached; closeout blocked.",
            vec!["closeout_text".to_string()],
        ));
    }

    if closeout_text_mentions_stale_proof_phrase(&normalized)
        && !closeout_text_says_not_proof(&normalized)
    {
        findings.push(ProofCloseoutLintFinding::error(
            "proof.closeout.sync_or_queue_claimed_as_proof",
            "Closeout text treats sync, cache warmup, worker selection, or queued CI status as proof.",
            "Proof-doctor: inconclusive; reason proof.rch.sync_not_proof; remote Cargo not reached; closeout blocked.",
            vec!["closeout_text".to_string()],
        ));
    }

    if input.dirty_tree && !closeout_text_has_dirty_tree_caveat(&normalized) {
        findings.push(ProofCloseoutLintFinding::error(
            "proof.closeout.dirty_tree_caveat_missing",
            "Closeout text claims proof from a shared dirty checkout without a dirty-tree caveat.",
            "Proof-doctor: dirty_tree_blocked; reason proof.dirty.unowned_path_overlap; closeout blocked unless dirty paths and ownership are named.",
            vec!["closeout_text".to_string(), "dirty_tree".to_string()],
        ));
    }

    if !has_records && !closeout_text_has_remote_proof_fields(&normalized, has_artifact_paths) {
        findings.push(ProofCloseoutLintFinding::error(
            "proof.closeout.remote_claim_missing_fields",
            "Remote proof claim is missing selected worker, command, remote Cargo/rustc/test evidence, classification, or retained artifact path.",
            "Proof-doctor: inconclusive; reason proof.closeout.remote_claim_missing_fields; closeout blocked until the generated proof-doctor handoff fields are present.",
            vec!["closeout_text".to_string()],
        ));
    }
}

fn closeout_record_supports_green_claim(record: &ProofAttemptRecord) -> bool {
    record.safe_to_close_source_bead()
        && (record.required_backend != ProofBackend::Rch
            || record
                .selected_worker
                .as_deref()
                .is_some_and(|worker| !worker.trim().is_empty()))
}

fn closeout_suggested_wording(
    input: &ProofCloseoutLintInput,
    supporting_record: Option<(&str, &ProofAttemptRecord)>,
    text_supports_green_claim: bool,
    closeout_eligible: bool,
) -> String {
    if let Some((artifact_path, record)) = supporting_record {
        return closeout_record_pass_wording(record, artifact_path, closeout_eligible);
    }

    if text_supports_green_claim {
        return closeout_text_pass_wording(input, closeout_eligible);
    }

    input
        .artifacts
        .iter()
        .flat_map(|artifact| {
            artifact
                .records
                .iter()
                .map(|record| (artifact.artifact_path.as_str(), record))
        })
        .find(|(_, record)| record.state.is_terminal())
        .map_or_else(
            || {
                if closeout_text_says_non_applicable(&normalize_closeout_text(
                    input.closeout_text.as_deref().unwrap_or_default(),
                )) {
                    "Proof-doctor: not applicable; docs-static change only; no Cargo/RCH proof lane claimed.\nProof-record: not_requested; path none; validation not_applicable; closeout blocked.".to_string()
                } else if input.artifacts.is_empty() {
                    "Proof-doctor: inconclusive; reason proof.closeout.missing_artifact_path; closeout blocked until retained artifacts are cited.".to_string()
                } else {
                    "Proof-doctor: inconclusive; reason proof.closeout.remote_claim_missing_fields; closeout blocked until selected worker, command, remote Cargo/rustc/test evidence, classification, and artifact path are retained.".to_string()
                }
            },
            |(artifact_path, record)| closeout_record_blocked_wording(record, artifact_path),
        )
}

fn closeout_record_pass_wording(
    record: &ProofAttemptRecord,
    artifact_path: &str,
    closeout_eligible: bool,
) -> String {
    let closeout = if closeout_eligible { "safe" } else { "blocked" };
    format!(
        "Proof-doctor: passed; phase {}; reason {}; verdict {}; remote Cargo {}; owner none; target_dir {}; target_lifecycle kept; target_size unknown; closeout {closeout}.\nProof-record: written; path {artifact_path}; validation ok; closeout {closeout}.",
        record
            .proof_doctor
            .as_ref()
            .map_or("terminal_classified", |snapshot| {
                proof_phase_label(snapshot.phase)
            }),
        record.reason_code,
        record.proof_id,
        remote_cargo_closeout_label(record.remote_cargo_reached),
        record.declared_target_dir.as_deref().unwrap_or("none"),
    )
}

fn closeout_text_pass_wording(input: &ProofCloseoutLintInput, closeout_eligible: bool) -> String {
    let closeout = if closeout_eligible { "safe" } else { "blocked" };
    let artifact_paths = input
        .artifacts
        .iter()
        .map(|artifact| artifact.artifact_path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Proof-doctor: passed; phase terminal_classified; reason proof.closeout.text_fields_ok; verdict retained-closeout-text; remote Cargo reached; owner none; target_dir unknown; target_lifecycle kept; target_size unknown; closeout {closeout}.\nProof-record: not_provided; artifacts {artifact_paths}; validation text_fields_ok; closeout {closeout}."
    )
}

fn closeout_record_blocked_wording(record: &ProofAttemptRecord, artifact_path: &str) -> String {
    let status = record.proof_doctor.as_ref().map_or_else(
        || proof_state_status_label(record.state),
        |snapshot| proof_doctor_status_label(snapshot.status),
    );
    format!(
        "Proof-doctor: {status}; phase {}; reason {}; verdict {}; remote Cargo {}; owner none; target_dir {}; target_lifecycle kept; target_size unknown; closeout blocked.\nProof-record: written; path {artifact_path}; validation {}; closeout blocked.",
        record
            .proof_doctor
            .as_ref()
            .map_or("terminal_classified", |snapshot| {
                proof_phase_label(snapshot.phase)
            }),
        record.reason_code,
        record.proof_id,
        remote_cargo_closeout_label(record.remote_cargo_reached),
        record.declared_target_dir.as_deref().unwrap_or("none"),
        if validate_proof_record(record)
            .iter()
            .any(|finding| finding.severity == ProofFindingSeverity::Error)
        {
            "error"
        } else {
            "ok"
        },
    )
}

fn closeout_lint_operator_summary(
    closeout_eligible: bool,
    proof_records_analyzed: u64,
    finding_count: usize,
    has_errors: bool,
) -> String {
    if closeout_eligible {
        return format!(
            "Proof closeout is eligible from retained evidence; {proof_records_analyzed} proof record(s) analyzed and {finding_count} finding(s) emitted."
        );
    }
    if has_errors {
        return format!(
            "Proof closeout is rejected; {proof_records_analyzed} proof record(s) analyzed and {finding_count} finding(s) emitted."
        );
    }
    format!(
        "Proof closeout is not green-closeout eligible; {proof_records_analyzed} proof record(s) analyzed and {finding_count} finding(s) emitted."
    )
}

fn normalize_closeout_text(text: &str) -> String {
    text.to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn closeout_text_claims_green(normalized: &str) -> bool {
    closeout_text_contains_any(
        normalized,
        &[
            "closeout safe",
            "proof lane passed",
            "proof passed",
            "remote proof passed",
            "remote rch proof",
            "rch proof passed",
            "green claim",
            "source health proved",
        ],
    ) || (normalized.contains("passed") && normalized.contains("remote cargo"))
}

fn closeout_text_mentions_local_fallback(normalized: &str) -> bool {
    closeout_text_contains_any(
        normalized,
        &[
            "local fallback",
            "local cargo",
            "cargo-local.sh",
            "scripts/cargo-local.sh",
        ],
    ) || (normalized.contains("cargo test")
        && !normalized.contains("rch exec")
        && !normalized.contains("local smoke"))
}

fn closeout_text_mentions_stale_proof_phrase(normalized: &str) -> bool {
    closeout_text_contains_any(
        normalized,
        &[
            "sync completed",
            "rsync",
            "cache warmup",
            "cache warmed",
            "workflow queued",
            "queued workflow",
            "status queued",
            "pending workflow",
            "github actions queued",
        ],
    )
}

fn closeout_text_says_not_proof(normalized: &str) -> bool {
    closeout_text_contains_any(
        normalized,
        &[
            "not proof",
            "not a proof",
            "not source proof",
            "not a source failure",
            "no source verdict",
            "not a source verdict",
            "not proof by itself",
        ],
    )
}

fn closeout_text_has_dirty_tree_caveat(normalized: &str) -> bool {
    normalized.contains("dirty")
        && closeout_text_contains_any(normalized, &["caveat", "blocked", "overlap", "shared"])
}

fn closeout_text_says_non_applicable(normalized: &str) -> bool {
    closeout_text_contains_any(
        normalized,
        &[
            "proof-doctor: not applicable",
            "docs-static change only",
            "no cargo/rch proof lane claimed",
            "no cargo proof lane claimed",
        ],
    )
}

fn closeout_text_has_remote_proof_fields(normalized: &str, has_artifact_paths: bool) -> bool {
    has_artifact_paths
        && closeout_text_contains_any(normalized, &["selected_worker", "selected worker"])
        && normalized.contains("command:")
        && normalized.contains("remote cargo reached")
        && normalized.contains("rustc")
        && normalized.contains("test")
        && closeout_text_contains_any(normalized, &["passed", "source_blocked", "test_blocked"])
}

fn closeout_text_supports_green_claim(input: &ProofCloseoutLintInput) -> bool {
    let normalized = normalize_closeout_text(input.closeout_text.as_deref().unwrap_or_default());
    closeout_text_claims_green(&normalized)
        && closeout_text_has_remote_proof_fields(&normalized, !input.artifacts.is_empty())
}

fn closeout_text_contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

const fn proof_doctor_status_label(status: ProofDoctorStatus) -> &'static str {
    match status {
        ProofDoctorStatus::Runnable => "runnable",
        ProofDoctorStatus::Passed => "passed",
        ProofDoctorStatus::SourceBlocked => "source_blocked",
        ProofDoctorStatus::TestBlocked => "test_blocked",
        ProofDoctorStatus::InfraBlocked => "infra_blocked",
        ProofDoctorStatus::DirtyTreeBlocked => "dirty_tree_blocked",
        ProofDoctorStatus::OwnershipBlocked => "ownership_blocked",
        ProofDoctorStatus::Invalid => "invalid",
        ProofDoctorStatus::SkippedNotProven => "skipped_not_proven",
        ProofDoctorStatus::Inconclusive => "inconclusive",
    }
}

const fn proof_phase_label(phase: ProofDoctorPhase) -> &'static str {
    match phase {
        ProofDoctorPhase::Preflight => "preflight",
        ProofDoctorPhase::LaunchObserved => "launch_observed",
        ProofDoctorPhase::RemoteCargoObserved => "remote_cargo_observed",
        ProofDoctorPhase::TerminalClassified => "terminal_classified",
        ProofDoctorPhase::EvidenceGap => "evidence_gap",
    }
}

const fn proof_state_status_label(state: ProofState) -> &'static str {
    match state {
        ProofState::NotRun | ProofState::ReachedRemoteCargo => "inconclusive",
        ProofState::SourceCompileFail => "source_blocked",
        ProofState::TestFail => "test_blocked",
        ProofState::Pass => "passed",
        ProofState::InfraBlockedPreCargo | ProofState::InfraBlockedPostCargo => "infra_blocked",
        ProofState::LocalInvalid => "invalid",
        ProofState::SkippedNotProven => "skipped_not_proven",
        ProofState::Inconclusive => "inconclusive",
    }
}

const fn remote_cargo_closeout_label(remote_cargo_reached: bool) -> &'static str {
    if remote_cargo_reached {
        "reached"
    } else {
        "not reached"
    }
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

fn is_proven_claim(claim: &str) -> bool {
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
            | ProofScope::CargoBuild
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

/// Blocker group for release and swarm closeout reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCloseoutBlockerGroup {
    /// Stable report bucket for the blocker.
    pub bucket: ProofReportBucket,
    /// Number of proof records in this blocker bucket.
    pub count: u64,
    /// Affected Beads, deduplicated in first-seen order.
    pub bead_ids: Vec<String>,
    /// Stable reason codes contributing to this blocker.
    pub reason_codes: Vec<String>,
    /// Operator-facing next actions from the selected records.
    pub next_actions: Vec<String>,
}

/// Release/swarm closeout report derived from proof-lane records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCloseoutReport {
    /// Report schema version.
    pub schema_version: u32,
    /// Number of records summarized.
    pub total_records: u64,
    /// Counts by evidence class key.
    pub by_evidence_class: BTreeMap<String, u64>,
    /// Beads with enough evidence to support normal source closeout.
    pub closeable_source_beads: Vec<String>,
    /// Beads with enough evidence to support high-scale claims.
    pub high_scale_claim_beads: Vec<String>,
    /// Actionable non-pass blocker groups.
    pub blocker_groups: Vec<ProofCloseoutBlockerGroup>,
    /// Validation findings inherited from proof-lane validation.
    pub findings: Vec<ProofLedgerFinding>,
    /// Concise operator summary for release notes and Beads comments.
    pub operator_summary: String,
}

/// Input artifact supplied to the proof-history indexer.
///
/// The indexer is intentionally independent of filesystem access so callers
/// can feed checked-in fixtures, retained E2E artifacts, or remote-collected
/// content while keeping hash/source/closeout metadata explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryArtifactInput {
    /// Repo-relative or absolute artifact path.
    pub artifact_path: String,
    /// Artifact content. `None` means the caller could not provide the file.
    pub content: Option<String>,
    /// Read error surfaced by the caller when content is unavailable.
    pub read_error: Option<String>,
    /// SHA-256 or equivalent content hash computed by the caller.
    pub content_sha256: Option<String>,
    /// Expected artifact hash, when a manifest or closeout declared one.
    pub expected_sha256: Option<String>,
    /// Source commit associated with this artifact.
    pub source_commit: Option<String>,
    /// Expected source commit, when a closeout or release gate declared one.
    pub expected_source_commit: Option<String>,
    /// Machine-readable proof category, such as `4` or `release/attestation`.
    pub proof_category: Option<String>,
    /// Current Beads closeout timestamp for stale-artifact detection.
    pub bead_closed_at_utc: Option<String>,
}

impl ProofHistoryArtifactInput {
    /// Construct an artifact input with content and no external expectations.
    #[must_use]
    pub fn new(artifact_path: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            artifact_path: artifact_path.into(),
            content: Some(content.into()),
            read_error: None,
            content_sha256: None,
            expected_sha256: None,
            source_commit: None,
            expected_source_commit: None,
            proof_category: None,
            bead_closed_at_utc: None,
        }
    }

    /// Construct a missing/unreadable artifact input.
    #[must_use]
    pub fn unavailable(artifact_path: impl Into<String>, read_error: Option<String>) -> Self {
        Self {
            artifact_path: artifact_path.into(),
            content: None,
            read_error,
            content_sha256: None,
            expected_sha256: None,
            source_commit: None,
            expected_source_commit: None,
            proof_category: None,
            bead_closed_at_utc: None,
        }
    }
}

/// Artifact-level status for proof-history ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofHistoryArtifactStatus {
    /// Artifact was parsed and indexed without artifact-level problems.
    Indexed,
    /// Artifact exists but has no proof records.
    Empty,
    /// Artifact records predate the current Beads closeout timestamp.
    Stale,
    /// Artifact source commit disagrees with the expected source commit.
    SourceCommitMismatch,
    /// Artifact hash disagrees with the expected hash.
    HashMismatch,
    /// Artifact content was present but could not be parsed as proof JSONL.
    InvalidJson,
    /// Caller reported that the artifact could not be read.
    Unreadable,
    /// Caller could not provide the artifact content.
    MissingFile,
}

impl ProofHistoryArtifactStatus {
    /// Stable string key for reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Indexed => "indexed",
            Self::Empty => "empty",
            Self::Stale => "stale",
            Self::SourceCommitMismatch => "source_commit_mismatch",
            Self::HashMismatch => "hash_mismatch",
            Self::InvalidJson => "invalid_json",
            Self::Unreadable => "unreadable",
            Self::MissingFile => "missing_file",
        }
    }
}

/// Per-artifact ingestion report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryArtifactReport {
    /// Artifact path supplied by the caller.
    pub artifact_path: String,
    /// Artifact ingestion status.
    pub status: ProofHistoryArtifactStatus,
    /// Number of proof records parsed from this artifact.
    pub rows_indexed: u64,
    /// Actual content hash supplied by the caller.
    pub content_sha256: Option<String>,
    /// Source commit associated with this artifact.
    pub source_commit: Option<String>,
    /// Proof category associated with this artifact.
    pub proof_category: Option<String>,
    /// Stable reason code for non-indexed status.
    pub reason_code: Option<String>,
    /// Operator-facing detail for non-indexed status.
    pub detail: Option<String>,
}

/// Indexed proof record plus the artifact metadata needed for release rollups.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryRecord {
    /// Artifact path that supplied the record.
    pub artifact_path: String,
    /// Artifact content hash supplied by the caller.
    pub artifact_sha256: Option<String>,
    /// Source commit associated with the artifact.
    pub source_commit: Option<String>,
    /// Proof category associated with the artifact.
    pub proof_category: Option<String>,
    /// Artifact status after hash/source/staleness checks.
    pub artifact_status: ProofHistoryArtifactStatus,
    /// Parsed durable proof record.
    pub record: ProofAttemptRecord,
    /// Validation and artifact findings scoped to this record.
    pub findings: Vec<ProofLedgerFinding>,
}

/// Machine-readable proof-history index from retained artifact content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryIndex {
    /// Index schema version.
    pub schema_version: u32,
    /// Per-artifact reports.
    pub artifacts: Vec<ProofHistoryArtifactReport>,
    /// Indexed records with artifact metadata.
    pub records: Vec<ProofHistoryRecord>,
    /// Flattened record-level findings.
    pub findings: Vec<ProofLedgerFinding>,
    /// Concise operator summary.
    pub operator_summary: String,
}

/// One release scoreboard row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofReleaseScoreboardRow {
    /// Proof category or `uncategorized`.
    pub proof_category: String,
    /// Beads issue id.
    pub bead_id: String,
    /// Parent proof program or epic id.
    pub parent_bead_id: Option<String>,
    /// Source commit associated with the artifact.
    pub source_commit: Option<String>,
    /// Artifact path that supplied the record.
    pub artifact_path: String,
    /// Artifact content hash supplied by the caller.
    pub artifact_sha256: Option<String>,
    /// Latest proof state for this row.
    pub latest_verdict: ProofState,
    /// Stable report bucket.
    pub bucket: ProofReportBucket,
    /// Stable reason code.
    pub reason_code: String,
    /// True only when the row can support closing the source bead.
    pub closeout_eligible: bool,
    /// True only when the row can support high-scale claims.
    pub high_scale_claim_allowed: bool,
    /// Selected RCH worker, when retained.
    pub selected_worker: Option<String>,
    /// Residual blocker reason for non-closeable rows.
    pub residual_blocker: Option<String>,
    /// Start timestamp.
    pub attempted_at_utc: String,
    /// Finish timestamp.
    pub finished_at_utc: Option<String>,
    /// Artifact status.
    pub artifact_status: ProofHistoryArtifactStatus,
    /// Record validation error count.
    pub validation_error_count: u64,
    /// Record validation warning count.
    pub validation_warning_count: u64,
}

/// Release scoreboard derived from retained proof-history artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofReleaseScoreboard {
    /// Scoreboard schema version.
    pub schema_version: u32,
    /// Number of indexed proof records.
    pub total_records: u64,
    /// All scoreboard rows.
    pub rows: Vec<ProofReleaseScoreboardRow>,
    /// Latest row per Beads id.
    pub latest_by_bead: Vec<ProofReleaseScoreboardRow>,
    /// Non-closeable rows and rows with artifact/validation issues.
    pub blocking_rows: Vec<ProofReleaseScoreboardRow>,
    /// Artifact reports with non-indexed status.
    pub artifact_issues: Vec<ProofHistoryArtifactReport>,
    /// Counts by report bucket key.
    pub by_bucket: BTreeMap<String, u64>,
    /// Counts by proof category.
    pub by_proof_category: BTreeMap<String, u64>,
    /// Beads with enough evidence to support source closeout.
    pub closeable_source_beads: Vec<String>,
    /// Beads with enough evidence to support high-scale claims.
    pub high_scale_claim_beads: Vec<String>,
    /// Flattened record-level findings.
    pub findings: Vec<ProofLedgerFinding>,
    /// Concise operator summary.
    pub operator_summary: String,
}

/// Query parameters for read-only proof-history surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryQuery {
    /// Restrict rows to one Beads id.
    pub bead_id: Option<String>,
    /// Restrict rows to one proof category.
    pub proof_category: Option<String>,
    /// Restrict rows to one terminal/intermediate proof state.
    pub status: Option<ProofState>,
    /// Return only rows that block release or source closeout.
    pub release_blocking_only: bool,
    /// Preserve absolute local paths in returned artifact paths.
    pub include_local_paths: bool,
    /// Maximum rows to return.
    pub limit: usize,
    /// Number of matching rows to skip.
    pub offset: usize,
}

impl Default for ProofHistoryQuery {
    fn default() -> Self {
        Self {
            bead_id: None,
            proof_category: None,
            status: None,
            release_blocking_only: false,
            include_local_paths: false,
            limit: 100,
            offset: 0,
        }
    }
}

/// Release-blocking summary included with proof-history query results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryReleaseBlockingSummary {
    /// Number of blocking rows.
    pub total_blocking_rows: u64,
    /// Blocking row counts by report bucket.
    pub by_bucket: BTreeMap<String, u64>,
    /// Blocking row counts by proof category.
    pub by_proof_category: BTreeMap<String, u64>,
    /// Beads that currently have blocking rows.
    pub blocking_beads: Vec<String>,
    /// Number of artifact-level issues.
    pub artifact_issue_count: u64,
    /// Number of validation findings.
    pub validation_finding_count: u64,
}

/// Paged read-only proof-history response shared by robot and MCP surfaces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofHistoryQueryResult {
    /// Response schema version.
    pub schema_version: u32,
    /// Query that produced this response.
    pub query: ProofHistoryQuery,
    /// Total rows matching the filters before pagination.
    pub total_matches: u64,
    /// Number of rows returned in this page.
    pub returned_rows: u64,
    /// Next offset when more rows are available.
    pub next_offset: Option<usize>,
    /// Latest row for `query.bead_id`, when a bead filter was supplied.
    pub latest_for_bead: Option<ProofReleaseScoreboardRow>,
    /// Paged canonical scoreboard rows.
    pub rows: Vec<ProofReleaseScoreboardRow>,
    /// Artifact issues such as missing, unreadable, stale, or invalid files.
    pub artifact_issues: Vec<ProofHistoryArtifactReport>,
    /// Release-blocking rollup independent of the page filters.
    pub release_blocking_summary: ProofHistoryReleaseBlockingSummary,
    /// Concise operator summary.
    pub operator_summary: String,
}

/// Proof closeout linter schema version implemented by this module.
pub const PROOF_CLOSEOUT_LINTER_SCHEMA_VERSION: u32 = 1;

/// Retained artifact supplied to the proof closeout linter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCloseoutLintArtifact {
    /// Repo-relative or absolute artifact path.
    pub artifact_path: String,
    /// Parsed proof records from this artifact, if it is a JSONL proof record.
    pub records: Vec<ProofAttemptRecord>,
    /// Read or parse error, when the artifact was unavailable or malformed.
    pub read_error: Option<String>,
}

/// Input to the proof closeout linter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCloseoutLintInput {
    /// Optional Beads issue id whose closeout text is being checked.
    pub bead_id: Option<String>,
    /// Proposed Beads closeout or handoff text.
    pub closeout_text: Option<String>,
    /// Required backend for this proof lane.
    pub required_backend: ProofBackend,
    /// Whether the proof ran or would close against a shared dirty checkout.
    pub dirty_tree: bool,
    /// Retained proof records or artifact paths.
    pub artifacts: Vec<ProofCloseoutLintArtifact>,
}

/// One proof closeout linter finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCloseoutLintFinding {
    /// Finding severity.
    pub severity: ProofFindingSeverity,
    /// Stable machine-readable reason code.
    pub reason_code: String,
    /// Operator-facing explanation.
    pub message: String,
    /// Suggested Beads wording that preserves the proof truth.
    pub suggested_beads_wording: String,
    /// Evidence keys or paths that triggered this finding.
    pub evidence_keys: Vec<String>,
}

impl ProofCloseoutLintFinding {
    fn error(
        reason_code: &str,
        message: &str,
        suggested_beads_wording: &str,
        evidence_keys: Vec<String>,
    ) -> Self {
        Self {
            severity: ProofFindingSeverity::Error,
            reason_code: reason_code.to_string(),
            message: message.to_string(),
            suggested_beads_wording: suggested_beads_wording.to_string(),
            evidence_keys,
        }
    }
}

/// Machine-readable proof closeout linter report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCloseoutLintReport {
    /// Linter schema version.
    pub schema_version: u32,
    /// Optional Beads issue id whose closeout text was checked.
    pub bead_id: Option<String>,
    /// True only when retained evidence supports a green source-bead closeout.
    pub closeout_eligible: bool,
    /// Number of proof records parsed from supplied artifacts.
    pub proof_records_analyzed: u64,
    /// Artifact paths supplied to the linter.
    pub artifact_paths: Vec<String>,
    /// Structured linter findings.
    pub findings: Vec<ProofCloseoutLintFinding>,
    /// Suggested Beads wording based on the strongest retained evidence.
    pub suggested_beads_wording: String,
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

impl ProofCloseoutReport {
    /// Build a release/swarm closeout report from proof records.
    #[must_use]
    pub fn from_records(records: &[ProofAttemptRecord]) -> Self {
        let mut by_evidence_class = BTreeMap::new();
        let mut closeable_source_beads = Vec::new();
        let mut high_scale_claim_beads = Vec::new();
        let mut blocker_groups = BTreeMap::<ProofReportBucket, ProofCloseoutBlockerGroup>::new();
        let mut findings = Vec::new();

        for record in records {
            let evidence_class = record.evidence_class();
            *by_evidence_class
                .entry(evidence_class.as_str().to_string())
                .or_insert(0) += 1;

            if record.safe_to_close_source_bead() {
                push_unique_non_empty(&mut closeable_source_beads, &record.bead_id);
            }
            if record.allows_high_scale_claim() {
                push_unique_non_empty(&mut high_scale_claim_beads, &record.bead_id);
            }

            let bucket = record.report_bucket();
            if bucket != ProofReportBucket::RemoteProofPassed {
                let group =
                    blocker_groups
                        .entry(bucket)
                        .or_insert_with(|| ProofCloseoutBlockerGroup {
                            bucket,
                            count: 0,
                            bead_ids: Vec::new(),
                            reason_codes: Vec::new(),
                            next_actions: Vec::new(),
                        });
                group.count += 1;
                push_unique_non_empty(&mut group.bead_ids, &record.bead_id);
                push_unique_non_empty(&mut group.reason_codes, &record.reason_code);
                push_unique_non_empty(&mut group.next_actions, &record.next_action);
            }

            findings.extend(validate_proof_record(record));
        }

        let blocker_groups = blocker_groups.into_values().collect::<Vec<_>>();
        let operator_summary = closeout_operator_summary(
            records.len(),
            closeable_source_beads.len(),
            high_scale_claim_beads.len(),
            blocker_groups.len(),
            &by_evidence_class,
            findings.len(),
        );

        Self {
            schema_version: PROOF_LANE_SCHEMA_VERSION,
            total_records: records.len() as u64,
            by_evidence_class,
            closeable_source_beads,
            high_scale_claim_beads,
            blocker_groups,
            findings,
            operator_summary,
        }
    }

    /// Count records in an evidence class.
    #[must_use]
    pub fn evidence_count(&self, evidence_class: ProofEvidenceClass) -> u64 {
        self.by_evidence_class
            .get(evidence_class.as_str())
            .copied()
            .unwrap_or(0)
    }

    /// Whether any validation finding is an error.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.findings
            .iter()
            .any(|finding| finding.severity == ProofFindingSeverity::Error)
    }
}

impl ProofHistoryIndex {
    /// Build a proof-history index from retained JSONL proof artifacts.
    ///
    /// Each non-empty line in `content` must be a [`ProofAttemptRecord`].
    /// Artifact metadata is checked separately from record truthfulness so a
    /// pass record in a stale or hash-mismatched artifact cannot become release
    /// evidence accidentally.
    #[must_use]
    pub fn from_artifacts(artifacts: &[ProofHistoryArtifactInput]) -> Self {
        let mut artifact_reports = Vec::new();
        let mut records = Vec::new();
        let mut findings = Vec::new();

        for artifact in artifacts {
            let mut report = ProofHistoryArtifactReport {
                artifact_path: artifact.artifact_path.clone(),
                status: ProofHistoryArtifactStatus::Indexed,
                rows_indexed: 0,
                content_sha256: artifact.content_sha256.clone(),
                source_commit: artifact.source_commit.clone(),
                proof_category: artifact.proof_category.clone(),
                reason_code: None,
                detail: None,
            };

            if artifact.content.is_none() {
                report.status = if artifact.read_error.is_some() {
                    ProofHistoryArtifactStatus::Unreadable
                } else {
                    ProofHistoryArtifactStatus::MissingFile
                };
                report.reason_code = Some(report.status.as_str().to_string());
                report.detail = artifact
                    .read_error
                    .clone()
                    .or_else(|| Some("artifact content was not supplied".to_string()));
                artifact_reports.push(report);
                continue;
            }

            let artifact_hash_mismatch = artifact
                .content_sha256
                .as_ref()
                .zip(artifact.expected_sha256.as_ref())
                .is_some_and(|(actual, expected)| actual != expected);
            let artifact_source_commit_mismatch = artifact
                .source_commit
                .as_ref()
                .zip(artifact.expected_source_commit.as_ref())
                .is_some_and(|(actual, expected)| actual != expected);

            if artifact_hash_mismatch {
                promote_artifact_status(&mut report, ProofHistoryArtifactStatus::HashMismatch);
                report.reason_code = Some("artifact_hash_mismatch".to_string());
                report.detail = Some("artifact hash does not match expected hash".to_string());
            }

            if artifact_source_commit_mismatch {
                let prior_status = report.status;
                promote_artifact_status(
                    &mut report,
                    ProofHistoryArtifactStatus::SourceCommitMismatch,
                );
                if report.status != prior_status || report.reason_code.is_none() {
                    report.reason_code = Some("artifact_source_commit_mismatch".to_string());
                    report.detail =
                        Some("artifact source commit does not match expected commit".to_string());
                }
            }

            let content = artifact.content.as_deref().unwrap_or_default();
            let mut saw_line = false;
            for (line_index, line) in content.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                saw_line = true;

                let parsed = serde_json::from_str::<ProofAttemptRecord>(line);
                let Ok(record) = parsed else {
                    promote_artifact_status(&mut report, ProofHistoryArtifactStatus::InvalidJson);
                    report.reason_code = Some("artifact_invalid_json".to_string());
                    report.detail = Some(format!(
                        "line {} did not parse as ProofAttemptRecord",
                        line_index + 1
                    ));
                    continue;
                };

                let mut record_status = report.status;
                let mut record_findings = validate_proof_record(&record);

                if artifact_hash_mismatch {
                    record_findings.push(ProofLedgerFinding::error(
                        &record,
                        "artifact_hash_mismatch",
                        "proof artifact hash does not match the expected hash",
                    ));
                }
                if artifact_source_commit_mismatch {
                    record_findings.push(ProofLedgerFinding::error(
                        &record,
                        "artifact_source_commit_mismatch",
                        "proof artifact source commit does not match the expected commit",
                    ));
                }
                if let Some(closeout_timestamp) = artifact.bead_closed_at_utc.as_deref()
                    && proof_record_is_older_than_closeout(&record, closeout_timestamp)
                {
                    record_status = ProofHistoryArtifactStatus::Stale;
                    promote_artifact_status(&mut report, ProofHistoryArtifactStatus::Stale);
                    record_findings.push(ProofLedgerFinding::error(
                        &record,
                        "artifact_older_than_closeout",
                        "proof artifact predates the current Beads closeout timestamp",
                    ));
                    report.reason_code = Some("artifact_older_than_closeout".to_string());
                    report.detail =
                        Some("one or more records predate the Beads closeout".to_string());
                }

                findings.extend(record_findings.clone());
                report.rows_indexed += 1;
                records.push(ProofHistoryRecord {
                    artifact_path: artifact.artifact_path.clone(),
                    artifact_sha256: artifact.content_sha256.clone(),
                    source_commit: artifact.source_commit.clone(),
                    proof_category: artifact.proof_category.clone(),
                    artifact_status: record_status,
                    record,
                    findings: record_findings,
                });
            }

            if !saw_line {
                promote_artifact_status(&mut report, ProofHistoryArtifactStatus::Empty);
                report.reason_code = Some("artifact_empty".to_string());
                report.detail = Some("artifact contained no proof records".to_string());
            }

            artifact_reports.push(report);
        }

        let operator_summary =
            proof_history_operator_summary(artifact_reports.len(), records.len(), findings.len());

        Self {
            schema_version: PROOF_LANE_SCHEMA_VERSION,
            artifacts: artifact_reports,
            records,
            findings,
            operator_summary,
        }
    }
}

impl ProofReleaseScoreboard {
    /// Build a release scoreboard from an indexed proof history.
    #[must_use]
    pub fn from_history(index: &ProofHistoryIndex) -> Self {
        let mut rows = index
            .records
            .iter()
            .map(ProofReleaseScoreboardRow::from_history_record)
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| {
            (
                &left.proof_category,
                &left.bead_id,
                &left.source_commit,
                &left.artifact_sha256,
                &left.artifact_path,
            )
                .cmp(&(
                    &right.proof_category,
                    &right.bead_id,
                    &right.source_commit,
                    &right.artifact_sha256,
                    &right.artifact_path,
                ))
        });

        let mut by_bucket = BTreeMap::new();
        let mut by_proof_category = BTreeMap::new();
        let mut latest_by_bead_map = BTreeMap::<String, ProofReleaseScoreboardRow>::new();
        let mut closeable_source_beads = Vec::new();
        let mut high_scale_claim_beads = Vec::new();

        for row in &rows {
            *by_bucket
                .entry(row.bucket.as_str().to_string())
                .or_insert(0) += 1;
            *by_proof_category
                .entry(row.proof_category.clone())
                .or_insert(0) += 1;

            if row.closeout_eligible {
                push_unique_non_empty(&mut closeable_source_beads, &row.bead_id);
            }
            if row.high_scale_claim_allowed {
                push_unique_non_empty(&mut high_scale_claim_beads, &row.bead_id);
            }

            latest_by_bead_map
                .entry(row.bead_id.clone())
                .and_modify(|selected| {
                    if proof_scoreboard_row_is_newer(row, selected) {
                        *selected = row.clone();
                    }
                })
                .or_insert_with(|| row.clone());
        }

        let mut latest_by_bead = latest_by_bead_map.into_values().collect::<Vec<_>>();
        latest_by_bead.sort_by(|left, right| left.bead_id.cmp(&right.bead_id));

        let blocking_rows = rows
            .iter()
            .filter(|row| !row.closeout_eligible)
            .cloned()
            .collect::<Vec<_>>();
        let artifact_issues = index
            .artifacts
            .iter()
            .filter(|artifact| artifact.status != ProofHistoryArtifactStatus::Indexed)
            .cloned()
            .collect::<Vec<_>>();
        let operator_summary = proof_scoreboard_operator_summary(
            rows.len(),
            latest_by_bead.len(),
            closeable_source_beads.len(),
            high_scale_claim_beads.len(),
            blocking_rows.len(),
            artifact_issues.len(),
            index.findings.len(),
        );

        Self {
            schema_version: PROOF_LANE_SCHEMA_VERSION,
            total_records: rows.len() as u64,
            rows,
            latest_by_bead,
            blocking_rows,
            artifact_issues,
            by_bucket,
            by_proof_category,
            closeable_source_beads,
            high_scale_claim_beads,
            findings: index.findings.clone(),
            operator_summary,
        }
    }

    /// Return the latest scoreboard row for a Beads id.
    #[must_use]
    pub fn latest_for_bead(&self, bead_id: &str) -> Option<&ProofReleaseScoreboardRow> {
        self.latest_by_bead
            .iter()
            .find(|row| row.bead_id == bead_id)
    }

    /// Return all rows that block release or closeout eligibility.
    #[must_use]
    pub fn release_blockers(&self) -> &[ProofReleaseScoreboardRow] {
        &self.blocking_rows
    }

    /// Query canonical proof-history rows with filtering, pagination, and
    /// remote-safe path redaction.
    #[must_use]
    pub fn query(&self, query: &ProofHistoryQuery) -> ProofHistoryQueryResult {
        let limit = query.limit.clamp(1, 1000);
        let offset = query.offset;
        let candidates = if query.release_blocking_only {
            &self.blocking_rows
        } else {
            &self.rows
        };
        let matching_rows = candidates
            .iter()
            .filter(|row| proof_history_row_matches_query(row, query))
            .cloned()
            .map(|row| redact_scoreboard_row(row, query.include_local_paths))
            .collect::<Vec<_>>();
        let total_matches = matching_rows.len();
        let rows = matching_rows
            .iter()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        let next_offset = (offset + rows.len() < total_matches).then_some(offset + rows.len());
        let latest_for_bead = query
            .bead_id
            .as_deref()
            .and_then(|bead_id| self.latest_for_bead(bead_id))
            .cloned()
            .map(|row| redact_scoreboard_row(row, query.include_local_paths));
        let artifact_issues = self
            .artifact_issues
            .iter()
            .cloned()
            .map(|artifact| redact_artifact_report(artifact, query.include_local_paths))
            .collect::<Vec<_>>();
        let release_blocking_summary = self.release_blocking_summary();
        let operator_summary = proof_history_query_operator_summary(
            total_matches,
            rows.len(),
            next_offset,
            query.release_blocking_only,
        );

        ProofHistoryQueryResult {
            schema_version: PROOF_LANE_SCHEMA_VERSION,
            query: ProofHistoryQuery {
                limit,
                ..query.clone()
            },
            total_matches: total_matches as u64,
            returned_rows: rows.len() as u64,
            next_offset,
            latest_for_bead,
            rows,
            artifact_issues,
            release_blocking_summary,
            operator_summary,
        }
    }

    /// Summarize the rows that block release or source closeout.
    #[must_use]
    pub fn release_blocking_summary(&self) -> ProofHistoryReleaseBlockingSummary {
        let mut by_bucket = BTreeMap::new();
        let mut by_proof_category = BTreeMap::new();
        let mut blocking_beads = Vec::new();

        for row in &self.blocking_rows {
            *by_bucket
                .entry(row.bucket.as_str().to_string())
                .or_insert(0) += 1;
            *by_proof_category
                .entry(row.proof_category.clone())
                .or_insert(0) += 1;
            push_unique_non_empty(&mut blocking_beads, &row.bead_id);
        }

        ProofHistoryReleaseBlockingSummary {
            total_blocking_rows: self.blocking_rows.len() as u64,
            by_bucket,
            by_proof_category,
            blocking_beads,
            artifact_issue_count: self.artifact_issues.len() as u64,
            validation_finding_count: self.findings.len() as u64,
        }
    }
}

impl ProofReleaseScoreboardRow {
    fn from_history_record(history: &ProofHistoryRecord) -> Self {
        let record = &history.record;
        let validation_error_count = history
            .findings
            .iter()
            .filter(|finding| finding.severity == ProofFindingSeverity::Error)
            .count() as u64;
        let validation_warning_count = history.findings.len() as u64 - validation_error_count;
        let artifact_clean = history.artifact_status == ProofHistoryArtifactStatus::Indexed;
        let closeout_eligible =
            artifact_clean && validation_error_count == 0 && record.safe_to_close_source_bead();
        let high_scale_claim_allowed = closeout_eligible && record.allows_high_scale_claim();
        let residual_blocker = if closeout_eligible {
            None
        } else {
            history
                .findings
                .iter()
                .find(|finding| finding.severity == ProofFindingSeverity::Error)
                .map(|finding| finding.reason_code.clone())
                .or_else(|| Some(record.report_bucket().as_str().to_string()))
        };

        Self {
            proof_category: history
                .proof_category
                .clone()
                .unwrap_or_else(|| "uncategorized".to_string()),
            bead_id: record.bead_id.clone(),
            parent_bead_id: record.parent_bead_id.clone(),
            source_commit: history.source_commit.clone(),
            artifact_path: history.artifact_path.clone(),
            artifact_sha256: history.artifact_sha256.clone(),
            latest_verdict: record.state,
            bucket: record.report_bucket(),
            reason_code: record.reason_code.clone(),
            closeout_eligible,
            high_scale_claim_allowed,
            selected_worker: record.selected_worker.clone(),
            residual_blocker,
            attempted_at_utc: record.attempted_at_utc.clone(),
            finished_at_utc: record.finished_at_utc.clone(),
            artifact_status: history.artifact_status,
            validation_error_count,
            validation_warning_count,
        }
    }
}

impl ProofAttemptRecord {
    /// Evidence class used by release and swarm closeout reports.
    #[must_use]
    pub fn evidence_class(&self) -> ProofEvidenceClass {
        match self.hardware_predicate {
            Some(ProofHardwarePredicate::ProvenPredicateMet) => {
                if self.allows_high_scale_claim() {
                    ProofEvidenceClass::HighScaleProven
                } else {
                    ProofEvidenceClass::HighScaleNotProven
                }
            }
            Some(ProofHardwarePredicate::SkippedNotProven) => {
                ProofEvidenceClass::HighScaleSkippedNotProven
            }
            Some(ProofHardwarePredicate::RemoteReduced) => ProofEvidenceClass::RemoteReduced,
            Some(ProofHardwarePredicate::LocalReduced) => ProofEvidenceClass::LocalReduced,
            Some(ProofHardwarePredicate::Unknown) | None => ProofEvidenceClass::Unknown,
        }
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

fn closeout_operator_summary(
    total_records: usize,
    closeable_source_beads: usize,
    high_scale_claim_beads: usize,
    blocker_groups: usize,
    by_evidence_class: &BTreeMap<String, u64>,
    finding_count: usize,
) -> String {
    let high_scale_proven = by_evidence_class
        .get(ProofEvidenceClass::HighScaleProven.as_str())
        .copied()
        .unwrap_or(0);
    let high_scale_not_proven = by_evidence_class
        .get(ProofEvidenceClass::HighScaleNotProven.as_str())
        .copied()
        .unwrap_or(0)
        + by_evidence_class
            .get(ProofEvidenceClass::HighScaleSkippedNotProven.as_str())
            .copied()
            .unwrap_or(0);
    let remote_reduced = by_evidence_class
        .get(ProofEvidenceClass::RemoteReduced.as_str())
        .copied()
        .unwrap_or(0);
    let local_reduced = by_evidence_class
        .get(ProofEvidenceClass::LocalReduced.as_str())
        .copied()
        .unwrap_or(0);

    format!(
        "records={total_records}; closeable_source_beads={closeable_source_beads}; \
         high_scale_claim_beads={high_scale_claim_beads}; blocker_groups={blocker_groups}; \
         high_scale_proven={high_scale_proven}; high_scale_not_proven={high_scale_not_proven}; \
         remote_reduced={remote_reduced}; local_reduced={local_reduced}; \
         findings={finding_count}"
    )
}

fn push_unique_non_empty(values: &mut Vec<String>, value: &str) {
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == value) {
        values.push(value.to_string());
    }
}

fn promote_artifact_status(
    report: &mut ProofHistoryArtifactReport,
    candidate: ProofHistoryArtifactStatus,
) {
    if proof_history_status_rank(candidate) > proof_history_status_rank(report.status) {
        report.status = candidate;
    }
}

const fn proof_history_status_rank(status: ProofHistoryArtifactStatus) -> u8 {
    match status {
        ProofHistoryArtifactStatus::Indexed => 0,
        ProofHistoryArtifactStatus::Empty => 10,
        ProofHistoryArtifactStatus::Stale => 20,
        ProofHistoryArtifactStatus::SourceCommitMismatch => 30,
        ProofHistoryArtifactStatus::HashMismatch => 40,
        ProofHistoryArtifactStatus::InvalidJson => 50,
        ProofHistoryArtifactStatus::Unreadable => 60,
        ProofHistoryArtifactStatus::MissingFile => 70,
    }
}

fn proof_record_is_older_than_closeout(
    record: &ProofAttemptRecord,
    closeout_timestamp: &str,
) -> bool {
    let closeout_timestamp = closeout_timestamp.trim();
    if closeout_timestamp.is_empty() {
        return false;
    }

    proof_record_observed_at(record).is_some_and(|observed| observed < closeout_timestamp)
}

fn proof_record_observed_at(record: &ProofAttemptRecord) -> Option<&str> {
    record
        .finished_at_utc
        .as_deref()
        .filter(|timestamp| !timestamp.trim().is_empty())
        .or_else(|| {
            if record.attempted_at_utc.trim().is_empty() {
                None
            } else {
                Some(record.attempted_at_utc.as_str())
            }
        })
}

fn proof_scoreboard_row_is_newer(
    candidate: &ProofReleaseScoreboardRow,
    selected: &ProofReleaseScoreboardRow,
) -> bool {
    let candidate_ts = candidate
        .finished_at_utc
        .as_deref()
        .filter(|timestamp| !timestamp.trim().is_empty())
        .unwrap_or(&candidate.attempted_at_utc);
    let selected_ts = selected
        .finished_at_utc
        .as_deref()
        .filter(|timestamp| !timestamp.trim().is_empty())
        .unwrap_or(&selected.attempted_at_utc);

    candidate_ts > selected_ts
        || (candidate_ts == selected_ts
            && proof_state_rank(candidate.latest_verdict)
                > proof_state_rank(selected.latest_verdict))
}

fn proof_history_row_matches_query(
    row: &ProofReleaseScoreboardRow,
    query: &ProofHistoryQuery,
) -> bool {
    query
        .bead_id
        .as_deref()
        .is_none_or(|bead_id| row.bead_id == bead_id)
        && query
            .proof_category
            .as_deref()
            .is_none_or(|category| row.proof_category == category)
        && query
            .status
            .is_none_or(|status| row.latest_verdict == status)
}

fn redact_scoreboard_row(
    mut row: ProofReleaseScoreboardRow,
    include_local_paths: bool,
) -> ProofReleaseScoreboardRow {
    row.artifact_path = redact_local_path(&row.artifact_path, include_local_paths);
    row
}

fn redact_artifact_report(
    mut report: ProofHistoryArtifactReport,
    include_local_paths: bool,
) -> ProofHistoryArtifactReport {
    report.artifact_path = redact_local_path(&report.artifact_path, include_local_paths);
    report
}

fn redact_local_path(path: &str, include_local_paths: bool) -> String {
    if include_local_paths || !path.starts_with('/') {
        return path.to_string();
    }

    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("artifact");
    format!("<local>/{file_name}")
}

fn proof_history_query_operator_summary(
    total_matches: usize,
    returned_rows: usize,
    next_offset: Option<usize>,
    release_blocking_only: bool,
) -> String {
    let mode = if release_blocking_only {
        "release_blocking"
    } else {
        "all"
    };
    match next_offset {
        Some(next) => format!(
            "mode={mode}; matches={total_matches}; returned={returned_rows}; next_offset={next}"
        ),
        None => format!("mode={mode}; matches={total_matches}; returned={returned_rows}"),
    }
}

fn proof_history_operator_summary(
    artifact_count: usize,
    record_count: usize,
    finding_count: usize,
) -> String {
    format!("artifacts={artifact_count}; records={record_count}; findings={finding_count}")
}

fn proof_scoreboard_operator_summary(
    row_count: usize,
    latest_count: usize,
    closeable_count: usize,
    high_scale_count: usize,
    blocker_count: usize,
    artifact_issue_count: usize,
    finding_count: usize,
) -> String {
    format!(
        "rows={row_count}; latest_beads={latest_count}; closeable_source_beads={closeable_count}; \
         high_scale_claim_beads={high_scale_count}; blockers={blocker_count}; \
         artifact_issues={artifact_issue_count}; findings={finding_count}"
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

    fn projected_record_from_doctor(
        input: &ProofDoctorPreflightInput,
        redaction_status: ProofRedactionStatus,
    ) -> ProofAttemptRecord {
        let verdict = classify_proof_doctor(input);
        ProofAttemptRecord::from_proof_doctor_verdict(&verdict, redaction_status)
    }

    fn proof_closeout_lint_input(
        closeout_text: &str,
        artifacts: Vec<ProofCloseoutLintArtifact>,
    ) -> ProofCloseoutLintInput {
        ProofCloseoutLintInput {
            bead_id: Some("ft-test".into()),
            closeout_text: Some(closeout_text.into()),
            required_backend: ProofBackend::Rch,
            dirty_tree: false,
            artifacts,
        }
    }

    fn lint_artifact(record: ProofAttemptRecord) -> ProofCloseoutLintArtifact {
        ProofCloseoutLintArtifact {
            artifact_path: "docs/attestations/proof-ledger/ft-test.jsonl".into(),
            records: vec![record],
            read_error: None,
        }
    }

    fn lint_finding_codes(report: &ProofCloseoutLintReport) -> Vec<&str> {
        report
            .findings
            .iter()
            .map(|finding| finding.reason_code.as_str())
            .collect()
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
    fn proof_doctor_pass_projection_validates_and_round_trips() {
        let mut input = base_doctor_input();
        input.evidence.remote_cargo_reached = true;
        input.evidence.rustc_reached = true;
        input.evidence.test_binary_started = true;
        input.evidence.remote_exit_code = Some(0);
        input.evidence.wrapper_exit_code = Some(0);
        input.evidence.selected_worker = Some("vmi1153651".into());
        input.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        input.evidence.artifact_paths = vec!["tests/e2e/artifacts/proof/pass/summary.json".into()];

        let record = projected_record_from_doctor(&input, ProofRedactionStatus::NoneNeeded);
        let serialized = serde_json::to_string(&record).expect("record serializes");
        let round_trip: ProofAttemptRecord =
            serde_json::from_str(&serialized).expect("record deserializes");

        assert_eq!(round_trip.state, ProofState::Pass);
        assert!(round_trip.safe_to_close_source_bead());
        assert_eq!(
            round_trip
                .proof_doctor
                .as_ref()
                .map(|snapshot| snapshot.status),
            Some(ProofDoctorStatus::Passed)
        );
        assert!(validate_proof_record(&round_trip).is_empty());
    }

    #[test]
    fn proof_doctor_infra_source_and_test_projection_records_validate() {
        let mut infra = base_doctor_input();
        infra.evidence.selected_worker = Some("vmi1293453".into());
        infra.evidence.sync_duration_ms = Some(140_454);
        infra.evidence.wrapper_exit_code = Some(1);
        infra.evidence.rch_failure_reason_code = Some("RCH-REMOTE-MIRROR-MISSING-FILE".into());
        infra.evidence.rch_failure_reason_detail =
            Some("missing crates/frankenterm-alloc/Cargo.toml".into());

        let mut source = base_doctor_input();
        source.evidence.remote_cargo_reached = true;
        source.evidence.rustc_reached = true;
        source.evidence.remote_exit_code = Some(101);
        source.evidence.diagnostic_summary = Some("missing field initializer".into());
        source.evidence.diagnostic_paths = vec!["crates/frankenterm-core/src/proof_lane.rs".into()];

        let mut test = source.clone();
        test.evidence.test_binary_started = true;
        test.evidence.diagnostic_summary = Some("assertion failed".into());

        let records = [
            (
                projected_record_from_doctor(&infra, ProofRedactionStatus::Unknown),
                ProofState::InfraBlockedPreCargo,
                ProofDoctorStatus::InfraBlocked,
            ),
            (
                projected_record_from_doctor(&source, ProofRedactionStatus::Unknown),
                ProofState::SourceCompileFail,
                ProofDoctorStatus::SourceBlocked,
            ),
            (
                projected_record_from_doctor(&test, ProofRedactionStatus::Unknown),
                ProofState::TestFail,
                ProofDoctorStatus::TestBlocked,
            ),
        ];

        for (record, state, doctor_status) in records {
            assert_eq!(record.state, state);
            assert_eq!(
                record.proof_doctor.as_ref().map(|snapshot| snapshot.status),
                Some(doctor_status)
            );
            assert!(
                validate_proof_record(&record)
                    .iter()
                    .all(|finding| finding.severity != ProofFindingSeverity::Error),
                "{state:?} projection should not have validation errors"
            );
        }
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
    fn closeout_report_groups_blockers_and_evidence_classes() {
        let mut remote_reduced = base_record(ProofState::Pass);
        remote_reduced.proof_id = "remote-reduced".into();
        remote_reduced.bead_id = "ft-storage-reduced".into();
        remote_reduced.remote_cargo_reached = true;
        remote_reduced.rustc_reached = true;
        remote_reduced.test_binary_started = true;
        remote_reduced.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        remote_reduced.hardware_predicate = Some(ProofHardwarePredicate::RemoteReduced);

        let mut high_scale = base_record(ProofState::Pass);
        high_scale.proof_id = "high-scale-pass".into();
        high_scale.bead_id = "ft-tn6cw-gauntlet".into();
        high_scale.remote_cargo_reached = true;
        high_scale.rustc_reached = true;
        high_scale.test_binary_started = true;
        high_scale.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        high_scale.hardware_predicate = Some(ProofHardwarePredicate::ProvenPredicateMet);

        let mut infra = base_record(ProofState::InfraBlockedPreCargo);
        infra.proof_id = "infra-blocked".into();
        infra.bead_id = "ft-tn6cw.1".into();
        infra.reason_code = "proof.infra.pre_cargo.rch_timeout_wrapper".into();
        infra.summary = "installed RCH failed before Cargo".into();
        infra.next_action = "fix RCH wrapper before rerunning release proof".into();

        let mut local_invalid = base_record(ProofState::LocalInvalid);
        local_invalid.proof_id = "local-invalid".into();
        local_invalid.bead_id = "ft-luq3w.4".into();
        local_invalid.observed_backend = ProofBackend::LocalShell;
        local_invalid.local_cargo_detected = true;
        local_invalid.artifact_retrieval_status = ArtifactRetrievalStatus::NotApplicable;
        local_invalid.hardware_predicate = Some(ProofHardwarePredicate::LocalReduced);
        local_invalid.reason_code = "proof.local_invalid.shell_wrapped_cargo".into();
        local_invalid.next_action = "rerun through rch with a direct cargo argv".into();

        let mut skipped = base_record(ProofState::SkippedNotProven);
        skipped.proof_id = "high-scale-skipped".into();
        skipped.bead_id = "ft-tn6cw.high-scale".into();
        skipped.hardware_predicate = Some(ProofHardwarePredicate::SkippedNotProven);
        skipped.reason_code = "proof.high_scale.skipped_not_proven".into();
        skipped.next_action = "run on 64+ CPU / 256+ GiB hardware before claiming PROVEN".into();

        let report = ProofCloseoutReport::from_records(&[
            remote_reduced,
            high_scale,
            infra,
            local_invalid,
            skipped,
        ]);

        assert_eq!(report.total_records, 5);
        assert_eq!(report.evidence_count(ProofEvidenceClass::RemoteReduced), 1);
        assert_eq!(
            report.evidence_count(ProofEvidenceClass::HighScaleProven),
            1
        );
        assert_eq!(
            report.evidence_count(ProofEvidenceClass::HighScaleSkippedNotProven),
            1
        );
        assert_eq!(report.evidence_count(ProofEvidenceClass::LocalReduced), 1);
        assert_eq!(
            report.closeable_source_beads,
            vec![
                "ft-storage-reduced".to_string(),
                "ft-tn6cw-gauntlet".to_string()
            ]
        );
        assert_eq!(
            report.high_scale_claim_beads,
            vec!["ft-tn6cw-gauntlet".to_string()]
        );

        let infra_group = report
            .blocker_groups
            .iter()
            .find(|group| group.bucket == ProofReportBucket::PreCargoInfrastructureBlocker)
            .expect("pre-Cargo blocker group");
        assert_eq!(infra_group.bead_ids, vec!["ft-tn6cw.1".to_string()]);
        assert!(
            infra_group
                .next_actions
                .contains(&"fix RCH wrapper before rerunning release proof".to_string())
        );

        let local_group = report
            .blocker_groups
            .iter()
            .find(|group| group.bucket == ProofReportBucket::InvalidLocalProof)
            .expect("local-invalid blocker group");
        assert_eq!(
            local_group.reason_codes,
            vec!["proof.local_invalid.shell_wrapped_cargo".to_string()]
        );

        let skipped_group = report
            .blocker_groups
            .iter()
            .find(|group| group.bucket == ProofReportBucket::SkippedNotProven)
            .expect("skipped high-scale blocker group");
        assert!(
            skipped_group
                .next_actions
                .iter()
                .any(|action| action.contains("64+ CPU / 256+ GiB"))
        );

        assert!(!report.has_errors());
        assert!(report.operator_summary.contains("high_scale_proven=1"));
        assert!(report.operator_summary.contains("high_scale_not_proven=1"));
        assert!(report.operator_summary.contains("remote_reduced=1"));
        assert!(report.operator_summary.contains("local_reduced=1"));
    }

    #[test]
    fn proof_history_scoreboard_indexes_jsonl_and_preserves_blocker_taxonomy() {
        let mut pass = base_record(ProofState::Pass);
        pass.proof_id = "pass".into();
        pass.bead_id = "ft-pass".into();
        pass.parent_bead_id = Some("ft-parent".into());
        pass.remote_cargo_reached = true;
        pass.rustc_reached = true;
        pass.test_binary_started = true;
        pass.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        pass.remote_exit_code = Some(0);
        pass.wrapper_exit_code = Some(0);
        pass.selected_worker = Some("vmi1152480".into());

        let mut source = base_record(ProofState::SourceCompileFail);
        source.proof_id = "source".into();
        source.bead_id = "ft-source".into();
        source.remote_cargo_reached = true;
        source.rustc_reached = true;
        source.remote_exit_code = Some(101);
        source.wrapper_exit_code = Some(101);
        source.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let mut infra = base_record(ProofState::InfraBlockedPreCargo);
        infra.proof_id = "infra".into();
        infra.bead_id = "ft-infra".into();

        let mut dirty = base_record(ProofState::Inconclusive);
        dirty.proof_id = "dirty".into();
        dirty.bead_id = "ft-dirty".into();
        dirty.reason_code = "proof.dirty.active_owned_path_overlap".into();
        dirty.summary = "dirty path overlaps proof lane".into();
        dirty = dirty.with_proof_doctor_verdict(&classify_proof_doctor(&{
            let mut input = base_doctor_input();
            input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
                path: "crates/frankenterm-core/src/storage.rs".into(),
                status: " M".into(),
                affects_proof: true,
                owner: Some(ProofDoctorOwner::Bead {
                    bead_id: "ft-owner".into(),
                    assignee: Some("SageRobin".into()),
                }),
            });
            input
        }));

        let mut skipped = base_record(ProofState::SkippedNotProven);
        skipped.proof_id = "skipped".into();
        skipped.bead_id = "ft-high-scale".into();
        skipped.hardware_predicate = Some(ProofHardwarePredicate::SkippedNotProven);
        skipped.reason_code = "proof.high_scale.predicate_absent".into();

        let content = records_to_jsonl(&[pass, source, infra, dirty, skipped]);
        let mut artifact =
            ProofHistoryArtifactInput::new("tests/e2e/artifacts/proof/records.jsonl", content);
        artifact.content_sha256 = Some("sha256:fixture".into());
        artifact.source_commit = Some("651d8a538".into());
        artifact.proof_category = Some("release/proof-handoff".into());

        let index = ProofHistoryIndex::from_artifacts(&[artifact]);
        let scoreboard = ProofReleaseScoreboard::from_history(&index);

        assert_eq!(index.records.len(), 5);
        assert_eq!(scoreboard.total_records, 5);
        assert_eq!(
            scoreboard
                .by_bucket
                .get(ProofReportBucket::RemoteProofPassed.as_str()),
            Some(&1)
        );
        assert_eq!(
            scoreboard
                .by_bucket
                .get(ProofReportBucket::SourceRed.as_str()),
            Some(&1)
        );
        assert_eq!(
            scoreboard
                .by_bucket
                .get(ProofReportBucket::DirtyTreeBlocked.as_str()),
            Some(&1)
        );
        assert_eq!(
            scoreboard
                .by_bucket
                .get(ProofReportBucket::SkippedNotProven.as_str()),
            Some(&1)
        );
        assert_eq!(
            scoreboard.closeable_source_beads,
            vec!["ft-pass".to_string()]
        );
        assert!(scoreboard.high_scale_claim_beads.is_empty());
        assert_eq!(
            scoreboard
                .latest_for_bead("ft-pass")
                .expect("latest pass row")
                .selected_worker
                .as_deref(),
            Some("vmi1152480")
        );
        assert!(
            scoreboard
                .blocking_rows
                .iter()
                .any(|row| row.bead_id == "ft-high-scale"
                    && row.residual_blocker.as_deref() == Some("skipped_not_proven"))
        );
    }

    #[test]
    fn proof_history_detects_missing_invalid_hash_source_and_stale_artifacts() {
        let mut record = base_record(ProofState::Pass);
        record.proof_id = "stale-pass".into();
        record.bead_id = "ft-stale".into();
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.test_binary_started = true;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let mut stale_artifact = ProofHistoryArtifactInput::new(
            "stale.jsonl",
            records_to_jsonl(std::slice::from_ref(&record)),
        );
        stale_artifact.bead_closed_at_utc = Some("2026-05-05T00:00:02Z".into());

        let mut hash_source_artifact =
            ProofHistoryArtifactInput::new("hash-source.jsonl", records_to_jsonl(&[record]));
        hash_source_artifact.content_sha256 = Some("sha256:actual".into());
        hash_source_artifact.expected_sha256 = Some("sha256:expected".into());
        hash_source_artifact.source_commit = Some("newer".into());
        hash_source_artifact.expected_source_commit = Some("older".into());

        let invalid_artifact = ProofHistoryArtifactInput::new("invalid.jsonl", "{not json}");
        let missing_artifact = ProofHistoryArtifactInput::unavailable("missing.jsonl", None);
        let unreadable_artifact = ProofHistoryArtifactInput::unavailable(
            "unreadable.jsonl",
            Some("permission denied".into()),
        );

        let index = ProofHistoryIndex::from_artifacts(&[
            stale_artifact,
            hash_source_artifact,
            invalid_artifact,
            missing_artifact,
            unreadable_artifact,
        ]);
        let scoreboard = ProofReleaseScoreboard::from_history(&index);

        assert_eq!(index.artifacts.len(), 5);
        assert!(
            index
                .artifacts
                .iter()
                .any(|artifact| artifact.status == ProofHistoryArtifactStatus::Stale)
        );
        assert!(
            index
                .artifacts
                .iter()
                .any(|artifact| artifact.status == ProofHistoryArtifactStatus::HashMismatch)
        );
        assert!(
            index
                .findings
                .iter()
                .any(|finding| finding.reason_code == "artifact_source_commit_mismatch")
        );
        assert!(
            index
                .artifacts
                .iter()
                .any(|artifact| artifact.status == ProofHistoryArtifactStatus::InvalidJson)
        );
        assert!(
            index
                .artifacts
                .iter()
                .any(|artifact| artifact.status == ProofHistoryArtifactStatus::MissingFile)
        );
        assert!(
            index
                .artifacts
                .iter()
                .any(|artifact| artifact.status == ProofHistoryArtifactStatus::Unreadable)
        );
        assert!(
            scoreboard
                .latest_for_bead("ft-stale")
                .is_some_and(|row| !row.closeout_eligible
                    && row.residual_blocker.as_deref() == Some("artifact_older_than_closeout"))
        );
        assert_eq!(scoreboard.artifact_issues.len(), 5);
    }

    #[test]
    fn proof_history_latest_for_bead_uses_newest_timestamp_not_strongest_state() {
        let mut older_pass = base_record(ProofState::Pass);
        older_pass.proof_id = "older-pass".into();
        older_pass.bead_id = "ft-latest".into();
        older_pass.finished_at_utc = Some("2026-05-05T00:00:01Z".into());
        older_pass.remote_cargo_reached = true;
        older_pass.rustc_reached = true;
        older_pass.test_binary_started = true;
        older_pass.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;

        let mut newer_infra = base_record(ProofState::InfraBlockedPreCargo);
        newer_infra.proof_id = "newer-infra".into();
        newer_infra.bead_id = "ft-latest".into();
        newer_infra.attempted_at_utc = "2026-05-05T00:01:00Z".into();
        newer_infra.finished_at_utc = Some("2026-05-05T00:01:30Z".into());

        let artifact = ProofHistoryArtifactInput::new(
            "latest.jsonl",
            records_to_jsonl(&[older_pass, newer_infra]),
        );
        let index = ProofHistoryIndex::from_artifacts(&[artifact]);
        let scoreboard = ProofReleaseScoreboard::from_history(&index);

        let latest = scoreboard
            .latest_for_bead("ft-latest")
            .expect("latest row for bead");
        assert_eq!(latest.latest_verdict, ProofState::InfraBlockedPreCargo);
        assert!(!latest.closeout_eligible);
    }

    #[test]
    fn proof_history_query_filters_paginates_and_redacts_absolute_paths() {
        let mut pass = base_record(ProofState::Pass);
        pass.proof_id = "pass-query".into();
        pass.bead_id = "ft-query-pass".into();
        pass.remote_cargo_reached = true;
        pass.rustc_reached = true;
        pass.test_binary_started = true;
        pass.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        pass.selected_worker = Some("vmi-query".into());

        let mut blocked = base_record(ProofState::InfraBlockedPreCargo);
        blocked.proof_id = "blocked-query".into();
        blocked.bead_id = "ft-query-blocked".into();
        blocked.reason_code = "proof.rch.queue_timeout_before_assignment".into();

        let mut pass_artifact = ProofHistoryArtifactInput::new(
            "/Users/operator/frankenterm/proof-pass.jsonl",
            records_to_jsonl(&[pass]),
        );
        pass_artifact.content_sha256 = Some("sha256:pass".into());
        pass_artifact.proof_category = Some("release/proof-handoff".into());
        let mut blocked_artifact = ProofHistoryArtifactInput::new(
            "tests/e2e/artifacts/proof-blocked.jsonl",
            records_to_jsonl(&[blocked]),
        );
        blocked_artifact.proof_category = Some("release/proof-handoff".into());

        let index = ProofHistoryIndex::from_artifacts(&[pass_artifact, blocked_artifact]);
        let scoreboard = ProofReleaseScoreboard::from_history(&index);
        let page = scoreboard.query(&ProofHistoryQuery {
            proof_category: Some("release/proof-handoff".into()),
            status: Some(ProofState::Pass),
            limit: 1,
            ..ProofHistoryQuery::default()
        });

        assert_eq!(page.total_matches, 1);
        assert_eq!(page.returned_rows, 1);
        assert_eq!(page.rows[0].bead_id, "ft-query-pass");
        assert_eq!(page.rows[0].artifact_path, "<local>/proof-pass.jsonl");
        assert_eq!(
            page.rows[0].artifact_sha256,
            Some("sha256:pass".to_string())
        );
        assert!(page.next_offset.is_none());

        let local_paths = scoreboard.query(&ProofHistoryQuery {
            bead_id: Some("ft-query-pass".into()),
            include_local_paths: true,
            ..ProofHistoryQuery::default()
        });
        assert_eq!(
            local_paths
                .latest_for_bead
                .expect("latest row")
                .artifact_path,
            "/Users/operator/frankenterm/proof-pass.jsonl"
        );
    }

    #[test]
    fn proof_history_query_reports_release_blockers_and_missing_artifacts() {
        let mut blocked = base_record(ProofState::InfraBlockedPreCargo);
        blocked.proof_id = "blocked-release".into();
        blocked.bead_id = "ft-blocking".into();
        blocked.reason_code = "proof.rch.pre_cargo_timeout_exec_missing".into();

        let artifact = ProofHistoryArtifactInput::new(
            "tests/e2e/artifacts/proof-blocking.jsonl",
            records_to_jsonl(&[blocked]),
        );
        let missing = ProofHistoryArtifactInput::unavailable("/var/tmp/missing-proof.jsonl", None);
        let index = ProofHistoryIndex::from_artifacts(&[artifact, missing]);
        let scoreboard = ProofReleaseScoreboard::from_history(&index);
        let blockers = scoreboard.query(&ProofHistoryQuery {
            release_blocking_only: true,
            limit: 10,
            ..ProofHistoryQuery::default()
        });

        assert_eq!(blockers.total_matches, 1);
        assert_eq!(blockers.rows[0].bead_id, "ft-blocking");
        assert_eq!(
            blockers.rows[0].residual_blocker.as_deref(),
            Some("pre_cargo_infra_blocked")
        );
        assert_eq!(blockers.artifact_issues.len(), 1);
        assert_eq!(
            blockers.artifact_issues[0].artifact_path,
            "<local>/missing-proof.jsonl"
        );
        assert_eq!(
            blockers.release_blocking_summary.total_blocking_rows,
            scoreboard.blocking_rows.len() as u64
        );
        assert_eq!(blockers.release_blocking_summary.artifact_issue_count, 1);
        assert!(
            blockers
                .release_blocking_summary
                .blocking_beads
                .contains(&"ft-blocking".to_string())
        );
    }

    fn records_to_jsonl(records: &[ProofAttemptRecord]) -> String {
        let mut jsonl = String::new();
        for record in records {
            jsonl.push_str(&serde_json::to_string(record).expect("serialize proof record"));
            jsonl.push('\n');
        }
        jsonl
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
    fn proof_closeout_linter_accepts_valid_remote_pass() {
        let mut record = base_record(ProofState::Pass);
        record.selected_worker = Some("vmi-proof".into());
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.test_binary_started = true;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        let input = proof_closeout_lint_input(
            "Proof-doctor: passed; closeout safe.",
            vec![lint_artifact(record)],
        );

        let report = lint_proof_closeout(&input);

        assert!(report.closeout_eligible);
        assert_eq!(report.proof_records_analyzed, 1);
        assert!(report.findings.is_empty());
        assert!(report.suggested_beads_wording.contains("closeout safe"));
    }

    #[test]
    fn proof_closeout_linter_accepts_complete_text_with_artifact_path() {
        let input = proof_closeout_lint_input(
            "Proof-doctor: passed; selected_worker vmi-proof; command: rch exec -- cargo test; remote Cargo reached; rustc reached; test binary passed; proof lane passed; closeout safe.",
            vec![ProofCloseoutLintArtifact {
                artifact_path: "tests/e2e/artifacts/proof-closeout-text.json".into(),
                records: Vec::new(),
                read_error: None,
            }],
        );

        let report = lint_proof_closeout(&input);

        assert!(report.closeout_eligible);
        assert_eq!(report.proof_records_analyzed, 0);
        assert!(report.findings.is_empty());
        assert!(
            report
                .suggested_beads_wording
                .contains("validation text_fields_ok")
        );
    }

    #[test]
    fn proof_closeout_linter_accepts_valid_source_failure_handoff() {
        let mut record = base_record(ProofState::SourceCompileFail);
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.remote_exit_code = Some(101);
        record.reason_code = "proof.source.remote_compile_error".into();
        record.summary = "remote rustc reported a first-party source error".into();
        let input = proof_closeout_lint_input(
            "Proof-doctor: source_blocked; remote Cargo reached; closeout blocked.",
            vec![lint_artifact(record)],
        );

        let report = lint_proof_closeout(&input);

        assert!(!report.closeout_eligible);
        assert!(report.findings.is_empty());
        assert!(report.suggested_beads_wording.contains("source_blocked"));
    }

    #[test]
    fn proof_closeout_linter_accepts_valid_infra_blocked_handoff() {
        let mut record = base_record(ProofState::InfraBlockedPreCargo);
        record.reason_code = "proof.rch.queue_timeout_before_assignment".into();
        record.summary = "RCH timed out before assigning a remote worker.".into();
        record.remote_cargo_reached = false;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Partial;
        let input = proof_closeout_lint_input(
            "Proof-doctor: infra_blocked; remote Cargo not reached; closeout blocked.",
            vec![lint_artifact(record)],
        );

        let report = lint_proof_closeout(&input);

        assert!(!report.closeout_eligible);
        assert!(report.findings.is_empty());
        assert!(report.suggested_beads_wording.contains("infra_blocked"));
    }

    #[test]
    fn proof_closeout_linter_rejects_local_fallback_as_rch_proof() {
        let input = proof_closeout_lint_input(
            "Local fallback cargo test passed, so RCH proof passed and closeout safe.",
            vec![ProofCloseoutLintArtifact {
                artifact_path: "tests/e2e/artifacts/local-smoke.json".into(),
                records: Vec::new(),
                read_error: None,
            }],
        );

        let report = lint_proof_closeout(&input);
        let codes = lint_finding_codes(&report);

        assert!(!report.closeout_eligible);
        assert!(codes.contains(&"proof.closeout.local_fallback_claimed_as_proof"));
    }

    #[test]
    fn proof_closeout_linter_rejects_sync_only_green_claim() {
        let input = proof_closeout_lint_input(
            "Selected worker vmi-proof and sync completed; proof lane passed and closeout safe.",
            vec![ProofCloseoutLintArtifact {
                artifact_path: "tests/e2e/artifacts/rch-sync.json".into(),
                records: Vec::new(),
                read_error: None,
            }],
        );

        let report = lint_proof_closeout(&input);
        let codes = lint_finding_codes(&report);

        assert!(!report.closeout_eligible);
        assert!(codes.contains(&"proof.closeout.sync_or_queue_claimed_as_proof"));
    }

    #[test]
    fn proof_closeout_linter_rejects_stale_ci_rollup_green_claim() {
        let input = proof_closeout_lint_input(
            "Queued workflow status is enough: GitHub Actions queued and proof passed, closeout safe.",
            vec![ProofCloseoutLintArtifact {
                artifact_path: "tests/e2e/artifacts/queued-ci.json".into(),
                records: Vec::new(),
                read_error: None,
            }],
        );

        let report = lint_proof_closeout(&input);
        let codes = lint_finding_codes(&report);

        assert!(!report.closeout_eligible);
        assert!(codes.contains(&"proof.closeout.sync_or_queue_claimed_as_proof"));
    }

    #[test]
    fn proof_closeout_linter_rejects_missing_artifact_path() {
        let input = proof_closeout_lint_input(
            "Remote proof passed and closeout safe.",
            vec![ProofCloseoutLintArtifact {
                artifact_path: "missing-proof-record.jsonl".into(),
                records: Vec::new(),
                read_error: Some("No such file or directory".into()),
            }],
        );

        let report = lint_proof_closeout(&input);
        let codes = lint_finding_codes(&report);

        assert!(!report.closeout_eligible);
        assert!(codes.contains(&"proof.closeout.artifact_unavailable"));
        assert!(codes.contains(&"proof.closeout.remote_claim_missing_fields"));
    }

    #[test]
    fn proof_closeout_linter_requires_dirty_tree_caveat_for_green_claims() {
        let mut record = base_record(ProofState::Pass);
        record.selected_worker = Some("vmi-proof".into());
        record.remote_cargo_reached = true;
        record.rustc_reached = true;
        record.test_binary_started = true;
        record.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        let mut input = proof_closeout_lint_input(
            "Proof-doctor: passed; closeout safe.",
            vec![lint_artifact(record)],
        );
        input.dirty_tree = true;

        let report = lint_proof_closeout(&input);
        let codes = lint_finding_codes(&report);

        assert!(!report.closeout_eligible);
        assert!(codes.contains(&"proof.closeout.dirty_tree_caveat_missing"));
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
