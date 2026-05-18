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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RchAdmissionCargoJobSource {
    CargoJobsFlag,
    CargoBuildJobsEnv,
    InstalledSelectorEstimate,
    Default,
}

impl RchAdmissionCargoJobSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CargoJobsFlag => "cargo_jobs_flag",
            Self::CargoBuildJobsEnv => "cargo_build_jobs_env",
            Self::InstalledSelectorEstimate => "installed_selector_estimate",
            Self::Default => "default",
        }
    }

    #[must_use]
    pub fn explicit(self) -> bool {
        matches!(self, Self::CargoJobsFlag | Self::CargoBuildJobsEnv)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RchAdmissionCargoCommandAnalysis {
    pub raw: String,
    pub normalized: String,
    pub classification: String,
    pub would_intercept: bool,
    pub target_dir: Option<String>,
    pub cargo_subcommand: Option<String>,
    pub package_scope: Vec<String>,
    pub test_scope: Vec<String>,
    pub explicit_jobs: Option<u32>,
    pub effective_jobs: u32,
    pub job_source: RchAdmissionCargoJobSource,
    pub estimated_slots: u32,
    pub installed_selector_estimated_slots: Option<u32>,
    pub slot_estimate_mismatch: bool,
    pub explanation: String,
}

impl RchAdmissionCargoCommandAnalysis {
    #[must_use]
    pub fn command_diagnostic(&self) -> RchAdmissionCommandDiagnostic {
        RchAdmissionCommandDiagnostic::new(self.raw.clone())
            .normalized(self.normalized.clone())
            .classification(self.classification.clone())
            .would_intercept(self.would_intercept)
            .maybe_target_dir(self.target_dir.clone())
    }

    #[must_use]
    pub fn collector_observation(&self) -> RchAdmissionCollectorObservation {
        RchAdmissionCollectorObservation::new(
            "rch_admission.cargo_command_analysis",
            "pure cargo command analyzer",
            self.explanation.clone(),
        )
    }
}

impl RchAdmissionCommandDiagnostic {
    #[must_use]
    fn maybe_target_dir(mut self, target_dir: Option<String>) -> Self {
        self.target_dir = target_dir;
        self
    }
}

#[must_use]
pub fn analyze_rch_admission_cargo_command<I, K, V>(
    raw: impl AsRef<str>,
    env: I,
    installed_selector_estimated_slots: Option<u32>,
) -> RchAdmissionCargoCommandAnalysis
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let raw = raw.as_ref().trim().to_string();
    let tokens = shell_words_lossy(&raw);
    let cargo_index = tokens.iter().position(|token| token == "cargo");
    let mut command_env = env
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_string(), value.as_ref().to_string()))
        .collect::<Vec<_>>();
    collect_inline_env_assignments(&tokens, cargo_index, &mut command_env);

    let installed_selector_estimated_slots =
        installed_selector_estimated_slots.map(|value| value.max(1));
    let Some(cargo_index) = cargo_index else {
        return RchAdmissionCargoCommandAnalysis {
            raw: nonempty_string(raw, "unknown"),
            normalized: "non-cargo command".to_string(),
            classification: "non_cargo".to_string(),
            would_intercept: false,
            target_dir: env_value(&command_env, "CARGO_TARGET_DIR"),
            cargo_subcommand: None,
            package_scope: Vec::new(),
            test_scope: Vec::new(),
            explicit_jobs: None,
            effective_jobs: 1,
            job_source: RchAdmissionCargoJobSource::Default,
            estimated_slots: 1,
            installed_selector_estimated_slots,
            slot_estimate_mismatch: installed_selector_estimated_slots
                .is_some_and(|slots| slots != 1),
            explanation: "command analyzer did not find a cargo invocation".to_string(),
        };
    };

    let cargo_tokens = tokens[cargo_index..].to_vec();
    let normalized = cargo_tokens.join(" ");
    let mut cargo_subcommand = None;
    let mut package_scope = Vec::new();
    let mut test_scope = Vec::new();
    let mut jobs_flag = None;
    let mut target_dir = env_value(&command_env, "CARGO_TARGET_DIR");
    let mut i = 1;
    let mut after_double_dash = false;

    while i < cargo_tokens.len() {
        let token = &cargo_tokens[i];
        if token == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if after_double_dash {
            i += 1;
            continue;
        }

        if let Some(value) = token.strip_prefix("--jobs=") {
            jobs_flag = parse_positive_u32(value);
            i += 1;
            continue;
        }
        if let Some(value) = token.strip_prefix("-j") {
            if !value.is_empty() {
                jobs_flag = parse_positive_u32(value);
                i += 1;
                continue;
            }
        }
        if matches!(token.as_str(), "-j" | "--jobs") {
            if let Some(value) = cargo_tokens
                .get(i + 1)
                .and_then(|value| parse_positive_u32(value))
            {
                jobs_flag = Some(value);
            }
            i += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--target-dir=") {
            target_dir = Some(nonempty_string(value, "unknown"));
            i += 1;
            continue;
        }
        if token == "--target-dir" {
            if let Some(value) = cargo_tokens.get(i + 1) {
                target_dir = Some(nonempty_string(value, "unknown"));
            }
            i += 2;
            continue;
        }

        if token.starts_with("--exclude=") {
            i += 1;
            continue;
        }
        if token == "--exclude" {
            i += 2;
            continue;
        }

        if let Some(value) = token.strip_prefix("--package=") {
            push_nonempty_unique(&mut package_scope, value);
            i += 1;
            continue;
        }
        if matches!(token.as_str(), "-p" | "--package") {
            if let Some(value) = cargo_tokens.get(i + 1) {
                push_nonempty_unique(&mut package_scope, value);
            }
            i += 2;
            continue;
        }

        if cargo_subcommand.is_none() && !token.starts_with('-') && !token.starts_with('+') {
            cargo_subcommand = Some(token.clone());
            i += 1;
            continue;
        }

        if cargo_subcommand.as_deref() == Some("test") && !token.starts_with('-') {
            push_nonempty_unique(&mut test_scope, token);
        }

        if cargo_option_takes_value(token) {
            i += 2;
        } else {
            i += 1;
        }
    }

    let env_jobs =
        env_value(&command_env, "CARGO_BUILD_JOBS").and_then(|value| parse_positive_u32(&value));
    let (explicit_jobs, effective_jobs, job_source) = if let Some(jobs) = jobs_flag {
        (Some(jobs), jobs, RchAdmissionCargoJobSource::CargoJobsFlag)
    } else if let Some(jobs) = env_jobs {
        (
            Some(jobs),
            jobs,
            RchAdmissionCargoJobSource::CargoBuildJobsEnv,
        )
    } else if let Some(slots) = installed_selector_estimated_slots {
        (
            None,
            slots,
            RchAdmissionCargoJobSource::InstalledSelectorEstimate,
        )
    } else {
        (None, 1, RchAdmissionCargoJobSource::Default)
    };
    let estimated_slots = effective_jobs.max(1);
    let slot_estimate_mismatch = installed_selector_estimated_slots
        .is_some_and(|installed_slots| installed_slots != estimated_slots);
    let classification = classify_cargo_command(cargo_subcommand.as_deref());
    let explanation = cargo_analysis_explanation(
        explicit_jobs,
        effective_jobs,
        job_source,
        estimated_slots,
        installed_selector_estimated_slots,
        slot_estimate_mismatch,
        &package_scope,
        &test_scope,
        target_dir.as_deref(),
    );

    RchAdmissionCargoCommandAnalysis {
        raw: nonempty_string(raw, "unknown"),
        normalized,
        classification,
        would_intercept: true,
        target_dir,
        cargo_subcommand,
        package_scope,
        test_scope,
        explicit_jobs,
        effective_jobs,
        job_source,
        estimated_slots,
        installed_selector_estimated_slots,
        slot_estimate_mismatch,
        explanation,
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
    pub fn with_cargo_command_analysis(
        mut self,
        analysis: &RchAdmissionCargoCommandAnalysis,
    ) -> Self {
        self.command = analysis.command_diagnostic();
        self.cargo_jobs = analysis.explicit_jobs;
        self.estimated_slots = Some(analysis.estimated_slots);
        self.collector_observations
            .push(analysis.collector_observation());
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
        if let Some(error_category) = observation.error_category.as_deref() {
            for reason_code in reason_codes_from_error_category(error_category) {
                reason_codes.insert(reason_code);
            }
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

fn reason_codes_from_error_category(error_category: &str) -> Vec<RchAdmissionReasonCode> {
    let normalized = error_category.to_ascii_lowercase();
    let mut reason_codes = Vec::new();
    if looks_like_enospc(&normalized) || normalized.contains("cache.write_failed") {
        reason_codes.push(RchAdmissionReasonCode::LocalEnoSpace);
    }
    if normalized.contains("no_admissible") || normalized.contains("worker=null") {
        reason_codes.push(RchAdmissionReasonCode::NoAdmissibleWorkers);
    }
    if normalized.contains("critical_pressure") || normalized.contains("pressure-critical") {
        reason_codes.push(RchAdmissionReasonCode::CriticalPressure);
    }
    if normalized.contains("telemetry_gap") || normalized.contains("stale_telemetry") {
        reason_codes.push(RchAdmissionReasonCode::TelemetryGap);
    }
    if normalized.contains("insufficient_slots") {
        reason_codes.push(RchAdmissionReasonCode::InsufficientSlots);
    }
    if normalized.contains("active_project_exclusion") {
        reason_codes.push(RchAdmissionReasonCode::ActiveProjectExclusion);
    }
    if normalized.contains("speedscore") || normalized.contains("response_shape") {
        reason_codes.push(RchAdmissionReasonCode::SpeedscoreResponseShape);
    }
    if normalized.contains("dry_run") && normalized.contains("worker") {
        reason_codes.push(RchAdmissionReasonCode::DryRunInconsistentWorker);
    }
    reason_codes
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

fn shell_words_lossy(input: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;

    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if let Some(quote_ch) = quote {
            if ch == quote_ch {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if matches!(ch, '\'' | '"') {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }

    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn collect_inline_env_assignments(
    tokens: &[String],
    cargo_index: Option<usize>,
    command_env: &mut Vec<(String, String)>,
) {
    let limit = cargo_index.unwrap_or(tokens.len());
    for token in tokens.iter().take(limit) {
        if token == "env"
            || token == "--"
            || token == "rch"
            || token == "exec"
            || token == "diagnose"
        {
            continue;
        }
        if let Some((key, value)) = token.split_once('=') {
            if is_supported_cargo_env_key(key) {
                command_env.push((key.to_string(), value.to_string()));
            }
        }
    }
}

fn is_supported_cargo_env_key(key: &str) -> bool {
    matches!(key, "CARGO_BUILD_JOBS" | "CARGO_TARGET_DIR")
}

fn env_value(command_env: &[(String, String)], key: &str) -> Option<String> {
    command_env.iter().rev().find_map(|(env_key, env_value)| {
        (env_key == key).then(|| nonempty_string(env_value.clone(), "unknown"))
    })
}

fn parse_positive_u32(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|value| *value > 0)
}

fn cargo_option_takes_value(token: &str) -> bool {
    matches!(
        token,
        "--bin"
            | "--bench"
            | "--example"
            | "--features"
            | "--manifest-path"
            | "--target"
            | "--target-dir"
            | "--profile"
            | "--message-format"
            | "--config"
            | "-p"
            | "--package"
            | "-j"
            | "--jobs"
    )
}

fn classify_cargo_command(cargo_subcommand: Option<&str>) -> String {
    match cargo_subcommand {
        Some("test") => "cargo_test",
        Some("check") => "cargo_check",
        Some("clippy") => "cargo_clippy",
        Some("build") => "cargo_build",
        Some("bench") => "cargo_bench",
        Some("fuzz") => "cargo_fuzz",
        Some(other) if !other.trim().is_empty() => "cargo_other",
        _ => "cargo_unknown",
    }
    .to_string()
}

fn cargo_analysis_explanation(
    explicit_jobs: Option<u32>,
    effective_jobs: u32,
    job_source: RchAdmissionCargoJobSource,
    estimated_slots: u32,
    installed_selector_estimated_slots: Option<u32>,
    slot_estimate_mismatch: bool,
    package_scope: &[String],
    test_scope: &[String],
    target_dir: Option<&str>,
) -> String {
    let job_phrase = if let Some(explicit_jobs) = explicit_jobs {
        format!(
            "explicit cargo job count {explicit_jobs} from {}",
            job_source.as_str()
        )
    } else {
        format!(
            "inferred cargo job count {effective_jobs} from {}",
            job_source.as_str()
        )
    };
    let selector_phrase = installed_selector_estimated_slots.map_or_else(
        || "installed_selector_estimated_slots=unavailable".to_string(),
        |slots| format!("installed_selector_estimated_slots={slots}"),
    );
    let mismatch_phrase = if slot_estimate_mismatch {
        "slot_estimate_mismatch=true"
    } else {
        "slot_estimate_mismatch=false"
    };
    let package_phrase = if package_scope.is_empty() {
        "package_scope=workspace".to_string()
    } else {
        format!("package_scope={}", package_scope.join(","))
    };
    let test_phrase = if test_scope.is_empty() {
        "test_scope=unspecified".to_string()
    } else {
        format!("test_scope={}", test_scope.join(","))
    };
    let target_phrase = target_dir.map_or_else(
        || "target_dir=unspecified".to_string(),
        |target_dir| format!("target_dir={target_dir}"),
    );

    format!(
        "{job_phrase}; estimated_slots={estimated_slots}; {selector_phrase}; {mismatch_phrase}; {package_phrase}; {test_phrase}; {target_phrase}"
    )
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
    fn cargo_command_analysis_honors_jobs_flag_scope_and_selector_mismatch() {
        let analysis = analyze_rch_admission_cargo_command(
            "rch diagnose --dry-run -- env CARGO_TARGET_DIR=/tmp/ft-rch cargo test -j 1 -p frankenterm-core rch_admission --lib -- --nocapture",
            std::iter::empty::<(&str, &str)>(),
            Some(4),
        );

        assert_eq!(analysis.classification, "cargo_test");
        assert_eq!(analysis.target_dir.as_deref(), Some("/tmp/ft-rch"));
        assert_eq!(
            analysis.package_scope,
            vec![String::from("frankenterm-core")]
        );
        assert_eq!(analysis.test_scope, vec![String::from("rch_admission")]);
        assert_eq!(analysis.explicit_jobs, Some(1));
        assert_eq!(analysis.effective_jobs, 1);
        assert_eq!(analysis.estimated_slots, 1);
        assert_eq!(
            analysis.job_source,
            RchAdmissionCargoJobSource::CargoJobsFlag
        );
        assert!(analysis.slot_estimate_mismatch);
        assert!(analysis.explanation.contains("explicit cargo job count 1"));
        assert!(
            analysis
                .explanation
                .contains("installed_selector_estimated_slots=4")
        );
        assert!(analysis.explanation.contains("slot_estimate_mismatch=true"));
    }

    #[test]
    fn cargo_command_analysis_does_not_treat_excluded_packages_as_test_filters() {
        let analysis = analyze_rch_admission_cargo_command(
            "cargo test --workspace --exclude frankenterm-core --exclude=frankenterm-gui rch_admission --all-targets",
            std::iter::empty::<(&str, &str)>(),
            Some(2),
        );

        assert_eq!(analysis.classification, "cargo_test");
        assert!(analysis.package_scope.is_empty());
        assert_eq!(analysis.test_scope, vec![String::from("rch_admission")]);
        assert_eq!(analysis.effective_jobs, 2);
    }

    #[test]
    fn cargo_command_analysis_honors_env_jobs_and_target_dir_flag() {
        let analysis = analyze_rch_admission_cargo_command(
            "cargo check -p mux --target-dir /tmp/target-rch --all-targets",
            [("CARGO_BUILD_JOBS", "2")],
            Some(2),
        );

        assert_eq!(analysis.classification, "cargo_check");
        assert_eq!(analysis.target_dir.as_deref(), Some("/tmp/target-rch"));
        assert_eq!(analysis.package_scope, vec![String::from("mux")]);
        assert!(analysis.test_scope.is_empty());
        assert_eq!(analysis.explicit_jobs, Some(2));
        assert_eq!(
            analysis.job_source,
            RchAdmissionCargoJobSource::CargoBuildJobsEnv
        );
        assert_eq!(analysis.estimated_slots, 2);
        assert!(!analysis.slot_estimate_mismatch);
        assert!(analysis.explanation.contains("explicit cargo job count 2"));
    }

    #[test]
    fn cargo_command_analysis_uses_installed_selector_when_jobs_are_inferred() {
        let analysis = analyze_rch_admission_cargo_command(
            "cargo clippy --workspace --all-targets -- -D warnings",
            std::iter::empty::<(&str, &str)>(),
            Some(4),
        );

        assert_eq!(analysis.classification, "cargo_clippy");
        assert_eq!(analysis.explicit_jobs, None);
        assert_eq!(analysis.effective_jobs, 4);
        assert_eq!(analysis.estimated_slots, 4);
        assert_eq!(
            analysis.job_source,
            RchAdmissionCargoJobSource::InstalledSelectorEstimate
        );
        assert!(!analysis.slot_estimate_mismatch);
        assert!(analysis.explanation.contains("inferred cargo job count 4"));
    }

    #[test]
    fn cargo_command_analysis_populates_report_fields_and_citation() {
        let analysis = analyze_rch_admission_cargo_command(
            "rch exec -- env CARGO_TARGET_DIR=/tmp/ft-proof cargo test -p frankenterm-core rch_admission --lib",
            [("CARGO_BUILD_JOBS", "1")],
            Some(4),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.cargo_command_analysis",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analysis);

        let report = build_rch_admission_report(&input);

        assert_eq!(report.command.classification.as_deref(), Some("cargo_test"));
        assert_eq!(report.command.target_dir.as_deref(), Some("/tmp/ft-proof"));
        assert_eq!(report.cargo_jobs, Some(1));
        assert_eq!(report.estimated_slots, Some(1));
        assert!(report.citations.iter().any(|citation| {
            citation.summary.contains("explicit cargo job count 1")
                && citation.summary.contains("slot_estimate_mismatch=true")
        }));
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
    fn compound_error_category_preserves_all_blocking_reasons() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.compound_error_category",
            intercepted_command(),
        )
        .with_collector_observation(
            RchAdmissionCollectorObservation::new(
                "rch.diagnose.worker_selection",
                "rch diagnose --json --dry-run",
                "worker selection skipped before transfer",
            )
            .error_category("no_admissible_workers=critical_pressure=5"),
        );

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::NoAdmissibleWorkers)
        );
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::CriticalPressure)
        );
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
