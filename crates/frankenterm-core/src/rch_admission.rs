//! Read-only RCH admission collector normalization.
//!
//! This module does not execute subprocesses, mutate Beads, touch Agent Mail,
//! restart services, inspect worker mirrors, run Cargo, or delete files. It
//! consumes already-collected facts from future CLI/doctor surfaces and emits
//! the static `ft.rch_admission.v1` contract.

#![allow(clippy::module_name_repetitions)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const RCH_ADMISSION_CONTRACT_ID: &str = "ft.rch_admission.v1";
pub const RCH_ADMISSION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionReasonCode {
    LocalEnoSpace,
    NoAdmissibleWorkers,
    CriticalPressure,
    TelemetryGap,
    InsufficientSlots,
    ActiveProjectExclusion,
    SpeedscoreResponseShape,
    DryRunInconsistentWorker,
    Unknown,
}

impl RchAdmissionReasonCode {
    #[must_use]
    pub fn recommendation(self) -> &'static str {
        match self {
            Self::LocalEnoSpace => {
                "Stop proof work and request operator-approved cleanup evidence."
            }
            Self::NoAdmissibleWorkers => {
                "Block the proof bead on RCH admission and retain dry-run output."
            }
            Self::CriticalPressure => {
                "Wait for worker recovery or operator-approved worker-side cleanup."
            }
            Self::TelemetryGap => "Refresh read-only RCH status evidence without mutating workers.",
            Self::InsufficientSlots => {
                "Use explicit Cargo jobs when appropriate, queue, or wait for capacity."
            }
            Self::ActiveProjectExclusion => {
                "Coordinate with the active same-project build owner before retrying."
            }
            Self::SpeedscoreResponseShape => {
                "Treat SpeedScore ranking as unavailable and rely on stable status evidence."
            }
            Self::DryRunInconsistentWorker => {
                "Preserve the inconsistent dry-run envelope and treat it as advisory only."
            }
            Self::Unknown => "Retain the artifact and file or update a Beads diagnosis.",
        }
    }

    #[must_use]
    pub fn operator_approval_required(self) -> bool {
        matches!(self, Self::LocalEnoSpace | Self::CriticalPressure)
    }

    #[must_use]
    pub fn blocks_proof(self) -> bool {
        matches!(
            self,
            Self::LocalEnoSpace
                | Self::NoAdmissibleWorkers
                | Self::CriticalPressure
                | Self::InsufficientSlots
                | Self::ActiveProjectExclusion
                | Self::DryRunInconsistentWorker
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionProofStatus {
    AdvisoryOnly,
    Runnable,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionProbeStatus {
    Ok,
    Failed,
    NotChecked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionSeverity {
    Info,
    Warning,
    Blocked,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionCitationKind {
    CommandOutput,
    Artifact,
    Bead,
    AgentMail,
    ManualNote,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionForbiddenAction {
    RunLocalCargoAsProof,
    RestartAgentMail,
    RepairAgentMailDb,
    RestartRchDaemon,
    MutateRchWorker,
    CancelOtherAgentBuild,
    DeleteFilesWithoutApproval,
    TreatDryRunAsCompileProof,
}

impl RchAdmissionForbiddenAction {
    #[must_use]
    pub fn stable_all() -> Vec<Self> {
        vec![
            Self::RunLocalCargoAsProof,
            Self::RestartAgentMail,
            Self::RepairAgentMailDb,
            Self::RestartRchDaemon,
            Self::MutateRchWorker,
            Self::CancelOtherAgentBuild,
            Self::DeleteFilesWithoutApproval,
            Self::TreatDryRunAsCompileProof,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionCommandDiagnostic {
    pub raw: String,
    pub normalized: Option<String>,
    pub classification: Option<String>,
    pub would_intercept: Option<bool>,
    pub target_dir: Option<String>,
}

impl RchAdmissionCommandDiagnostic {
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self {
            raw: nonempty_string(raw, "unknown"),
            normalized: None,
            classification: None,
            would_intercept: None,
            target_dir: None,
        }
    }

    #[must_use]
    pub fn normalized(mut self, normalized: impl Into<String>) -> Self {
        self.normalized = Some(nonempty_string(normalized, "unknown"));
        self
    }

    #[must_use]
    pub fn classification(mut self, classification: impl Into<String>) -> Self {
        self.classification = Some(nonempty_string(classification, "unknown"));
        self
    }

    #[must_use]
    pub fn would_intercept(mut self, would_intercept: bool) -> Self {
        self.would_intercept = Some(would_intercept);
        self
    }

    #[must_use]
    pub fn target_dir(mut self, target_dir: impl Into<String>) -> Self {
        self.target_dir = Some(nonempty_string(target_dir, "unknown"));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionProbeDiagnostic {
    pub status: RchAdmissionProbeStatus,
    pub error_code: Option<String>,
    pub message: String,
}

impl RchAdmissionProbeDiagnostic {
    #[must_use]
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            status: RchAdmissionProbeStatus::Ok,
            error_code: None,
            message: nonempty_string(message, "probe succeeded"),
        }
    }

    #[must_use]
    pub fn failed(error_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: RchAdmissionProbeStatus::Failed,
            error_code: Some(nonempty_string(error_code, "unknown")),
            message: nonempty_string(message, "probe failed"),
        }
    }

    #[must_use]
    pub fn not_checked(message: impl Into<String>) -> Self {
        Self {
            status: RchAdmissionProbeStatus::NotChecked,
            error_code: None,
            message: nonempty_string(message, "not checked"),
        }
    }

    fn failed_for_enospc(&self) -> bool {
        self.status == RchAdmissionProbeStatus::Failed
            && self
                .error_code
                .as_deref()
                .into_iter()
                .chain(std::iter::once(self.message.as_str()))
                .any(looks_like_enospc)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionLocalDiskDiagnostic {
    pub system_data_free_bytes: Option<u64>,
    pub private_tmp_free_bytes: Option<u64>,
    pub repo_write_probe: RchAdmissionProbeDiagnostic,
    pub rch_cache_write_probe: RchAdmissionProbeDiagnostic,
}

impl RchAdmissionLocalDiskDiagnostic {
    #[must_use]
    pub fn not_checked() -> Self {
        Self {
            system_data_free_bytes: None,
            private_tmp_free_bytes: None,
            repo_write_probe: RchAdmissionProbeDiagnostic::not_checked("repo write not checked"),
            rch_cache_write_probe: RchAdmissionProbeDiagnostic::not_checked(
                "RCH cache write not checked",
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionBeadsDiagnostic {
    pub db_writeable: Option<bool>,
    pub jsonl_writeable: Option<bool>,
    pub active_bead: Option<String>,
    pub blocking_beads: Vec<String>,
}

impl RchAdmissionBeadsDiagnostic {
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            db_writeable: None,
            jsonl_writeable: None,
            active_bead: None,
            blocking_beads: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionAgentMailDiagnostic {
    pub db_open: Option<bool>,
    pub api_reachable: Option<bool>,
    pub reservation_conflicts: Vec<String>,
}

impl RchAdmissionAgentMailDiagnostic {
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            db_open: None,
            api_reachable: None,
            reservation_conflicts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionQueueDiagnostic {
    pub posture: Option<String>,
    pub active_project_exclusion: bool,
    pub active_builds: u32,
    pub queued_builds: u32,
    pub workers_healthy: Option<u32>,
    pub workers_total: Option<u32>,
}

impl RchAdmissionQueueDiagnostic {
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            posture: None,
            active_project_exclusion: false,
            active_builds: 0,
            queued_builds: 0,
            workers_healthy: None,
            workers_total: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionWorkerRejection {
    pub worker: Option<String>,
    pub reason_code: RchAdmissionReasonCode,
    pub detail: String,
    pub severity: RchAdmissionSeverity,
}

impl RchAdmissionWorkerRejection {
    #[must_use]
    pub fn new(
        worker: Option<impl Into<String>>,
        reason_code: RchAdmissionReasonCode,
        detail: impl Into<String>,
        severity: RchAdmissionSeverity,
    ) -> Self {
        Self {
            worker: worker.map(|worker| nonempty_string(worker, "worker.unknown")),
            reason_code,
            detail: nonempty_string(detail, "worker rejected"),
            severity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RchAdmissionCollectorObservation {
    pub source_id: String,
    pub source_command: String,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub error_category: Option<String>,
    pub citation_kind: RchAdmissionCitationKind,
    pub citation_path: Option<String>,
    pub summary: String,
    pub reason_code: Option<RchAdmissionReasonCode>,
}

impl RchAdmissionCollectorObservation {
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        source_command: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            source_id: nonempty_string(source_id, "collector.unknown"),
            source_command: nonempty_string(source_command, "collector unavailable"),
            collected_at_ms: None,
            freshness_ms: None,
            error_category: None,
            citation_kind: RchAdmissionCitationKind::CommandOutput,
            citation_path: None,
            summary: nonempty_string(summary, "collector evidence unavailable"),
            reason_code: None,
        }
    }

    #[must_use]
    pub fn live(mut self, collected_at_ms: u64, freshness_ms: u64) -> Self {
        self.collected_at_ms = Some(collected_at_ms);
        self.freshness_ms = Some(freshness_ms);
        self
    }

    #[must_use]
    pub fn error_category(mut self, error_category: impl Into<String>) -> Self {
        self.error_category = Some(nonempty_string(error_category, "unknown"));
        self
    }

    #[must_use]
    pub fn citation_kind(mut self, citation_kind: RchAdmissionCitationKind) -> Self {
        self.citation_kind = citation_kind;
        self
    }

    #[must_use]
    pub fn citation_path(mut self, citation_path: impl Into<String>) -> Self {
        self.citation_path = Some(nonempty_string(citation_path, "unknown"));
        self
    }

    #[must_use]
    pub fn reason_code(mut self, reason_code: RchAdmissionReasonCode) -> Self {
        self.reason_code = Some(reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RchAdmissionGitDirtyPath {
    pub path: String,
    pub status: String,
    pub category: String,
}

impl RchAdmissionGitDirtyPath {
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        status: impl Into<String>,
        category: impl Into<String>,
    ) -> Self {
        Self {
            path: nonempty_string(path, "unknown"),
            status: nonempty_string(status, "dirty"),
            category: nonempty_string(category, "dirty_tree"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RchAdmissionCollectorInput {
    pub generated_at_ms: u64,
    pub source: String,
    pub command: RchAdmissionCommandDiagnostic,
    pub local_disk: RchAdmissionLocalDiskDiagnostic,
    pub beads: RchAdmissionBeadsDiagnostic,
    pub agent_mail: RchAdmissionAgentMailDiagnostic,
    pub rch_queue: RchAdmissionQueueDiagnostic,
    pub worker_rejections: Vec<RchAdmissionWorkerRejection>,
    pub cargo_jobs: Option<u32>,
    pub estimated_slots: Option<u32>,
    pub ready_beads: Vec<String>,
    pub in_progress_beads: Vec<String>,
    pub git_dirty_paths: Vec<RchAdmissionGitDirtyPath>,
    pub collector_observations: Vec<RchAdmissionCollectorObservation>,
}

impl RchAdmissionCollectorInput {
    #[must_use]
    pub fn new(
        generated_at_ms: u64,
        source: impl Into<String>,
        command: RchAdmissionCommandDiagnostic,
    ) -> Self {
        Self {
            generated_at_ms,
            source: nonempty_string(source, "unknown"),
            command,
            local_disk: RchAdmissionLocalDiskDiagnostic::not_checked(),
            beads: RchAdmissionBeadsDiagnostic::unknown(),
            agent_mail: RchAdmissionAgentMailDiagnostic::unknown(),
            rch_queue: RchAdmissionQueueDiagnostic::unknown(),
            worker_rejections: Vec::new(),
            cargo_jobs: None,
            estimated_slots: None,
            ready_beads: Vec::new(),
            in_progress_beads: Vec::new(),
            git_dirty_paths: Vec::new(),
            collector_observations: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_local_disk(mut self, local_disk: RchAdmissionLocalDiskDiagnostic) -> Self {
        self.local_disk = local_disk;
        self
    }

    #[must_use]
    pub fn with_beads(mut self, beads: RchAdmissionBeadsDiagnostic) -> Self {
        self.beads = beads;
        self
    }

    #[must_use]
    pub fn with_agent_mail(mut self, agent_mail: RchAdmissionAgentMailDiagnostic) -> Self {
        self.agent_mail = agent_mail;
        self
    }

    #[must_use]
    pub fn with_rch_queue(mut self, rch_queue: RchAdmissionQueueDiagnostic) -> Self {
        self.rch_queue = rch_queue;
        self
    }

    #[must_use]
    pub fn with_worker_rejection(mut self, rejection: RchAdmissionWorkerRejection) -> Self {
        self.worker_rejections.push(rejection);
        self
    }

    #[must_use]
    pub fn with_cargo_jobs(mut self, cargo_jobs: u32) -> Self {
        self.cargo_jobs = Some(cargo_jobs.max(1));
        self
    }

    #[must_use]
    pub fn with_estimated_slots(mut self, estimated_slots: u32) -> Self {
        self.estimated_slots = Some(estimated_slots.max(1));
        self
    }

    #[must_use]
    pub fn with_ready_bead(mut self, bead_id: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.ready_beads, bead_id);
        self
    }

    #[must_use]
    pub fn with_in_progress_bead(mut self, bead_id: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.in_progress_beads, bead_id);
        self
    }

    #[must_use]
    pub fn with_git_dirty_path(mut self, dirty_path: RchAdmissionGitDirtyPath) -> Self {
        if !self.git_dirty_paths.iter().any(|path| path == &dirty_path) {
            self.git_dirty_paths.push(dirty_path);
        }
        self
    }

    #[must_use]
    pub fn with_collector_observation(
        mut self,
        observation: RchAdmissionCollectorObservation,
    ) -> Self {
        self.collector_observations.push(observation);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionRecommendation {
    pub action: String,
    pub reason_code: RchAdmissionReasonCode,
    pub operator_approval_required: bool,
}

impl RchAdmissionRecommendation {
    #[must_use]
    pub fn from_reason(reason_code: RchAdmissionReasonCode) -> Self {
        Self {
            action: reason_code.recommendation().to_string(),
            reason_code,
            operator_approval_required: reason_code.operator_approval_required(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionCitation {
    pub kind: RchAdmissionCitationKind,
    pub path: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub advisory_only: bool,
    pub proof_status: RchAdmissionProofStatus,
    pub command: RchAdmissionCommandDiagnostic,
    pub local_disk: RchAdmissionLocalDiskDiagnostic,
    pub beads: RchAdmissionBeadsDiagnostic,
    pub agent_mail: RchAdmissionAgentMailDiagnostic,
    pub rch_queue: RchAdmissionQueueDiagnostic,
    pub worker_rejections: Vec<RchAdmissionWorkerRejection>,
    pub cargo_jobs: Option<u32>,
    pub estimated_slots: Option<u32>,
    pub reason_codes: Vec<RchAdmissionReasonCode>,
    pub recommendations: Vec<RchAdmissionRecommendation>,
    pub forbidden_actions: Vec<RchAdmissionForbiddenAction>,
    pub citations: Vec<RchAdmissionCitation>,
}

#[must_use]
pub fn build_rch_admission_report(input: &RchAdmissionCollectorInput) -> RchAdmissionReport {
    let mut reason_codes = BTreeSet::new();
    let mut citations = Vec::new();

    collect_local_disk_reasons(input, &mut reason_codes);
    collect_beads_reasons(input, &mut reason_codes);
    collect_agent_mail_reasons(input, &mut reason_codes);
    collect_rch_queue_reasons(input, &mut reason_codes);

    for rejection in &input.worker_rejections {
        reason_codes.insert(rejection.reason_code);
    }

    for observation in &input.collector_observations {
        if let Some(reason_code) = observation.reason_code {
            reason_codes.insert(reason_code);
        }
        if let Some(reason_code) = observation
            .error_category
            .as_deref()
            .and_then(reason_code_from_error_category)
        {
            reason_codes.insert(reason_code);
        }
        citations.push(citation_from_collector_observation(observation));
    }

    for bead_id in &input.ready_beads {
        citations.push(RchAdmissionCitation {
            kind: RchAdmissionCitationKind::Bead,
            path: Some(bead_id.clone()),
            summary: format!("ready bead {bead_id} observed by read-only Beads collector"),
        });
    }
    for bead_id in &input.in_progress_beads {
        citations.push(RchAdmissionCitation {
            kind: RchAdmissionCitationKind::Bead,
            path: Some(bead_id.clone()),
            summary: format!("in-progress bead {bead_id} observed by read-only Beads collector"),
        });
    }
    for dirty_path in &input.git_dirty_paths {
        citations.push(RchAdmissionCitation {
            kind: RchAdmissionCitationKind::CommandOutput,
            path: Some(dirty_path.path.clone()),
            summary: format!(
                "git status reported {} {} ({})",
                dirty_path.status, dirty_path.path, dirty_path.category
            ),
        });
    }

    if reason_codes.is_empty() {
        reason_codes.insert(RchAdmissionReasonCode::Unknown);
    }

    let reason_codes = reason_codes.into_iter().collect::<Vec<_>>();
    let proof_status = proof_status_for(&reason_codes, input);
    let recommendations = reason_codes
        .iter()
        .copied()
        .map(RchAdmissionRecommendation::from_reason)
        .collect();

    RchAdmissionReport {
        schema_version: RCH_ADMISSION_SCHEMA_VERSION,
        contract_id: RCH_ADMISSION_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        source: input.source.clone(),
        advisory_only: true,
        proof_status,
        command: input.command.clone(),
        local_disk: input.local_disk.clone(),
        beads: input.beads.clone(),
        agent_mail: input.agent_mail.clone(),
        rch_queue: input.rch_queue.clone(),
        worker_rejections: input.worker_rejections.clone(),
        cargo_jobs: input.cargo_jobs,
        estimated_slots: input.estimated_slots,
        reason_codes,
        recommendations,
        forbidden_actions: RchAdmissionForbiddenAction::stable_all(),
        citations,
    }
}

fn collect_local_disk_reasons(
    input: &RchAdmissionCollectorInput,
    reason_codes: &mut BTreeSet<RchAdmissionReasonCode>,
) {
    if input.local_disk.repo_write_probe.failed_for_enospc()
        || input.local_disk.rch_cache_write_probe.failed_for_enospc()
    {
        reason_codes.insert(RchAdmissionReasonCode::LocalEnoSpace);
    }
}

fn collect_beads_reasons(
    input: &RchAdmissionCollectorInput,
    reason_codes: &mut BTreeSet<RchAdmissionReasonCode>,
) {
    if matches!(input.beads.db_writeable, Some(false))
        || matches!(input.beads.jsonl_writeable, Some(false))
    {
        reason_codes.insert(RchAdmissionReasonCode::LocalEnoSpace);
    }
}

fn collect_agent_mail_reasons(
    input: &RchAdmissionCollectorInput,
    reason_codes: &mut BTreeSet<RchAdmissionReasonCode>,
) {
    if matches!(input.agent_mail.db_open, Some(false))
        || matches!(input.agent_mail.api_reachable, Some(false))
    {
        reason_codes.insert(RchAdmissionReasonCode::TelemetryGap);
    }
}

fn collect_rch_queue_reasons(
    input: &RchAdmissionCollectorInput,
    reason_codes: &mut BTreeSet<RchAdmissionReasonCode>,
) {
    if input.rch_queue.active_project_exclusion {
        reason_codes.insert(RchAdmissionReasonCode::ActiveProjectExclusion);
    }
    if input.rch_queue.workers_healthy.is_none() || input.rch_queue.workers_total.is_none() {
        reason_codes.insert(RchAdmissionReasonCode::TelemetryGap);
    }
    if input.command.would_intercept == Some(true)
        && input.rch_queue.workers_total.unwrap_or(0) > 0
        && input.rch_queue.workers_healthy == Some(0)
    {
        reason_codes.insert(RchAdmissionReasonCode::NoAdmissibleWorkers);
    }
    if let (Some(estimated_slots), Some(workers_healthy)) =
        (input.estimated_slots, input.rch_queue.workers_healthy)
    {
        if workers_healthy > 0 && estimated_slots > workers_healthy {
            reason_codes.insert(RchAdmissionReasonCode::InsufficientSlots);
        }
    }
}

fn proof_status_for(
    reason_codes: &[RchAdmissionReasonCode],
    input: &RchAdmissionCollectorInput,
) -> RchAdmissionProofStatus {
    if reason_codes
        .iter()
        .any(|reason_code| reason_code.blocks_proof())
    {
        return RchAdmissionProofStatus::Blocked;
    }
    if reason_codes.iter().any(|reason_code| {
        matches!(
            reason_code,
            RchAdmissionReasonCode::TelemetryGap
                | RchAdmissionReasonCode::SpeedscoreResponseShape
                | RchAdmissionReasonCode::Unknown
        )
    }) {
        return RchAdmissionProofStatus::Unknown;
    }
    if input.command.would_intercept == Some(true) {
        return RchAdmissionProofStatus::Runnable;
    }
    RchAdmissionProofStatus::AdvisoryOnly
}

fn citation_from_collector_observation(
    observation: &RchAdmissionCollectorObservation,
) -> RchAdmissionCitation {
    let mut summary = format!(
        "{} via {}: {}",
        observation.source_id, observation.source_command, observation.summary
    );
    if let Some(freshness_ms) = observation.freshness_ms {
        summary.push_str(&format!("; freshness_ms={freshness_ms}"));
    }
    if let Some(error_category) = &observation.error_category {
        summary.push_str(&format!("; error_category={error_category}"));
    }
    RchAdmissionCitation {
        kind: observation.citation_kind,
        path: observation.citation_path.clone(),
        summary,
    }
}

fn reason_code_from_error_category(error_category: &str) -> Option<RchAdmissionReasonCode> {
    let normalized = error_category.to_ascii_lowercase();
    if looks_like_enospc(&normalized) || normalized.contains("cache.write_failed") {
        return Some(RchAdmissionReasonCode::LocalEnoSpace);
    }
    if normalized.contains("no_admissible") || normalized.contains("worker=null") {
        return Some(RchAdmissionReasonCode::NoAdmissibleWorkers);
    }
    if normalized.contains("critical_pressure") || normalized.contains("pressure-critical") {
        return Some(RchAdmissionReasonCode::CriticalPressure);
    }
    if normalized.contains("telemetry_gap") || normalized.contains("stale_telemetry") {
        return Some(RchAdmissionReasonCode::TelemetryGap);
    }
    if normalized.contains("insufficient_slots") {
        return Some(RchAdmissionReasonCode::InsufficientSlots);
    }
    if normalized.contains("active_project_exclusion") {
        return Some(RchAdmissionReasonCode::ActiveProjectExclusion);
    }
    if normalized.contains("speedscore") || normalized.contains("response_shape") {
        return Some(RchAdmissionReasonCode::SpeedscoreResponseShape);
    }
    if normalized.contains("dry_run") && normalized.contains("worker") {
        return Some(RchAdmissionReasonCode::DryRunInconsistentWorker);
    }
    None
}

fn looks_like_enospc(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    normalized.contains("enospc")
        || normalized.contains("eno_space")
        || normalized.contains("no space left")
        || normalized.contains("out of disk")
}

fn nonempty_string(value: impl Into<String>, fallback: &str) -> String {
    let value = value.into();
    if value.trim().is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn push_nonempty_unique(values: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !value.trim().is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intercepted_command() -> RchAdmissionCommandDiagnostic {
        RchAdmissionCommandDiagnostic::new("rch exec -- cargo test -p frankenterm-core")
            .normalized("cargo test -p frankenterm-core")
            .classification("cargo_test")
            .would_intercept(true)
            .target_dir("/tmp/ft-proof")
    }

    #[test]
    fn local_enospc_probe_fails_closed_with_forbidden_actions() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_359_000,
            "test.local_enospc",
            intercepted_command(),
        )
        .with_local_disk(RchAdmissionLocalDiskDiagnostic {
            system_data_free_bytes: Some(104_857_600),
            private_tmp_free_bytes: Some(104_857_600),
            repo_write_probe: RchAdmissionProbeDiagnostic::failed(
                "ENOSPC",
                "No space left on device",
            ),
            rch_cache_write_probe: RchAdmissionProbeDiagnostic::failed(
                "config.cache.write_failed",
                "RCH cache write failed before transfer",
            ),
        })
        .with_beads(RchAdmissionBeadsDiagnostic {
            db_writeable: Some(false),
            jsonl_writeable: Some(false),
            active_bead: Some("ft-e2egh".to_string()),
            blocking_beads: Vec::new(),
        });

        let report = build_rch_admission_report(&input);

        assert_eq!(report.contract_id, RCH_ADMISSION_CONTRACT_ID);
        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
        assert_eq!(
            report.reason_codes,
            vec![RchAdmissionReasonCode::LocalEnoSpace]
        );
        assert!(report.advisory_only);
        assert!(
            report
                .forbidden_actions
                .contains(&RchAdmissionForbiddenAction::RunLocalCargoAsProof)
        );
        assert!(
            report
                .forbidden_actions
                .contains(&RchAdmissionForbiddenAction::DeleteFilesWithoutApproval)
        );
        assert!(report.recommendations[0].operator_approval_required);
    }

    #[test]
    fn collector_observation_preserves_source_freshness_and_error_category() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.collectors",
            intercepted_command(),
        )
        .with_collector_observation(
            RchAdmissionCollectorObservation::new(
                "local_disk.system_data",
                "df -h /System/Volumes/Data",
                "local APFS data volume was near full",
            )
            .live(1_779_013_897_000, 1_000)
            .error_category("ENOSPC")
            .citation_path("/System/Volumes/Data"),
        );

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::LocalEnoSpace)
        );
        assert_eq!(report.citations.len(), 1);
        let citation = &report.citations[0];
        assert_eq!(citation.kind, RchAdmissionCitationKind::CommandOutput);
        assert_eq!(citation.path.as_deref(), Some("/System/Volumes/Data"));
        assert!(citation.summary.contains("df -h /System/Volumes/Data"));
        assert!(citation.summary.contains("freshness_ms=1000"));
        assert!(citation.summary.contains("error_category=ENOSPC"));
    }

    #[test]
    fn rch_queue_and_worker_rejections_normalize_reason_codes() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.rch_queue",
            intercepted_command(),
        )
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("degraded".to_string()),
            active_project_exclusion: true,
            active_builds: 1,
            queued_builds: 2,
            workers_healthy: Some(0),
            workers_total: Some(8),
        })
        .with_estimated_slots(4)
        .with_worker_rejection(RchAdmissionWorkerRejection::new(
            Some("vmi1149989"),
            RchAdmissionReasonCode::CriticalPressure,
            "critical_pressure=5",
            RchAdmissionSeverity::Critical,
        ));

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::ActiveProjectExclusion)
        );
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::CriticalPressure)
        );
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::NoAdmissibleWorkers)
        );
        assert_eq!(
            report.worker_rejections[0].worker.as_deref(),
            Some("vmi1149989")
        );
    }

    #[test]
    fn beads_and_git_context_are_retained_as_citations_without_new_schema_fields() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.coordination",
            intercepted_command(),
        )
        .with_beads(RchAdmissionBeadsDiagnostic {
            db_writeable: Some(true),
            jsonl_writeable: Some(true),
            active_bead: Some("ft-69gwh.2".to_string()),
            blocking_beads: vec!["ft-4tp7g".to_string()],
        })
        .with_ready_bead("ft-69gwh.3")
        .with_in_progress_bead("ft-fyk4x.1")
        .with_git_dirty_path(RchAdmissionGitDirtyPath::new(
            "docs/json-schema/PROVENANCE.md",
            "M",
            "other_agent",
        ));

        let report = build_rch_admission_report(&input);

        assert_eq!(report.beads.active_bead.as_deref(), Some("ft-69gwh.2"));
        assert_eq!(report.beads.blocking_beads, vec!["ft-4tp7g"]);
        assert!(
            report
                .citations
                .iter()
                .any(|citation| citation.summary.contains("ready bead ft-69gwh.3"))
        );
        assert!(
            report
                .citations
                .iter()
                .any(|citation| citation.summary.contains("in-progress bead ft-fyk4x.1"))
        );
        assert!(
            report
                .citations
                .iter()
                .any(|citation| citation.summary.contains("git status reported M"))
        );
    }
}
