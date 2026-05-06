#![allow(clippy::module_name_repetitions)]
#![allow(clippy::struct_excessive_bools)]

//! Proof-doctor preflight DTOs and classifier substrate for `ft-wik9p.3`.
//!
//! This module is intentionally pure. It does not run `git`, `br`, Agent Mail,
//! RCH, or Cargo. Callers collect those observations and pass them here so the
//! resulting verdict can be reused by CLI, robot-mode, Beads comments, Agent
//! Mail handoffs, and proof-lane ledger projections.

use serde::{Deserialize, Serialize};

use crate::proof_lane::{ArtifactRetrievalStatus, ProofBackend, ProofScope, ProofState};

/// Proof-doctor schema version implemented by this module.
pub const PROOF_DOCTOR_SCHEMA_VERSION: u32 = 1;
const DEFAULT_SCALE_LAB_REQUIRED_RELEASE_CLAIM_STATUS: &str = "real-hardware-proven";
const DEFAULT_SCALE_LAB_REQUIRED_MANIFEST_STATUS: &str = "proven";
const DEFAULT_SCALE_LAB_REQUIRED_EVIDENCE_MODE: &str = "real_hardware_run";
const DEFAULT_SCALE_LAB_REQUIRED_PANE_SCALES: &[u64] = &[50, 200, 1_000];
const DEFAULT_SCALE_LAB_MIN_LOGICAL_CORES: u64 = 64;
const DEFAULT_SCALE_LAB_MIN_MEMORY_BYTES: u64 = 256 * 1024 * 1024 * 1024;

/// Preflight or post-launch phase inspected by proof-doctor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofDoctorPhase {
    /// No material proof command has run.
    Preflight,
    /// Backend launch began, but only early evidence exists.
    LaunchObserved,
    /// Retained logs prove remote Cargo or rustc started.
    RemoteCargoObserved,
    /// A terminal proof state or blocker has enough evidence for handoff.
    TerminalClassified,
    /// Required logs or metadata are missing.
    EvidenceGap,
}

/// Top-level operator decision for a proof-doctor verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofDoctorStatus {
    /// No known preflight blocker.
    Runnable,
    /// Existing evidence proves the required lane passed.
    Passed,
    /// Remote Cargo/rustc found code-owned failure.
    SourceBlocked,
    /// Tests, benches, or E2E assertions failed.
    TestBlocked,
    /// RCH, worker, shell, timeout, substrate, or artifact retrieval blocked proof.
    InfraBlocked,
    /// Dirty files make the proof unsafe or unattributable.
    DirtyTreeBlocked,
    /// A different active owner owns the blocker.
    OwnershipBlocked,
    /// Command shape or backend is off-policy for the claimed proof.
    Invalid,
    /// Required predicate is absent and the claim is skipped, not proven.
    SkippedNotProven,
    /// Evidence is incomplete or contradictory.
    Inconclusive,
}

/// Category for one proof-doctor blocker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofDoctorBlockerKind {
    /// Installed or selected RCH cannot satisfy required behavior.
    RchTooling,
    /// Worker capacity, health, hardware, or admission is insufficient.
    WorkerCapacity,
    /// Repo sync or transfer failed before Cargo.
    RemoteSync,
    /// Remote wrapper or shell failed before Cargo.
    RemoteLaunch,
    /// Cargo started, but remote substrate prevented complete evidence.
    RemoteSubstrate,
    /// First-party compile, feature, lint, or build-script error.
    SourceCompile,
    /// Test, bench, E2E, or harness assertion failure.
    TestAssertion,
    /// Dirty tracked or untracked path affects the lane.
    DirtyTree,
    /// Another Bead, reservation, or agent owns the blocker.
    BeadOwnership,
    /// Local Cargo, shell-wrapped RCH, or fail-open fallback.
    CommandShape,
    /// Required log, manifest, sidecar, or redaction evidence is missing.
    ArtifactGap,
    /// Repo policy forbids the attempted proof path.
    Policy,
}

/// Severity for one blocker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofDoctorSeverity {
    /// Advisory but proof may proceed.
    Warn,
    /// Proof should not proceed or cannot be claimed.
    Block,
}

/// Owner signal attached to a proof-doctor blocker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProofDoctorOwner {
    /// The current agent owns the path or blocker.
    CurrentAgent {
        /// Agent name.
        agent_name: String,
        /// Optional Bead id.
        bead_id: Option<String>,
    },
    /// Another active agent owns the path or blocker.
    OtherAgent {
        /// Agent name.
        agent_name: String,
        /// Optional Bead id.
        bead_id: Option<String>,
    },
    /// Beads identifies an owner.
    Bead {
        /// Bead id.
        bead_id: String,
        /// Optional assignee.
        assignee: Option<String>,
    },
    /// Agent Mail reservation identifies an owner.
    Reservation {
        /// Agent name.
        agent_name: String,
        /// Reserved path pattern.
        path_pattern: String,
    },
    /// No owner could be identified.
    Unknown,
}

/// One blocker in a proof-doctor verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorBlocker {
    /// Blocker category.
    pub blocker_kind: ProofDoctorBlockerKind,
    /// Stable machine-readable reason code.
    pub reason_code: String,
    /// Blocker severity.
    pub severity: ProofDoctorSeverity,
    /// Best owner signal, when known.
    pub owner: Option<ProofDoctorOwner>,
    /// Paths affected by this blocker.
    pub affected_paths: Vec<String>,
    /// Evidence keys that justify the classifier decision.
    pub evidence_keys: Vec<String>,
    /// Operator-facing message.
    pub message: String,
    /// Next action for the operator or agent.
    pub next_action: String,
}

impl ProofDoctorBlocker {
    #[must_use]
    fn block(
        blocker_kind: ProofDoctorBlockerKind,
        reason_code: &str,
        message: &str,
        next_action: &str,
    ) -> Self {
        Self {
            blocker_kind,
            reason_code: reason_code.to_string(),
            severity: ProofDoctorSeverity::Block,
            owner: None,
            affected_paths: Vec::new(),
            evidence_keys: Vec::new(),
            message: message.to_string(),
            next_action: next_action.to_string(),
        }
    }

    #[must_use]
    fn with_owner(mut self, owner: ProofDoctorOwner) -> Self {
        self.owner = Some(owner);
        self
    }

    #[must_use]
    fn with_path(mut self, path: impl Into<String>) -> Self {
        self.affected_paths.push(path.into());
        self
    }

    #[must_use]
    fn with_evidence(mut self, key: &str) -> Self {
        self.evidence_keys.push(key.to_string());
        self
    }
}

/// RCH config value and source observed by the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorConfigSource {
    /// Config key, for example `compilation.external_timeout_enabled`.
    pub key: String,
    /// Effective value as display-safe text.
    pub value: String,
    /// Source label, for example `user`, `project`, or `env`.
    pub source: String,
    /// Whether this source produced the effective value.
    pub effective: bool,
}

/// Observed relationship between the selected RCH binary and required behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofDoctorToolVersionState {
    /// No version comparison or behavior check was available.
    Unknown,
    /// Installed RCH appears to honor the required behavior.
    InstalledCurrent,
    /// Installed RCH is stale or contradicts effective configuration.
    InstalledStale,
    /// A local patched RCH binary is being used instead of the installed one.
    PatchedLocal,
    /// Evidence includes both installed and patched RCH surfaces.
    Mixed,
}

/// Active Bead reference observed during preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorBeadRef {
    /// Bead id.
    pub bead_id: String,
    /// Bead title.
    pub title: String,
    /// Optional assignee.
    pub assignee: Option<String>,
    /// Bead status.
    pub status: String,
}

/// File reservation reference observed during preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorReservationRef {
    /// Agent holding the reservation.
    pub agent_name: String,
    /// Reserved path or glob pattern.
    pub path_pattern: String,
    /// Owning Bead when known.
    pub bead_id: Option<String>,
}

/// Dirty path metadata observed during preflight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorDirtyPath {
    /// Repository-relative path.
    pub path: String,
    /// Git status marker, for example `M`, `??`, or `AM`.
    pub status: String,
    /// Whether the caller already knows this path affects the proof lane.
    pub affects_proof: bool,
    /// Best owner signal for this path.
    pub owner: Option<ProofDoctorOwner>,
}

/// Scale-lab artifact evidence consumed by high-scale proof-doctor checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorScaleLabArtifactEvidence {
    /// Whether this proof lane requires a scale-lab artifact.
    pub required: bool,
    /// Retained scale-lab artifact path.
    pub artifact_path: Option<String>,
    /// Artifact schema version.
    pub schema_version: Option<String>,
    /// Whether the artifact is too old for the claim being made.
    pub artifact_stale: bool,
    /// Whether the artifact failed schema or semantic validation.
    pub artifact_malformed: bool,
    /// Release claim status reported by the artifact.
    pub release_claim_status: Option<String>,
    /// Required release claim status for this lane.
    pub required_release_claim_status: String,
    /// Proof manifest status reported by the artifact.
    pub manifest_status: Option<String>,
    /// Evidence mode reported by the artifact.
    pub evidence_mode: Option<String>,
    /// Whether the artifact reports a live mux substrate.
    pub live_mux_available: Option<bool>,
    /// Pane scales represented by the artifact.
    pub pane_scales: Vec<u64>,
    /// Pane scales required by this lane.
    pub required_pane_scales: Vec<u64>,
    /// Maximum requested logical cores represented by the artifact.
    pub max_requested_logical_cores: Option<u64>,
    /// Minimum logical cores required by this lane.
    pub min_required_logical_cores: u64,
    /// Maximum requested memory represented by the artifact.
    pub max_requested_memory_bytes: Option<u64>,
    /// Minimum memory required by this lane.
    pub min_required_memory_bytes: u64,
}

impl Default for ProofDoctorScaleLabArtifactEvidence {
    fn default() -> Self {
        Self {
            required: false,
            artifact_path: None,
            schema_version: None,
            artifact_stale: false,
            artifact_malformed: false,
            release_claim_status: None,
            required_release_claim_status: DEFAULT_SCALE_LAB_REQUIRED_RELEASE_CLAIM_STATUS
                .to_string(),
            manifest_status: None,
            evidence_mode: None,
            live_mux_available: None,
            pane_scales: Vec::new(),
            required_pane_scales: DEFAULT_SCALE_LAB_REQUIRED_PANE_SCALES.to_vec(),
            max_requested_logical_cores: None,
            min_required_logical_cores: DEFAULT_SCALE_LAB_MIN_LOGICAL_CORES,
            max_requested_memory_bytes: None,
            min_required_memory_bytes: DEFAULT_SCALE_LAB_MIN_MEMORY_BYTES,
        }
    }
}

/// Evidence snapshot consumed by the proof-doctor classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorEvidence {
    /// RCH binary path.
    pub rch_binary_path: Option<String>,
    /// RCH version.
    pub rch_version: Option<String>,
    /// Machine-readable installed-vs-patched RCH state.
    pub tool_version_state: ProofDoctorToolVersionState,
    /// Config source rows.
    pub rch_config_sources: Vec<ProofDoctorConfigSource>,
    /// Effective external-timeout setting.
    pub rch_external_timeout_enabled: Option<bool>,
    /// Whether stale external-timeout behavior was observed.
    pub stale_external_timeout_observed: bool,
    /// Selected worker id.
    pub selected_worker: Option<String>,
    /// Worker probe artifact path.
    pub worker_probe_artifact: Option<String>,
    /// Healthy worker count reported by RCH status.
    pub healthy_worker_count: Option<u32>,
    /// Rust-capable worker count reported by RCH status or worker capabilities.
    pub rust_worker_count: Option<u32>,
    /// Available remote execution slots reported by RCH status.
    pub available_worker_slots: Option<u32>,
    /// RCH sync duration.
    pub sync_duration_ms: Option<u64>,
    /// Remote command duration.
    pub remote_command_duration_ms: Option<u64>,
    /// Wrapper exit code.
    pub wrapper_exit_code: Option<i32>,
    /// Remote exit code.
    pub remote_exit_code: Option<i32>,
    /// True when remote Cargo started.
    pub remote_cargo_reached: bool,
    /// True when rustc or build execution started.
    pub rustc_reached: bool,
    /// True when assertions started.
    pub test_binary_started: bool,
    /// True when local Cargo or fail-open execution was detected.
    pub local_cargo_detected: bool,
    /// Artifact retrieval state.
    pub artifact_retrieval_status: ArtifactRetrievalStatus,
    /// Dirty path observations.
    pub dirty_paths: Vec<ProofDoctorDirtyPath>,
    /// Active Beads observed by preflight.
    pub active_beads: Vec<ProofDoctorBeadRef>,
    /// File reservations observed by preflight.
    pub reservations: Vec<ProofDoctorReservationRef>,
    /// Retained artifact paths.
    pub artifact_paths: Vec<String>,
    /// First-party paths named by compile/test diagnostics.
    pub diagnostic_paths: Vec<String>,
    /// Short redaction-safe compile/test diagnostic summary.
    pub diagnostic_summary: Option<String>,
    /// High-scale predicate status, when relevant.
    pub high_scale_predicate_met: Option<bool>,
    /// Scale-lab artifact evidence, when the lane needs high-scale claim evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_lab_artifact: Option<ProofDoctorScaleLabArtifactEvidence>,
}

impl Default for ProofDoctorEvidence {
    fn default() -> Self {
        Self {
            rch_binary_path: None,
            rch_version: None,
            tool_version_state: ProofDoctorToolVersionState::Unknown,
            rch_config_sources: Vec::new(),
            rch_external_timeout_enabled: None,
            stale_external_timeout_observed: false,
            selected_worker: None,
            worker_probe_artifact: None,
            healthy_worker_count: None,
            rust_worker_count: None,
            available_worker_slots: None,
            sync_duration_ms: None,
            remote_command_duration_ms: None,
            wrapper_exit_code: None,
            remote_exit_code: None,
            remote_cargo_reached: false,
            rustc_reached: false,
            test_binary_started: false,
            local_cargo_detected: false,
            artifact_retrieval_status: ArtifactRetrievalStatus::NotStarted,
            dirty_paths: Vec::new(),
            active_beads: Vec::new(),
            reservations: Vec::new(),
            artifact_paths: Vec::new(),
            diagnostic_paths: Vec::new(),
            diagnostic_summary: None,
            high_scale_predicate_met: None,
            scale_lab_artifact: None,
        }
    }
}

/// Durable projection into the existing proof-lane ledger vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofAttemptProjection {
    /// Projected proof state.
    pub state: ProofState,
    /// Primary reason code.
    pub reason_code: String,
    /// Operator-facing summary.
    pub summary: String,
    /// Whether this projection can support source-bead closeout.
    pub safe_to_close: bool,
}

/// Next action selected by proof-doctor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorNextAction {
    /// Stable action code.
    pub action_code: String,
    /// Human-readable instruction.
    pub message: String,
}

/// Input to the pure proof-doctor classifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorPreflightInput {
    /// Optional owning Bead id.
    pub bead_id: Option<String>,
    /// Optional parent Bead id.
    pub parent_bead_id: Option<String>,
    /// Agent or operator name.
    pub agent_name: String,
    /// Absolute repo path.
    pub repo_path: String,
    /// Git head used by the observation.
    pub git_head: String,
    /// Branch name.
    pub branch: String,
    /// RFC3339 timestamp from the caller.
    pub generated_at_utc: String,
    /// Intended proof command argv.
    pub intended_command: Vec<String>,
    /// Intended target dir.
    pub intended_target_dir: Option<String>,
    /// Scope being proven.
    pub intended_scope: ProofScope,
    /// Required backend.
    pub required_backend: ProofBackend,
    /// Phase being classified.
    pub phase: ProofDoctorPhase,
    /// Repo-relative paths or prefixes that define the lane scope.
    pub proof_path_prefixes: Vec<String>,
    /// Evidence snapshot.
    pub evidence: ProofDoctorEvidence,
}

impl Default for ProofDoctorPreflightInput {
    fn default() -> Self {
        Self {
            bead_id: None,
            parent_bead_id: None,
            agent_name: String::new(),
            repo_path: String::new(),
            git_head: String::new(),
            branch: String::new(),
            generated_at_utc: String::new(),
            intended_command: Vec::new(),
            intended_target_dir: None,
            intended_scope: ProofScope::CargoTest,
            required_backend: ProofBackend::Rch,
            phase: ProofDoctorPhase::Preflight,
            proof_path_prefixes: Vec::new(),
            evidence: ProofDoctorEvidence::default(),
        }
    }
}

/// Machine-readable proof-doctor verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofDoctorVerdict {
    /// Schema version.
    pub schema_version: u32,
    /// Stable verdict id.
    pub verdict_id: String,
    /// Optional owning Bead id.
    pub bead_id: Option<String>,
    /// Optional parent Bead id.
    pub parent_bead_id: Option<String>,
    /// RFC3339 timestamp from the caller.
    pub generated_at_utc: String,
    /// Agent or operator name.
    pub agent_name: String,
    /// Absolute repo path.
    pub repo_path: String,
    /// Git head used by the observation.
    pub git_head: String,
    /// Branch name.
    pub branch: String,
    /// Intended proof command argv.
    pub intended_command: Vec<String>,
    /// Intended target dir.
    pub intended_target_dir: Option<String>,
    /// Scope being proven.
    pub intended_scope: ProofScope,
    /// Required backend.
    pub required_backend: ProofBackend,
    /// Phase inspected by proof-doctor.
    pub phase: ProofDoctorPhase,
    /// Top-level status.
    pub status: ProofDoctorStatus,
    /// Blockers found by proof-doctor.
    pub blockers: Vec<ProofDoctorBlocker>,
    /// Evidence consumed by the classifier.
    pub evidence: ProofDoctorEvidence,
    /// Existing proof-lane projection, when material evidence exists.
    pub ledger_projection: Option<ProofAttemptProjection>,
    /// Operator-facing summary.
    pub operator_summary: String,
    /// Next action.
    pub next_action: ProofDoctorNextAction,
}

/// Classify one proof-doctor preflight or observed proof snapshot.
#[must_use]
pub fn classify_proof_doctor(input: &ProofDoctorPreflightInput) -> ProofDoctorVerdict {
    let mut blockers = Vec::new();

    classify_command_shape(input, &mut blockers);
    classify_rch_tooling(input, &mut blockers);
    classify_execution_evidence(input, &mut blockers);
    classify_dirty_paths(input, &mut blockers);
    classify_high_scale(input, &mut blockers);
    classify_scale_lab_artifact(input, &mut blockers);

    let status = select_status(input, &blockers);
    let primary = blockers.first();
    let reason_code = primary.map_or_else(
        || "proof.runnable".to_string(),
        |blocker| blocker.reason_code.clone(),
    );
    let operator_summary = primary.map_or_else(
        || runnable_operator_summary(input.phase),
        |blocker| blocker.message.clone(),
    );
    let next_action = primary.map_or_else(
        || ProofDoctorNextAction {
            action_code: "run_remote_proof".to_string(),
            message:
                "Run the intended proof through the required backend and attach ledger evidence."
                    .to_string(),
        },
        |blocker| ProofDoctorNextAction {
            action_code: next_action_code(status).to_string(),
            message: blocker.next_action.clone(),
        },
    );

    ProofDoctorVerdict {
        schema_version: PROOF_DOCTOR_SCHEMA_VERSION,
        verdict_id: verdict_id(input, &reason_code),
        bead_id: input.bead_id.clone(),
        parent_bead_id: input.parent_bead_id.clone(),
        generated_at_utc: input.generated_at_utc.clone(),
        agent_name: input.agent_name.clone(),
        repo_path: input.repo_path.clone(),
        git_head: input.git_head.clone(),
        branch: input.branch.clone(),
        intended_command: input.intended_command.clone(),
        intended_target_dir: input.intended_target_dir.clone(),
        intended_scope: input.intended_scope,
        required_backend: input.required_backend,
        phase: input.phase,
        status,
        blockers,
        evidence: input.evidence.clone(),
        ledger_projection: projection_for(status, &reason_code, &operator_summary),
        operator_summary,
        next_action,
    }
}

fn classify_command_shape(
    input: &ProofDoctorPreflightInput,
    blockers: &mut Vec<ProofDoctorBlocker>,
) {
    if input.required_backend != ProofBackend::Rch {
        return;
    }

    if input.evidence.local_cargo_detected || is_local_cargo_command(&input.intended_command) {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::CommandShape,
                "proof.command.local_cargo_invalid",
                "Local Cargo was offered for an RCH-required proof lane.",
                "Rerun through direct RCH remote Cargo or record this only as local smoke.",
            )
            .with_evidence("intended_command"),
        );
        return;
    }

    if is_shell_wrapped_cargo(&input.intended_command) {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::CommandShape,
                "proof.command.shell_wrapped_rch_unclassified",
                "Shell-wrapped RCH Cargo cannot be claimed as remote proof without positive remote-Cargo evidence.",
                "Use direct `rch exec -- env CARGO_TARGET_DIR=... cargo ...` or retain metadata proving remote Cargo started.",
            )
            .with_evidence("intended_command"),
        );
    }
}

fn classify_rch_tooling(input: &ProofDoctorPreflightInput, blockers: &mut Vec<ProofDoctorBlocker>) {
    let evidence = &input.evidence;
    if input.required_backend != ProofBackend::Rch {
        return;
    }

    if evidence.rch_external_timeout_enabled == Some(false)
        && evidence.stale_external_timeout_observed
    {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::RchTooling,
                "proof.rch.stale_external_timeout_config",
                "Effective RCH config disables the external timeout wrapper, but stale timeout behavior was observed.",
                "Update or select an RCH binary that honors the effective config before rerunning proof.",
            )
            .with_evidence("rch_config_sources")
            .with_evidence("rch_binary_path"),
        );
    }

    if evidence.rust_worker_count == Some(0) {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::WorkerCapacity,
                "proof.rch.no_rust_workers",
                "RCH status found no Rust-capable workers for a required remote Cargo lane.",
                "Restore worker capabilities or choose a Rust-capable RCH worker before rerunning proof.",
            )
            .with_evidence("rust_worker_count")
            .with_evidence("healthy_worker_count")
            .with_evidence("available_worker_slots"),
        );
    }
}

fn classify_execution_evidence(
    input: &ProofDoctorPreflightInput,
    blockers: &mut Vec<ProofDoctorBlocker>,
) {
    let evidence = &input.evidence;

    if evidence.remote_cargo_reached
        && evidence.test_binary_started
        && evidence.remote_exit_code.is_some_and(|code| code != 0)
    {
        let message = evidence
            .diagnostic_summary
            .as_deref()
            .unwrap_or("The remote proof command reached assertion execution and failed.");
        let blocker = diagnostic_blocker(
            ProofDoctorBlockerKind::TestAssertion,
            "proof.test.remote_assertion_failed",
            message,
            "Fix the behavior or test harness before claiming this lane.",
            evidence,
        );
        blockers.push(
            blocker
                .with_evidence("remote_exit_code")
                .with_evidence("test_binary_started"),
        );
        return;
    }

    if evidence.remote_cargo_reached
        && evidence.rustc_reached
        && evidence.remote_exit_code == Some(101)
    {
        let message = evidence
            .diagnostic_summary
            .as_deref()
            .unwrap_or("Remote rustc reached first-party code and reported a compile error.");
        let blocker = diagnostic_blocker(
            ProofDoctorBlockerKind::SourceCompile,
            "proof.source.remote_compile_error",
            message,
            "Handoff to the owner of the first-party source failure; do not claim this proof lane green.",
            evidence,
        );
        blockers.push(
            blocker
                .with_evidence("remote_exit_code")
                .with_evidence("rustc_reached"),
        );
        return;
    }

    if !evidence.remote_cargo_reached
        && evidence.wrapper_exit_code == Some(127)
        && (evidence.selected_worker.is_some() || evidence.sync_duration_ms.is_some())
    {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::RemoteLaunch,
                "proof.rch.pre_cargo_timeout_exec_missing",
                "RCH selected a worker or synced, then the remote launch failed before Cargo started.",
                "Block on RCH tooling or worker launch; do not claim source pass or fail.",
            )
            .with_evidence("selected_worker")
            .with_evidence("sync_duration_ms")
            .with_evidence("wrapper_exit_code"),
        );
        return;
    }

    if !evidence.remote_cargo_reached
        && (evidence.selected_worker.is_some() || evidence.sync_duration_ms.is_some())
        && input.phase != ProofDoctorPhase::Preflight
    {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::ArtifactGap,
                "proof.rch.sync_not_proof",
                "RCH worker or sync evidence exists, but no retained log proves remote Cargo started.",
                "Rerun with fail-closed RCH logging or mark the attempt inconclusive.",
            )
            .with_evidence("selected_worker")
            .with_evidence("sync_duration_ms"),
        );
    }
}

fn diagnostic_blocker(
    blocker_kind: ProofDoctorBlockerKind,
    reason_code: &str,
    message: &str,
    next_action: &str,
    evidence: &ProofDoctorEvidence,
) -> ProofDoctorBlocker {
    let mut blocker = ProofDoctorBlocker::block(blocker_kind, reason_code, message, next_action);
    for path in &evidence.diagnostic_paths {
        if blocker.owner.is_none() {
            if let Some(owner) = diagnostic_owner_for_path(evidence, path) {
                blocker = blocker.with_owner(owner).with_evidence("diagnostic_owner");
            }
        }
        blocker = blocker.with_path(path.clone());
    }
    if !evidence.diagnostic_paths.is_empty() {
        blocker = blocker.with_evidence("diagnostic_paths");
    }
    if evidence.diagnostic_summary.is_some() {
        blocker = blocker.with_evidence("diagnostic_summary");
    }
    blocker
}

fn diagnostic_owner_for_path(
    evidence: &ProofDoctorEvidence,
    path: &str,
) -> Option<ProofDoctorOwner> {
    evidence
        .dirty_paths
        .iter()
        .find(|dirty_path| dirty_path.path == path)
        .and_then(|dirty_path| dirty_path.owner.clone())
        .or_else(|| {
            evidence
                .reservations
                .iter()
                .find(|reservation| path_overlaps_pattern(path, &reservation.path_pattern))
                .map(|reservation| ProofDoctorOwner::Reservation {
                    agent_name: reservation.agent_name.clone(),
                    path_pattern: reservation.path_pattern.clone(),
                })
        })
}

fn path_overlaps_pattern(path: &str, pattern: &str) -> bool {
    path == pattern
        || pattern == "*"
        || pattern == "**/*"
        || pattern
            .strip_suffix("/*")
            .is_some_and(|prefix| path.starts_with(&format!("{prefix}/")))
        || pattern
            .strip_suffix("/**")
            .is_some_and(|prefix| path.starts_with(&format!("{prefix}/")))
}

fn classify_dirty_paths(input: &ProofDoctorPreflightInput, blockers: &mut Vec<ProofDoctorBlocker>) {
    for dirty_path in &input.evidence.dirty_paths {
        if !dirty_path.affects_proof
            && !path_overlaps_prefixes(&dirty_path.path, &input.proof_path_prefixes)
        {
            continue;
        }

        let owner = dirty_path
            .owner
            .clone()
            .unwrap_or(ProofDoctorOwner::Unknown);
        let reason_code = if matches!(owner, ProofDoctorOwner::Unknown) {
            "proof.dirty.unowned_path_overlap"
        } else {
            "proof.dirty.active_owned_path_overlap"
        };
        let message = if matches!(owner, ProofDoctorOwner::Unknown) {
            "A dirty path overlaps the proof lane, but no owner could be identified."
        } else {
            "A dirty path overlaps the proof lane and is owned by active work."
        };

        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::DirtyTree,
                reason_code,
                message,
                "Resolve ownership or wait for the owning Bead before claiming proof.",
            )
            .with_owner(owner)
            .with_path(dirty_path.path.clone())
            .with_evidence("dirty_paths"),
        );
    }
}

fn classify_high_scale(input: &ProofDoctorPreflightInput, blockers: &mut Vec<ProofDoctorBlocker>) {
    if input.intended_scope == ProofScope::HighScale
        && input.evidence.high_scale_predicate_met == Some(false)
    {
        blockers.push(
            ProofDoctorBlocker::block(
                ProofDoctorBlockerKind::WorkerCapacity,
                "proof.high_scale.predicate_absent",
                "The required high-scale worker predicate is absent.",
                "Record the lane as skipped-not-proven or rerun on matching hardware.",
            )
            .with_evidence("high_scale_predicate_met"),
        );
    }
}

fn classify_scale_lab_artifact(
    input: &ProofDoctorPreflightInput,
    blockers: &mut Vec<ProofDoctorBlocker>,
) {
    if !requires_scale_lab_artifact(input) {
        return;
    }

    let Some(artifact) = input.evidence.scale_lab_artifact.as_ref() else {
        blockers.push(
            scale_lab_blocker(
                ProofDoctorBlockerKind::ArtifactGap,
                "proof.scale_lab.artifact_missing",
                "A scale-lab proof lane requires a retained scale-lab artifact.",
                "Attach the scale-lab staged proof artifact before graduating the claim.",
                None,
            )
            .with_evidence("intended_command"),
        );
        return;
    };

    if artifact.artifact_path.as_deref().is_none_or(str::is_empty) {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.artifact_missing",
            "A scale-lab proof lane requires a retained scale-lab artifact.",
            "Attach the scale-lab staged proof artifact before graduating the claim.",
            Some(artifact),
        ));
        return;
    }

    let structural_blocker_count = blockers.len();

    if artifact.artifact_stale {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.artifact_stale",
            "The retained scale-lab artifact is stale for this release claim.",
            "Rerun the scale-lab lane and attach a fresh artifact.",
            Some(artifact),
        ));
    }

    if artifact.artifact_malformed {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.artifact_malformed",
            "The retained scale-lab artifact failed schema or semantic validation.",
            "Regenerate the scale-lab artifact and keep the malformed file out of release evidence.",
            Some(artifact),
        ));
    }

    if !artifact_contains_required_pane_scales(artifact) {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.pane_scales_incomplete",
            "The scale-lab artifact is missing one or more required pane-scale lanes.",
            "Rerun the missing 50/200/500+ scale-lab stages before graduating the claim.",
            Some(artifact),
        ));
    }

    if blockers.len() > structural_blocker_count {
        return;
    }

    if artifact.release_claim_status.as_deref()
        != Some(artifact.required_release_claim_status.as_str())
    {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.release_claim_not_proven",
            "Scale-lab artifact does not meet the required release claim truth tier.",
            "Record this lane as skipped-not-proven or rerun on live matching hardware.",
            Some(artifact),
        ));
        return;
    }

    if artifact.manifest_status.as_deref() != Some(DEFAULT_SCALE_LAB_REQUIRED_MANIFEST_STATUS) {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.manifest_not_proven",
            "Scale-lab artifact proof manifest is not proven.",
            "Record this lane as skipped-not-proven or attach a proven proof-manifest artifact.",
            Some(artifact),
        ));
        return;
    }

    if artifact.evidence_mode.as_deref() != Some(DEFAULT_SCALE_LAB_REQUIRED_EVIDENCE_MODE) {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.evidence_mode_not_real_hardware",
            "Scale-lab artifact evidence mode is not a real hardware run.",
            "Record this lane as skipped-not-proven or rerun on live matching hardware.",
            Some(artifact),
        ));
        return;
    }

    if artifact.live_mux_available != Some(true) {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.live_mux_absent",
            "Scale-lab artifact did not come from a live mux substrate.",
            "Rerun with live panes before using this artifact for release claim graduation.",
            Some(artifact),
        ));
    }

    let cores_missing = artifact
        .max_requested_logical_cores
        .is_none_or(|cores| cores < artifact.min_required_logical_cores);
    let memory_missing = artifact
        .max_requested_memory_bytes
        .is_none_or(|memory| memory < artifact.min_required_memory_bytes);
    if cores_missing || memory_missing {
        blockers.push(scale_lab_blocker(
            ProofDoctorBlockerKind::ArtifactGap,
            "proof.scale_lab.hardware_shape_mismatch",
            "Scale-lab artifact does not represent the required high-scale host shape.",
            "Rerun on hardware matching the requested core and memory predicate.",
            Some(artifact),
        ));
    }
}

fn requires_scale_lab_artifact(input: &ProofDoctorPreflightInput) -> bool {
    input
        .evidence
        .scale_lab_artifact
        .as_ref()
        .is_some_and(|artifact| artifact.required)
        || input.intended_scope == ProofScope::HighScale
        || command_mentions_scale_lab(&input.intended_command)
}

fn command_mentions_scale_lab(command: &[String]) -> bool {
    command.iter().any(|token| {
        let token = token.to_ascii_lowercase();
        token.contains("scale_lab") || token.contains("scale-lab")
    })
}

fn artifact_contains_required_pane_scales(artifact: &ProofDoctorScaleLabArtifactEvidence) -> bool {
    artifact
        .required_pane_scales
        .iter()
        .all(|required| artifact.pane_scales.contains(required))
}

fn scale_lab_blocker(
    blocker_kind: ProofDoctorBlockerKind,
    reason_code: &str,
    message: &str,
    next_action: &str,
    artifact: Option<&ProofDoctorScaleLabArtifactEvidence>,
) -> ProofDoctorBlocker {
    let mut blocker = ProofDoctorBlocker::block(blocker_kind, reason_code, message, next_action)
        .with_evidence("scale_lab_artifact");
    if let Some(path) = artifact.and_then(|artifact| artifact.artifact_path.as_deref()) {
        if !path.is_empty() {
            blocker = blocker.with_path(path.to_string());
        }
    }
    blocker
}

fn select_status(
    input: &ProofDoctorPreflightInput,
    blockers: &[ProofDoctorBlocker],
) -> ProofDoctorStatus {
    if blockers.iter().any(|blocker| {
        blocker.blocker_kind == ProofDoctorBlockerKind::CommandShape
            || blocker.blocker_kind == ProofDoctorBlockerKind::Policy
    }) {
        return ProofDoctorStatus::Invalid;
    }

    if blockers
        .iter()
        .any(|blocker| blocker.blocker_kind == ProofDoctorBlockerKind::SourceCompile)
    {
        return ProofDoctorStatus::SourceBlocked;
    }

    if blockers
        .iter()
        .any(|blocker| blocker.blocker_kind == ProofDoctorBlockerKind::TestAssertion)
    {
        return ProofDoctorStatus::TestBlocked;
    }

    if blockers
        .iter()
        .any(|blocker| blocker.blocker_kind == ProofDoctorBlockerKind::DirtyTree)
    {
        return ProofDoctorStatus::DirtyTreeBlocked;
    }

    if blockers
        .iter()
        .any(|blocker| blocker.blocker_kind == ProofDoctorBlockerKind::BeadOwnership)
    {
        return ProofDoctorStatus::OwnershipBlocked;
    }

    if blockers
        .iter()
        .any(|blocker| skipped_not_proven_reason(&blocker.reason_code))
    {
        return ProofDoctorStatus::SkippedNotProven;
    }

    if blockers.iter().any(|blocker| {
        matches!(
            blocker.blocker_kind,
            ProofDoctorBlockerKind::RchTooling
                | ProofDoctorBlockerKind::WorkerCapacity
                | ProofDoctorBlockerKind::RemoteSync
                | ProofDoctorBlockerKind::RemoteLaunch
                | ProofDoctorBlockerKind::RemoteSubstrate
        )
    }) {
        return ProofDoctorStatus::InfraBlocked;
    }

    if blockers
        .iter()
        .any(|blocker| blocker.blocker_kind == ProofDoctorBlockerKind::ArtifactGap)
    {
        return ProofDoctorStatus::Inconclusive;
    }

    if input.evidence.remote_cargo_reached
        && input.evidence.remote_exit_code == Some(0)
        && input.evidence.artifact_retrieval_status == ArtifactRetrievalStatus::Complete
    {
        return ProofDoctorStatus::Passed;
    }

    ProofDoctorStatus::Runnable
}

fn skipped_not_proven_reason(reason_code: &str) -> bool {
    matches!(
        reason_code,
        "proof.high_scale.predicate_absent"
            | "proof.scale_lab.release_claim_not_proven"
            | "proof.scale_lab.manifest_not_proven"
            | "proof.scale_lab.evidence_mode_not_real_hardware"
            | "proof.scale_lab.live_mux_absent"
            | "proof.scale_lab.hardware_shape_mismatch"
    )
}

fn projection_for(
    status: ProofDoctorStatus,
    reason_code: &str,
    summary: &str,
) -> Option<ProofAttemptProjection> {
    let state = match status {
        ProofDoctorStatus::Runnable => ProofState::NotRun,
        ProofDoctorStatus::Passed => ProofState::Pass,
        ProofDoctorStatus::SourceBlocked => ProofState::SourceCompileFail,
        ProofDoctorStatus::TestBlocked => ProofState::TestFail,
        ProofDoctorStatus::InfraBlocked => ProofState::InfraBlockedPreCargo,
        ProofDoctorStatus::DirtyTreeBlocked
        | ProofDoctorStatus::OwnershipBlocked
        | ProofDoctorStatus::Inconclusive => ProofState::Inconclusive,
        ProofDoctorStatus::Invalid => ProofState::LocalInvalid,
        ProofDoctorStatus::SkippedNotProven => ProofState::SkippedNotProven,
    };

    Some(ProofAttemptProjection {
        state,
        reason_code: reason_code.to_string(),
        summary: summary.to_string(),
        safe_to_close: status == ProofDoctorStatus::Passed,
    })
}

fn next_action_code(status: ProofDoctorStatus) -> &'static str {
    match status {
        ProofDoctorStatus::Runnable => "run_remote_proof",
        ProofDoctorStatus::Passed => "attach_pass_evidence",
        ProofDoctorStatus::SourceBlocked => "handoff_source_owner",
        ProofDoctorStatus::TestBlocked => "fix_failing_assertion",
        ProofDoctorStatus::InfraBlocked => "fix_infrastructure",
        ProofDoctorStatus::DirtyTreeBlocked => "resolve_dirty_tree",
        ProofDoctorStatus::OwnershipBlocked => "handoff_owner",
        ProofDoctorStatus::Invalid => "fix_command_shape",
        ProofDoctorStatus::SkippedNotProven => "supply_predicate_or_skip",
        ProofDoctorStatus::Inconclusive => "rerun_with_artifacts",
    }
}

fn runnable_operator_summary(phase: ProofDoctorPhase) -> String {
    match phase {
        ProofDoctorPhase::Preflight => {
            "Advisory preflight verdict: proof lane is runnable; no known blocker was found before the proof command starts.".to_string()
        }
        _ => "Proof lane is runnable; no known blocker was found in the retained evidence.".to_string(),
    }
}

fn verdict_id(input: &ProofDoctorPreflightInput, reason_code: &str) -> String {
    let bead = input.bead_id.as_deref().unwrap_or("no-bead");
    format!("proof-doctor:{bead}:{reason_code}")
}

fn is_local_cargo_command(command: &[String]) -> bool {
    command
        .first()
        .is_some_and(|first| command_token_is(first, "cargo"))
}

fn is_shell_wrapped_cargo(command: &[String]) -> bool {
    command.windows(3).any(|window| {
        (command_token_is(&window[0], "bash")
            || command_token_is(&window[0], "sh")
            || command_token_is(&window[0], "zsh"))
            && window[1] == "-lc"
            && window[2].contains("cargo")
    }) || command
        .windows(2)
        .any(|window| window[0] == "-lc" && window[1].contains("cargo"))
}

fn command_token_is(token: &str, expected: &str) -> bool {
    token == expected || token.ends_with(&format!("/{expected}"))
}

fn path_overlaps_prefixes(path: &str, prefixes: &[String]) -> bool {
    prefixes
        .iter()
        .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    struct TestRobotEnvelope<'a> {
        ok: bool,
        data: TestRobotData<'a>,
        elapsed_ms: u64,
        version: &'static str,
        now: u64,
    }

    #[derive(serde::Serialize)]
    struct TestRobotData<'a> {
        schema_version: u32,
        verdict: &'a ProofDoctorVerdict,
    }

    fn owner_source_value(owner: Option<&ProofDoctorOwner>) -> serde_json::Value {
        match owner {
            Some(ProofDoctorOwner::CurrentAgent {
                agent_name,
                bead_id,
            }) => serde_json::json!({
                "type": "current_agent",
                "agent_name": agent_name,
                "bead_id": bead_id,
            }),
            Some(ProofDoctorOwner::OtherAgent {
                agent_name,
                bead_id,
            }) => serde_json::json!({
                "type": "other_agent",
                "agent_name": agent_name,
                "bead_id": bead_id,
            }),
            Some(ProofDoctorOwner::Bead { bead_id, assignee }) => serde_json::json!({
                "type": "bead",
                "bead_id": bead_id,
                "assignee": assignee,
            }),
            Some(ProofDoctorOwner::Reservation {
                agent_name,
                path_pattern,
            }) => serde_json::json!({
                "type": "reservation",
                "agent_name": agent_name,
                "path_pattern": path_pattern,
            }),
            Some(ProofDoctorOwner::Unknown) => serde_json::json!({
                "type": "unknown",
            }),
            None => serde_json::Value::Null,
        }
    }

    fn dirty_paths_value(input: &ProofDoctorPreflightInput) -> serde_json::Value {
        serde_json::Value::Array(
            input
                .evidence
                .dirty_paths
                .iter()
                .map(|dirty_path| {
                    serde_json::json!({
                        "path": dirty_path.path,
                        "status": dirty_path.status,
                        "owner": owner_source_value(dirty_path.owner.as_ref()),
                    })
                })
                .collect(),
        )
    }

    fn e2e_fixture_log(
        scenario: &str,
        input: &ProofDoctorPreflightInput,
        verdict: &ProofDoctorVerdict,
    ) -> serde_json::Value {
        let primary = verdict.blockers.first();
        serde_json::json!({
            "scenario": scenario,
            "command": input.intended_command,
            "worker_id": input.evidence.selected_worker,
            "sync_completed": input.evidence.sync_duration_ms.is_some(),
            "cargo_started": input.evidence.remote_cargo_reached,
            "first_error": input.evidence.diagnostic_summary,
            "dirty_files": dirty_paths_value(input),
            "ownership_source": owner_source_value(primary.and_then(|blocker| blocker.owner.as_ref())),
            "status": verdict.status,
            "reason_code": primary.map(|blocker| blocker.reason_code.as_str()),
        })
    }

    fn base_input() -> ProofDoctorPreflightInput {
        ProofDoctorPreflightInput {
            bead_id: Some("ft-wik9p.3".to_string()),
            parent_bead_id: Some("ft-wik9p".to_string()),
            agent_name: "OliveChapel".to_string(),
            repo_path: "/Users/jemanuel/projects/frankenterm".to_string(),
            git_head: "HEAD".to_string(),
            branch: "main".to_string(),
            generated_at_utc: "2026-05-05T12:00:00Z".to_string(),
            intended_command: vec![
                "rch".to_string(),
                "exec".to_string(),
                "--".to_string(),
                "env".to_string(),
                "CARGO_TARGET_DIR=/tmp/ft-wik9p3-target".to_string(),
                "cargo".to_string(),
                "test".to_string(),
                "-p".to_string(),
                "frankenterm-core-audit-types".to_string(),
            ],
            intended_target_dir: Some("/tmp/ft-wik9p3-target".to_string()),
            intended_scope: ProofScope::CargoTest,
            required_backend: ProofBackend::Rch,
            phase: ProofDoctorPhase::Preflight,
            proof_path_prefixes: vec!["crates/frankenterm-core-audit-types".to_string()],
            evidence: ProofDoctorEvidence::default(),
        }
    }

    fn scale_lab_artifact() -> ProofDoctorScaleLabArtifactEvidence {
        ProofDoctorScaleLabArtifactEvidence {
            required: true,
            artifact_path: Some(
                "/tmp/ft-5kt3d-target/scale-lab-smoke/run/scale-lab-staged-proof.v1.json"
                    .to_string(),
            ),
            schema_version: Some("ft.scale_lab.staged_proof.v1".to_string()),
            release_claim_status: Some(DEFAULT_SCALE_LAB_REQUIRED_RELEASE_CLAIM_STATUS.to_string()),
            manifest_status: Some("proven".to_string()),
            evidence_mode: Some("real_hardware_run".to_string()),
            live_mux_available: Some(true),
            pane_scales: DEFAULT_SCALE_LAB_REQUIRED_PANE_SCALES.to_vec(),
            max_requested_logical_cores: Some(DEFAULT_SCALE_LAB_MIN_LOGICAL_CORES),
            max_requested_memory_bytes: Some(DEFAULT_SCALE_LAB_MIN_MEMORY_BYTES),
            ..ProofDoctorScaleLabArtifactEvidence::default()
        }
    }

    fn robot_envelope_value(verdict: &ProofDoctorVerdict) -> serde_json::Value {
        serde_json::to_value(TestRobotEnvelope {
            ok: true,
            data: TestRobotData {
                schema_version: PROOF_DOCTOR_SCHEMA_VERSION,
                verdict,
            },
            elapsed_ms: 12,
            version: "0.1.0-test",
            now: 1_777_960_000,
        })
        .expect("serialize proof-doctor test robot envelope")
    }

    fn status_snapshot(verdict: &ProofDoctorVerdict) -> serde_json::Value {
        let envelope = robot_envelope_value(verdict);
        let first_blocker = envelope["data"]["verdict"]["blockers"]
            .as_array()
            .and_then(|blockers| blockers.first());
        let reason_code = first_blocker.map_or(serde_json::Value::Null, |blocker| {
            blocker["reason_code"].clone()
        });
        let blocker_kind = first_blocker.map_or(serde_json::Value::Null, |blocker| {
            blocker["blocker_kind"].clone()
        });
        let affected_path = first_blocker.map_or(serde_json::Value::Null, |blocker| {
            blocker["affected_paths"][0].clone()
        });

        serde_json::json!({
            "ok": envelope["ok"],
            "schema_version": envelope["data"]["schema_version"],
            "verdict_schema_version": envelope["data"]["verdict"]["schema_version"],
            "verdict_id": envelope["data"]["verdict"]["verdict_id"],
            "status": envelope["data"]["verdict"]["status"],
            "phase": envelope["data"]["verdict"]["phase"],
            "reason_code": reason_code,
            "blocker_kind": blocker_kind,
            "affected_path": affected_path,
            "ledger_state": envelope["data"]["verdict"]["ledger_projection"]["state"],
            "safe_to_close": envelope["data"]["verdict"]["ledger_projection"]["safe_to_close"],
            "tool_version_state": envelope["data"]["verdict"]["evidence"]["tool_version_state"],
        })
    }

    fn core_status_verdicts() -> Vec<ProofDoctorVerdict> {
        let runnable = classify_proof_doctor(&base_input());

        let mut infra = base_input();
        infra.phase = ProofDoctorPhase::TerminalClassified;
        infra.evidence.tool_version_state = ProofDoctorToolVersionState::InstalledStale;
        infra.evidence.selected_worker = Some("vmi1152480".to_string());
        infra.evidence.sync_duration_ms = Some(176_008);
        infra.evidence.wrapper_exit_code = Some(127);
        let infra = classify_proof_doctor(&infra);

        let mut source = base_input();
        source.phase = ProofDoctorPhase::TerminalClassified;
        source.evidence.tool_version_state = ProofDoctorToolVersionState::PatchedLocal;
        source.evidence.remote_cargo_reached = true;
        source.evidence.rustc_reached = true;
        source.evidence.remote_exit_code = Some(101);
        source.evidence.diagnostic_paths =
            vec!["crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs".to_string()];
        source.evidence.diagnostic_summary =
            Some("Remote rustc reported a missing field initializer.".to_string());
        let source = classify_proof_doctor(&source);

        let mut dirty = base_input();
        dirty.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/proof_doctor.rs".to_string(),
            status: "M".to_string(),
            affects_proof: false,
            owner: Some(ProofDoctorOwner::Bead {
                bead_id: "ft-wik9p.6".to_string(),
                assignee: Some("OliveChapel".to_string()),
            }),
        });
        let dirty = classify_proof_doctor(&dirty);

        let mut inconclusive = base_input();
        inconclusive.phase = ProofDoctorPhase::LaunchObserved;
        inconclusive.evidence.selected_worker = Some("vmi1156319".to_string());
        inconclusive.evidence.sync_duration_ms = Some(225_197);
        let inconclusive = classify_proof_doctor(&inconclusive);

        vec![runnable, infra, source, dirty, inconclusive]
    }

    #[test]
    fn e2e_fixture_scenarios_capture_required_proof_doctor_logs() {
        let mut stale_timeout = base_input();
        stale_timeout.bead_id = Some("ft-wik9p.7.installed-stale".to_string());
        stale_timeout.phase = ProofDoctorPhase::TerminalClassified;
        stale_timeout.evidence.rch_binary_path = Some("/Users/jemanuel/.local/bin/rch".to_string());
        stale_timeout.evidence.rch_version = Some("1.0.24+32a0ea5".to_string());
        stale_timeout.evidence.tool_version_state = ProofDoctorToolVersionState::InstalledStale;
        stale_timeout.evidence.rch_external_timeout_enabled = Some(false);
        stale_timeout.evidence.stale_external_timeout_observed = true;
        stale_timeout
            .evidence
            .rch_config_sources
            .push(ProofDoctorConfigSource {
                key: "compilation.external_timeout_enabled".to_string(),
                value: "false".to_string(),
                source: "user".to_string(),
                effective: true,
            });
        stale_timeout.evidence.selected_worker = Some("vmi1152480".to_string());
        stale_timeout.evidence.sync_duration_ms = Some(176_008);
        stale_timeout.evidence.wrapper_exit_code = Some(127);
        let stale_timeout_verdict = classify_proof_doctor(&stale_timeout);

        let mut source_fail = base_input();
        source_fail.bead_id = Some("ft-wik9p.7.patched-source".to_string());
        source_fail.phase = ProofDoctorPhase::TerminalClassified;
        source_fail.evidence.tool_version_state = ProofDoctorToolVersionState::PatchedLocal;
        source_fail.evidence.selected_worker = Some("vmi1153651".to_string());
        source_fail.evidence.sync_duration_ms = Some(164_000);
        source_fail.evidence.remote_command_duration_ms = Some(88_000);
        source_fail.evidence.remote_cargo_reached = true;
        source_fail.evidence.rustc_reached = true;
        source_fail.evidence.remote_exit_code = Some(101);
        source_fail.evidence.diagnostic_paths =
            vec!["crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs".to_string()];
        source_fail.evidence.diagnostic_summary =
            Some("remote rustc reported missing field `external_service_observation`".to_string());
        source_fail.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs".to_string(),
            status: "M".to_string(),
            affects_proof: false,
            owner: Some(ProofDoctorOwner::Bead {
                bead_id: "ft-1grhq.5".to_string(),
                assignee: Some("CoralBeaver".to_string()),
            }),
        });
        let source_fail_verdict = classify_proof_doctor(&source_fail);

        let mut dirty_active = base_input();
        dirty_active.bead_id = Some("ft-wik9p.7.dirty-active".to_string());
        dirty_active.proof_path_prefixes =
            vec!["crates/frankenterm-core-audit-types/src/proof_handoff.rs".to_string()];
        dirty_active
            .evidence
            .dirty_paths
            .push(ProofDoctorDirtyPath {
                path: "crates/frankenterm-core-audit-types/src/proof_handoff.rs".to_string(),
                status: "M".to_string(),
                affects_proof: true,
                owner: Some(ProofDoctorOwner::Reservation {
                    agent_name: "MagentaFalcon".to_string(),
                    path_pattern: "crates/frankenterm-core-audit-types/src/proof_handoff.rs"
                        .to_string(),
                }),
            });
        let dirty_active_verdict = classify_proof_doctor(&dirty_active);

        let mut clean_pass = base_input();
        clean_pass.bead_id = Some("ft-wik9p.7.clean-pass".to_string());
        clean_pass.phase = ProofDoctorPhase::TerminalClassified;
        clean_pass.evidence.tool_version_state = ProofDoctorToolVersionState::PatchedLocal;
        clean_pass.evidence.selected_worker = Some("vmi1152480".to_string());
        clean_pass.evidence.sync_duration_ms = Some(140_000);
        clean_pass.evidence.remote_cargo_reached = true;
        clean_pass.evidence.rustc_reached = true;
        clean_pass.evidence.test_binary_started = true;
        clean_pass.evidence.remote_exit_code = Some(0);
        clean_pass.evidence.artifact_retrieval_status = ArtifactRetrievalStatus::Complete;
        clean_pass.evidence.artifact_paths =
            vec!["tests/e2e/artifacts/proof/ft-wik9p.7/pass.json".to_string()];
        let clean_plan_verdict = classify_proof_doctor(&base_input());
        let clean_pass_verdict = classify_proof_doctor(&clean_pass);

        assert_eq!(
            stale_timeout_verdict.status,
            ProofDoctorStatus::InfraBlocked
        );
        assert_eq!(
            stale_timeout_verdict.blockers[0].reason_code,
            "proof.rch.stale_external_timeout_config"
        );
        assert!(stale_timeout_verdict.blockers.iter().any(|blocker| {
            blocker.reason_code == "proof.rch.pre_cargo_timeout_exec_missing"
                && blocker
                    .evidence_keys
                    .iter()
                    .any(|key| key == "wrapper_exit_code")
        }));

        assert_eq!(source_fail_verdict.status, ProofDoctorStatus::SourceBlocked);
        assert_eq!(
            source_fail_verdict.blockers[0].affected_paths,
            vec!["crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs"]
        );
        assert!(matches!(
            source_fail_verdict.blockers[0].owner.as_ref(),
            Some(ProofDoctorOwner::Bead {
                bead_id,
                assignee: Some(assignee),
            }) if bead_id == "ft-1grhq.5" && assignee == "CoralBeaver"
        ));

        assert_eq!(
            dirty_active_verdict.status,
            ProofDoctorStatus::DirtyTreeBlocked
        );
        assert_ne!(
            dirty_active_verdict.status,
            ProofDoctorStatus::SourceBlocked
        );
        assert_eq!(clean_plan_verdict.status, ProofDoctorStatus::Runnable);
        assert_eq!(clean_pass_verdict.status, ProofDoctorStatus::Passed);
        assert_eq!(
            clean_pass_verdict
                .ledger_projection
                .as_ref()
                .map(|projection| (projection.state, projection.safe_to_close)),
            Some((ProofState::Pass, true))
        );

        let logs = serde_json::Value::Array(vec![
            e2e_fixture_log(
                "installed_stale_timeout",
                &stale_timeout,
                &stale_timeout_verdict,
            ),
            e2e_fixture_log("patched_source_failure", &source_fail, &source_fail_verdict),
            e2e_fixture_log("dirty_active_owner", &dirty_active, &dirty_active_verdict),
            e2e_fixture_log("clean_pass", &clean_pass, &clean_pass_verdict),
        ]);

        assert_eq!(logs[0]["worker_id"].as_str(), Some("vmi1152480"));
        assert_eq!(logs[0]["sync_completed"].as_bool(), Some(true));
        assert_eq!(logs[0]["cargo_started"].as_bool(), Some(false));
        assert_eq!(
            logs[1]["first_error"].as_str(),
            source_fail.evidence.diagnostic_summary.as_deref()
        );
        assert_eq!(
            logs[1]["ownership_source"]["bead_id"].as_str(),
            Some("ft-1grhq.5")
        );
        assert_eq!(
            logs[2]["dirty_files"][0]["owner"]["agent_name"].as_str(),
            Some("MagentaFalcon")
        );
        assert_eq!(logs[3]["cargo_started"].as_bool(), Some(true));
    }

    #[test]
    fn clean_direct_rch_lane_is_runnable() {
        let verdict = classify_proof_doctor(&base_input());

        assert_eq!(verdict.status, ProofDoctorStatus::Runnable);
        assert!(verdict.blockers.is_empty());
        assert_eq!(verdict.generated_at_utc, "2026-05-05T12:00:00Z");
        assert!(
            verdict
                .operator_summary
                .contains("Advisory preflight verdict")
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::NotRun)
        );
    }

    #[test]
    fn scale_lab_command_without_artifact_is_inconclusive() {
        let mut input = base_input();
        input.intended_scope = ProofScope::HighScale;
        input
            .intended_command
            .push("scale_lab_staged_lanes_mark_replay_unproven_without_live_hardware".to_string());

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::Inconclusive);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.scale_lab.artifact_missing"
        );
        assert_eq!(
            verdict.blockers[0].blocker_kind,
            ProofDoctorBlockerKind::ArtifactGap
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::Inconclusive)
        );
    }

    #[test]
    fn scale_lab_local_smoke_cannot_graduate_high_scale_claim() {
        let mut input = base_input();
        input.intended_scope = ProofScope::HighScale;
        input.evidence.high_scale_predicate_met = Some(true);
        let mut artifact = scale_lab_artifact();
        artifact.release_claim_status = Some("local-smoke".to_string());
        artifact.manifest_status = Some("skipped_not_proven".to_string());
        artifact.evidence_mode = Some("synthetic_smoke".to_string());
        artifact.live_mux_available = Some(false);
        input.evidence.scale_lab_artifact = Some(artifact);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::SkippedNotProven);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.scale_lab.release_claim_not_proven"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::SkippedNotProven)
        );
        assert!(
            !verdict
                .ledger_projection
                .as_ref()
                .is_some_and(|projection| projection.safe_to_close),
            "skipped scale-lab evidence must not be safe to close as proven"
        );
    }

    #[test]
    fn scale_lab_malformed_artifact_is_inconclusive() {
        let mut input = base_input();
        input.intended_scope = ProofScope::HighScale;
        input.evidence.high_scale_predicate_met = Some(true);
        let mut artifact = scale_lab_artifact();
        artifact.artifact_malformed = true;
        input.evidence.scale_lab_artifact = Some(artifact);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::Inconclusive);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.scale_lab.artifact_malformed"
        );
    }

    #[test]
    fn scale_lab_manifest_and_mode_mismatch_cannot_graduate_high_scale_claim() {
        let mut input = base_input();
        input.intended_scope = ProofScope::HighScale;
        input.evidence.high_scale_predicate_met = Some(true);
        let mut artifact = scale_lab_artifact();
        artifact.manifest_status = Some("skipped_not_proven".to_string());
        artifact.evidence_mode = Some("synthetic_smoke".to_string());
        input.evidence.scale_lab_artifact = Some(artifact);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::SkippedNotProven);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.scale_lab.manifest_not_proven"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::SkippedNotProven)
        );

        let mut input = base_input();
        input.intended_scope = ProofScope::HighScale;
        input.evidence.high_scale_predicate_met = Some(true);
        let mut artifact = scale_lab_artifact();
        artifact.evidence_mode = Some("synthetic_smoke".to_string());
        input.evidence.scale_lab_artifact = Some(artifact);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::SkippedNotProven);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.scale_lab.evidence_mode_not_real_hardware"
        );
    }

    #[test]
    fn scale_lab_real_hardware_artifact_keeps_high_scale_lane_runnable() {
        let mut input = base_input();
        input.intended_scope = ProofScope::HighScale;
        input.evidence.high_scale_predicate_met = Some(true);
        input.evidence.scale_lab_artifact = Some(scale_lab_artifact());

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::Runnable);
        assert!(verdict.blockers.is_empty());
        let json = serde_json::to_value(&verdict).expect("serialize verdict");
        assert_eq!(
            json["evidence"]["scale_lab_artifact"]["release_claim_status"].as_str(),
            Some(DEFAULT_SCALE_LAB_REQUIRED_RELEASE_CLAIM_STATUS)
        );
    }

    #[test]
    fn pre_cargo_timeout_wrapper_is_infra_blocked_not_source_blocked() {
        let mut input = base_input();
        input.phase = ProofDoctorPhase::TerminalClassified;
        input.evidence.selected_worker = Some("vmi1149989".to_string());
        input.evidence.sync_duration_ms = Some(180_611);
        input.evidence.wrapper_exit_code = Some(127);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::InfraBlocked);
        assert_ne!(verdict.status, ProofDoctorStatus::SourceBlocked);
        assert_eq!(
            verdict.blockers[0].blocker_kind,
            ProofDoctorBlockerKind::RemoteLaunch
        );
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.rch.pre_cargo_timeout_exec_missing"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::InfraBlockedPreCargo)
        );
    }

    #[test]
    fn stale_external_timeout_config_blocks_preflight() {
        let mut input = base_input();
        input.evidence.rch_binary_path = Some("/Users/jemanuel/.local/bin/rch".to_string());
        input.evidence.rch_external_timeout_enabled = Some(false);
        input.evidence.stale_external_timeout_observed = true;
        input.evidence.tool_version_state = ProofDoctorToolVersionState::InstalledStale;
        input
            .evidence
            .rch_config_sources
            .push(ProofDoctorConfigSource {
                key: "compilation.external_timeout_enabled".to_string(),
                value: "false".to_string(),
                source: "user".to_string(),
                effective: true,
            });

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::InfraBlocked);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.rch.stale_external_timeout_config"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::InfraBlockedPreCargo)
        );
    }

    #[test]
    fn patched_local_rch_tool_state_is_visible_without_blocking() {
        let mut input = base_input();
        input.evidence.rch_binary_path = Some("/tmp/rch-config-patch-target/debug/rch".to_string());
        input.evidence.rch_version = Some("1.0.24+32a0ea5".to_string());
        input.evidence.rch_external_timeout_enabled = Some(false);
        input.evidence.tool_version_state = ProofDoctorToolVersionState::PatchedLocal;

        let verdict = classify_proof_doctor(&input);
        let json = serde_json::to_value(&verdict).expect("serialize verdict");

        assert_eq!(verdict.status, ProofDoctorStatus::Runnable);
        assert_eq!(
            json["evidence"]["tool_version_state"].as_str(),
            Some("patched_local")
        );
        assert_eq!(
            json["evidence"]["rch_binary_path"].as_str(),
            Some("/tmp/rch-config-patch-target/debug/rch")
        );
    }

    #[test]
    fn no_rust_workers_blocks_remote_cargo_preflight() {
        let mut input = base_input();
        input.evidence.healthy_worker_count = Some(8);
        input.evidence.rust_worker_count = Some(0);
        input.evidence.available_worker_slots = Some(54);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::InfraBlocked);
        assert_eq!(verdict.blockers[0].reason_code, "proof.rch.no_rust_workers");
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::InfraBlockedPreCargo)
        );
    }

    #[test]
    fn local_cargo_is_invalid_for_rch_required_lane() {
        let mut input = base_input();
        input.intended_command = vec!["cargo".to_string(), "test".to_string()];

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::Invalid);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.command.local_cargo_invalid"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::LocalInvalid)
        );
    }

    #[test]
    fn bare_shell_token_does_not_invalidate_direct_rch_lane() {
        let mut input = base_input();
        input.intended_command.push("bash".to_string());

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::Runnable);
        assert!(verdict.blockers.is_empty());
    }

    #[test]
    fn dirty_active_owned_path_overlap_blocks_with_owner() {
        let mut input = base_input();
        input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/lib.rs".to_string(),
            status: "M".to_string(),
            affects_proof: false,
            owner: Some(ProofDoctorOwner::Bead {
                bead_id: "ft-wik9p.3".to_string(),
                assignee: Some("OliveChapel".to_string()),
            }),
        });

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::DirtyTreeBlocked);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.dirty.active_owned_path_overlap"
        );
        assert_eq!(
            verdict.blockers[0].affected_paths,
            vec!["crates/frankenterm-core-audit-types/src/lib.rs"]
        );
    }

    #[test]
    fn dirty_untracked_reservation_owner_blocks_with_owner() {
        let mut input = base_input();
        input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/proof_doctor.rs".to_string(),
            status: "??".to_string(),
            affects_proof: true,
            owner: Some(ProofDoctorOwner::Reservation {
                agent_name: "CoralBeaver".to_string(),
                path_pattern: "crates/frankenterm-core-audit-types/src/proof_doctor.rs".to_string(),
            }),
        });

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::DirtyTreeBlocked);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.dirty.active_owned_path_overlap"
        );
        assert!(matches!(
            verdict.blockers[0].owner.as_ref(),
            Some(ProofDoctorOwner::Reservation { .. })
        ));
    }

    #[test]
    fn dirty_untracked_unowned_path_overlap_blocks_without_owner() {
        let mut input = base_input();
        input.evidence.dirty_paths.push(ProofDoctorDirtyPath {
            path: "crates/frankenterm-core-audit-types/src/proof_doctor.rs".to_string(),
            status: "??".to_string(),
            affects_proof: false,
            owner: None,
        });

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::DirtyTreeBlocked);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.dirty.unowned_path_overlap"
        );
        assert!(matches!(
            verdict.blockers[0].owner.as_ref(),
            Some(ProofDoctorOwner::Unknown)
        ));
    }

    #[test]
    fn sync_without_remote_cargo_is_inconclusive_not_green() {
        let mut input = base_input();
        input.phase = ProofDoctorPhase::LaunchObserved;
        input.evidence.selected_worker = Some("vmi1152480".to_string());
        input.evidence.sync_duration_ms = Some(176_008);

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::Inconclusive);
        assert_eq!(verdict.blockers[0].reason_code, "proof.rch.sync_not_proof");
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::Inconclusive)
        );
    }

    #[test]
    fn remote_compile_failure_maps_to_source_blocked() {
        let mut input = base_input();
        input.phase = ProofDoctorPhase::TerminalClassified;
        input.evidence.remote_cargo_reached = true;
        input.evidence.rustc_reached = true;
        input.evidence.remote_exit_code = Some(101);
        input.evidence.diagnostic_paths =
            vec!["crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs".to_string()];
        input.evidence.diagnostic_summary =
            Some("Remote rustc reported missing field `external_service_observation`.".to_string());

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::SourceBlocked);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.source.remote_compile_error"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::SourceCompileFail)
        );
        assert_eq!(
            verdict.blockers[0].affected_paths,
            vec!["crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs"]
        );
        assert!(
            verdict.blockers[0]
                .evidence_keys
                .iter()
                .any(|key| key == "diagnostic_summary")
        );
    }

    #[test]
    fn remote_test_assertion_failure_maps_to_test_blocked() {
        let mut input = base_input();
        input.phase = ProofDoctorPhase::TerminalClassified;
        input.evidence.remote_cargo_reached = true;
        input.evidence.rustc_reached = true;
        input.evidence.test_binary_started = true;
        input.evidence.remote_exit_code = Some(101);
        input.evidence.diagnostic_paths =
            vec!["crates/frankenterm-core-audit-types/src/proof_doctor.rs".to_string()];
        input.evidence.diagnostic_summary =
            Some("Remote test assertion failed for stale installed RCH wording.".to_string());

        let verdict = classify_proof_doctor(&input);

        assert_eq!(verdict.status, ProofDoctorStatus::TestBlocked);
        assert_eq!(
            verdict.blockers[0].reason_code,
            "proof.test.remote_assertion_failed"
        );
        assert_eq!(
            verdict
                .ledger_projection
                .as_ref()
                .map(|projection| projection.state),
            Some(ProofState::TestFail)
        );
        assert_eq!(
            verdict.blockers[0].affected_paths,
            vec!["crates/frankenterm-core-audit-types/src/proof_doctor.rs"]
        );
    }

    #[test]
    fn robot_json_golden_snapshots_cover_core_statuses() {
        let snapshots = core_status_verdicts()
            .iter()
            .map(status_snapshot)
            .collect::<Vec<_>>();

        assert_eq!(
            serde_json::Value::Array(snapshots),
            serde_json::json!([
                {
                    "ok": true,
                    "schema_version": 1,
                    "verdict_schema_version": 1,
                    "verdict_id": "proof-doctor:ft-wik9p.3:proof.runnable",
                    "status": "runnable",
                    "phase": "preflight",
                    "reason_code": null,
                    "blocker_kind": null,
                    "affected_path": null,
                    "ledger_state": "NOT_RUN",
                    "safe_to_close": false,
                    "tool_version_state": "unknown",
                },
                {
                    "ok": true,
                    "schema_version": 1,
                    "verdict_schema_version": 1,
                    "verdict_id": "proof-doctor:ft-wik9p.3:proof.rch.pre_cargo_timeout_exec_missing",
                    "status": "infra_blocked",
                    "phase": "terminal_classified",
                    "reason_code": "proof.rch.pre_cargo_timeout_exec_missing",
                    "blocker_kind": "remote_launch",
                    "affected_path": null,
                    "ledger_state": "INFRA_BLOCKED_PRE_CARGO",
                    "safe_to_close": false,
                    "tool_version_state": "installed_stale",
                },
                {
                    "ok": true,
                    "schema_version": 1,
                    "verdict_schema_version": 1,
                    "verdict_id": "proof-doctor:ft-wik9p.3:proof.source.remote_compile_error",
                    "status": "source_blocked",
                    "phase": "terminal_classified",
                    "reason_code": "proof.source.remote_compile_error",
                    "blocker_kind": "source_compile",
                    "affected_path": "crates/frankenterm-core/src/resource_pressure_clock_timer_chaos.rs",
                    "ledger_state": "SOURCE_COMPILE_FAIL",
                    "safe_to_close": false,
                    "tool_version_state": "patched_local",
                },
                {
                    "ok": true,
                    "schema_version": 1,
                    "verdict_schema_version": 1,
                    "verdict_id": "proof-doctor:ft-wik9p.3:proof.dirty.active_owned_path_overlap",
                    "status": "dirty_tree_blocked",
                    "phase": "preflight",
                    "reason_code": "proof.dirty.active_owned_path_overlap",
                    "blocker_kind": "dirty_tree",
                    "affected_path": "crates/frankenterm-core-audit-types/src/proof_doctor.rs",
                    "ledger_state": "INCONCLUSIVE",
                    "safe_to_close": false,
                    "tool_version_state": "unknown",
                },
                {
                    "ok": true,
                    "schema_version": 1,
                    "verdict_schema_version": 1,
                    "verdict_id": "proof-doctor:ft-wik9p.3:proof.rch.sync_not_proof",
                    "status": "inconclusive",
                    "phase": "launch_observed",
                    "reason_code": "proof.rch.sync_not_proof",
                    "blocker_kind": "artifact_gap",
                    "affected_path": null,
                    "ledger_state": "INCONCLUSIVE",
                    "safe_to_close": false,
                    "tool_version_state": "unknown",
                },
            ])
        );
    }

    #[test]
    fn robot_toon_roundtrip_preserves_core_statuses() {
        for verdict in core_status_verdicts() {
            let envelope = robot_envelope_value(&verdict);
            let toon = toon_rust::encode(envelope.clone(), None);
            let decoded = toon_rust::try_decode(&toon, None).expect("decode proof-doctor toon");
            let json_str =
                toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
            let roundtripped: serde_json::Value =
                serde_json::from_str(&json_str).expect("parse roundtripped toon json");

            assert_eq!(roundtripped["ok"], envelope["ok"]);
            assert_eq!(
                roundtripped["data"]["verdict"]["status"],
                envelope["data"]["verdict"]["status"]
            );
            assert_eq!(
                roundtripped["data"]["verdict"]["phase"],
                envelope["data"]["verdict"]["phase"]
            );
            assert_eq!(
                roundtripped["data"]["verdict"]["evidence"]["tool_version_state"],
                envelope["data"]["verdict"]["evidence"]["tool_version_state"]
            );
        }
    }

    #[test]
    fn human_summaries_stay_concise_and_do_not_overclaim_non_passes() {
        for verdict in core_status_verdicts() {
            assert!(
                verdict.operator_summary.len() <= 120,
                "summary too long for {:?}: {}",
                verdict.status,
                verdict.operator_summary
            );

            if verdict.status != ProofDoctorStatus::Passed {
                let summary = verdict.operator_summary.to_ascii_lowercase();
                assert!(!summary.contains("green"));
                assert!(!summary.contains("passed"));
                assert!(!summary.contains("safe to close"));
            }
        }
    }
}
