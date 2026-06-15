//! Read-only RCH admission collector normalization.
//!
//! This module does not execute subprocesses, mutate Beads, touch Agent Mail,
//! restart services, inspect worker mirrors, run Cargo, or delete files. It
//! consumes already-collected facts from future CLI/doctor surfaces and emits
//! the static `ft.rch_admission.v1` contract.

#![allow(clippy::module_name_repetitions)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::proof_intent::{
    ProofIntent, ProofIntentEnvVar, ProofIntentQueueEntry, ProofKind, ProofRedactionPolicy,
    ProofScope,
};

pub const RCH_ADMISSION_CONTRACT_ID: &str = "ft.rch_admission.v1";
pub const RCH_ADMISSION_SCHEMA_VERSION: u16 = 1;
pub const RCH_ADMISSION_PREFLIGHT_CONTRACT_ID: &str = "ft.rch_admission.preflight.v1";

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
    WorkerToolchainMissingTarget,
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
            Self::WorkerToolchainMissingTarget => {
                "Keep the proof bead blocked until the selected worker has the requested Rust target or is quarantined for that target."
            }
            Self::Unknown => "Retain the artifact and file or update a Beads diagnosis.",
        }
    }

    #[must_use]
    pub fn operator_approval_required(self) -> bool {
        matches!(
            self,
            Self::LocalEnoSpace | Self::CriticalPressure | Self::WorkerToolchainMissingTarget
        )
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
                | Self::WorkerToolchainMissingTarget
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
pub enum RchAdmissionPreflightVerdict {
    Admitted,
    Deferred,
    Invalid,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionTargetDirHygiene {
    UniqueTmpTarget,
    ExplicitNonTmpTarget,
    SharedWorkspaceTarget,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionProofCommandRisk {
    NonCargoCommand,
    UnsupportedProofKind,
    MultiPackageScope,
    MissingRchExecWrapper,
    MissingRemoteRequiredEnv,
    MissingNoSelfHealingEnv,
    LocalFallbackRequested,
    MissingNoSelfHealingFlag,
    MissingTargetDir,
    SharedWorkspaceTargetDir,
}

impl RchAdmissionProofCommandRisk {
    #[must_use]
    fn permits_local_fallback(self) -> bool {
        matches!(
            self,
            Self::MissingRemoteRequiredEnv
                | Self::MissingNoSelfHealingEnv
                | Self::LocalFallbackRequested
                | Self::MissingNoSelfHealingFlag
        )
    }

    #[must_use]
    fn invalidates_hygiene(self) -> bool {
        matches!(
            self,
            Self::NonCargoCommand
                | Self::UnsupportedProofKind
                | Self::MultiPackageScope
                | Self::MissingRchExecWrapper
                | Self::MissingTargetDir
                | Self::SharedWorkspaceTargetDir
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionProofCommandAnalysis {
    pub raw: String,
    pub normalized: String,
    pub classification: String,
    pub proof_kind: Option<ProofKind>,
    pub proof_scope: Option<ProofScope>,
    pub package_scope: Vec<String>,
    pub test_scope: Vec<String>,
    pub target_dir: Option<String>,
    pub target_triple: Option<String>,
    pub estimated_slots: u32,
    pub target_dir_hygiene: RchAdmissionTargetDirHygiene,
    pub remote_required: bool,
    pub no_self_healing: bool,
    pub rch_exec_wrapped: bool,
    pub risks: Vec<RchAdmissionProofCommandRisk>,
    pub proof_intent_compatible: bool,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchAdmissionPreflightReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub predicted_at_ms: u64,
    pub advisory_only: bool,
    pub verdict: RchAdmissionPreflightVerdict,
    pub rch_admission_state: String,
    pub queue_intent_recommended: bool,
    pub selected_worker: Option<String>,
    pub estimated_slots: Option<u32>,
    pub reason_codes: Vec<RchAdmissionReasonCode>,
    pub proof_command: RchAdmissionProofCommandAnalysis,
    pub admission_report: RchAdmissionReport,
    pub summary: String,
}

impl RchAdmissionPreflightReport {
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn to_deferred_proof_intent(
        &self,
        source_hash: impl Into<String>,
        expected_artifact_path: Option<String>,
        bead_id: Option<String>,
        attestation_slot: Option<String>,
        redaction_policy: ProofRedactionPolicy,
        created_at_ms: i64,
    ) -> Option<ProofIntent> {
        if self.verdict != RchAdmissionPreflightVerdict::Deferred || !self.queue_intent_recommended
        {
            return None;
        }

        Some(ProofIntent::new(
            self.proof_command.raw.clone(),
            self.proof_command.proof_scope.clone()?,
            self.proof_command.proof_kind?,
            source_hash,
            expected_artifact_path,
            true,
            bead_id,
            attestation_slot,
            redaction_policy,
            created_at_ms,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn to_deferred_proof_intent_queue_entry(
        &self,
        source_hash: impl Into<String>,
        expected_artifact_path: Option<String>,
        bead_id: Option<String>,
        attestation_slot: Option<String>,
        redaction_policy: ProofRedactionPolicy,
        queued_at_ms: i64,
    ) -> Option<ProofIntentQueueEntry> {
        let intent = self.to_deferred_proof_intent(
            source_hash,
            expected_artifact_path,
            bead_id,
            attestation_slot,
            redaction_policy,
            queued_at_ms,
        )?;
        let (command_env, command_argv) = proof_command_replay_parts(&self.proof_command.raw);

        Some(ProofIntentQueueEntry::new(
            intent,
            command_argv,
            command_env,
            self.proof_command.target_dir.clone(),
            self.rch_admission_state.clone(),
            queued_at_ms,
        ))
    }
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
    pub target_triple: Option<String>,
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
    let words = shell_words_lossy(&raw);
    let cargo_index = words.iter().position(|word| word == "cargo");
    let mut command_env = env
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_string(), value.as_ref().to_string()))
        .collect::<Vec<_>>();
    collect_inline_env_assignments(&words, cargo_index, &mut command_env);

    let installed_selector_estimated_slots =
        installed_selector_estimated_slots.map(|value| value.max(1));
    let Some(cargo_index) = cargo_index else {
        return RchAdmissionCargoCommandAnalysis {
            raw: nonempty_string(raw, "unknown"),
            normalized: "non-cargo command".to_string(),
            classification: "non_cargo".to_string(),
            would_intercept: false,
            target_dir: env_value(&command_env, "CARGO_TARGET_DIR"),
            target_triple: None,
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

    let cargo_words = words.get(cargo_index..).unwrap_or(&[]).to_vec();
    let normalized = cargo_words.join(" ");
    let mut cargo_subcommand = None;
    let mut package_scope = Vec::new();
    let mut test_scope = Vec::new();
    let mut jobs_flag = None;
    let mut target_dir = env_value(&command_env, "CARGO_TARGET_DIR");
    let mut target_triple = None;
    let mut i = 1;
    let mut after_double_dash = false;

    while let Some(word) = cargo_words.get(i) {
        if word == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if after_double_dash {
            i += 1;
            continue;
        }

        if let Some(value) = word.strip_prefix("--jobs=") {
            jobs_flag = parse_positive_u32(value);
            i += 1;
            continue;
        }
        if let Some(value) = word.strip_prefix("-j") {
            if !value.is_empty() {
                jobs_flag = parse_positive_u32(value);
                i += 1;
                continue;
            }
        }
        if matches!(word.as_str(), "-j" | "--jobs") {
            if let Some(value) = cargo_words
                .get(i + 1)
                .and_then(|value| parse_positive_u32(value))
            {
                jobs_flag = Some(value);
            }
            i += 2;
            continue;
        }

        if let Some(value) = word.strip_prefix("--target=") {
            target_triple = Some(nonempty_string(value, "unknown"));
            i += 1;
            continue;
        }
        if word == "--target" {
            if let Some(value) = cargo_words.get(i + 1) {
                target_triple = Some(nonempty_string(value.as_str(), "unknown"));
            }
            i += 2;
            continue;
        }

        if let Some(value) = word.strip_prefix("--target-dir=") {
            target_dir = Some(nonempty_string(value, "unknown"));
            i += 1;
            continue;
        }
        if word == "--target-dir" {
            if let Some(value) = cargo_words.get(i + 1) {
                target_dir = Some(nonempty_string(value, "unknown"));
            }
            i += 2;
            continue;
        }

        if word.starts_with("--exclude=") {
            i += 1;
            continue;
        }
        if word == "--exclude" {
            i += 2;
            continue;
        }

        if let Some(value) = word.strip_prefix("--package=") {
            push_nonempty_unique(&mut package_scope, value);
            i += 1;
            continue;
        }
        if matches!(word.as_str(), "-p" | "--package") {
            if let Some(value) = cargo_words.get(i + 1) {
                push_nonempty_unique(&mut package_scope, value);
            }
            i += 2;
            continue;
        }

        if cargo_subcommand.is_none() && !word.starts_with('-') && !word.starts_with('+') {
            cargo_subcommand = Some(word.clone());
            i += 1;
            continue;
        }

        if cargo_subcommand.as_deref() == Some("test") && !word.starts_with('-') {
            push_nonempty_unique(&mut test_scope, word);
        }

        if cargo_option_takes_value(word) {
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
    let explanation = cargo_analysis_explanation(CargoAnalysisExplanationInput {
        explicit_jobs,
        effective_jobs,
        job_source,
        estimated_slots,
        installed_selector_estimated_slots,
        slot_estimate_mismatch,
        package_scope: &package_scope,
        test_scope: &test_scope,
        target_dir: target_dir.as_deref(),
        target_triple: target_triple.as_deref(),
    });

    RchAdmissionCargoCommandAnalysis {
        raw: nonempty_string(raw, "unknown"),
        normalized,
        classification,
        would_intercept: true,
        target_dir,
        target_triple,
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

#[must_use]
pub fn analyze_rch_admission_proof_command<I, K, V>(
    raw: impl AsRef<str>,
    env: I,
    installed_selector_estimated_slots: Option<u32>,
) -> RchAdmissionProofCommandAnalysis
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
{
    let raw = raw.as_ref().trim().to_string();
    let words = shell_words_lossy(&raw);
    let command_env = env
        .into_iter()
        .map(|(key, value)| (key.as_ref().to_string(), value.as_ref().to_string()))
        .collect::<Vec<_>>();
    let cargo = analyze_rch_admission_cargo_command(
        &raw,
        command_env
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str())),
        installed_selector_estimated_slots,
    );
    proof_command_analysis_from_cargo(raw, &words, &cargo)
}

#[must_use]
pub fn build_rch_admission_preflight_report(
    input: &RchAdmissionCollectorInput,
    proof_command: RchAdmissionProofCommandAnalysis,
) -> RchAdmissionPreflightReport {
    let admission_report = build_rch_admission_report(input);
    let command_permits_local_fallback = proof_command
        .risks
        .iter()
        .any(|risk| risk.permits_local_fallback());
    let command_requests_local_fallback = proof_command
        .risks
        .contains(&RchAdmissionProofCommandRisk::LocalFallbackRequested);
    let command_invalid = proof_command
        .risks
        .iter()
        .any(|risk| risk.invalidates_hygiene());

    let (verdict, rch_admission_state, queue_intent_recommended, summary) =
        if command_requests_local_fallback {
            (
                RchAdmissionPreflightVerdict::Blocked,
                "blocked_command".to_string(),
                false,
                format!(
                    "blocked: command permits local fallback; predicted_at_ms={}",
                    input.generated_at_ms
                ),
            )
        } else if command_invalid {
            (
                RchAdmissionPreflightVerdict::Invalid,
                "invalid".to_string(),
                false,
                format!(
                    "invalid: local-only or target-dir hygiene cannot produce RCH proof; predicted_at_ms={}",
                    input.generated_at_ms
                ),
            )
        } else if command_permits_local_fallback {
            (
                RchAdmissionPreflightVerdict::Blocked,
                "blocked_command".to_string(),
                false,
                format!(
                    "blocked: command permits local fallback; predicted_at_ms={}",
                    input.generated_at_ms
                ),
            )
        } else if admission_report.proof_status == RchAdmissionProofStatus::Runnable {
            (
                RchAdmissionPreflightVerdict::Admitted,
                "admitted".to_string(),
                false,
                admitted_preflight_summary(
                    input.generated_at_ms,
                    &proof_command,
                    &admission_report,
                ),
            )
        } else {
            (
                RchAdmissionPreflightVerdict::Deferred,
                "wait_rch".to_string(),
                proof_command.proof_intent_compatible,
                deferred_preflight_summary(input.generated_at_ms, &admission_report),
            )
        };

    RchAdmissionPreflightReport {
        schema_version: RCH_ADMISSION_SCHEMA_VERSION,
        contract_id: RCH_ADMISSION_PREFLIGHT_CONTRACT_ID.to_string(),
        predicted_at_ms: input.generated_at_ms,
        advisory_only: true,
        verdict,
        rch_admission_state,
        queue_intent_recommended,
        selected_worker: admission_report.rch_queue.selected_worker.clone(),
        estimated_slots: admission_report.estimated_slots,
        reason_codes: admission_report.reason_codes.clone(),
        proof_command,
        admission_report,
        summary,
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
    pub selected_worker: Option<String>,
    pub worker_slots_available: Option<u32>,
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
            selected_worker: None,
            worker_slots_available: None,
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
        if worker_rejection_blocks_proof(input, rejection) {
            reason_codes.insert(rejection.reason_code);
        }
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
        && input.rch_queue.selected_worker.is_none()
    {
        reason_codes.insert(RchAdmissionReasonCode::NoAdmissibleWorkers);
    }
    let available_slots = input
        .rch_queue
        .worker_slots_available
        .or(input.rch_queue.workers_healthy);
    if let (Some(estimated_slots), Some(available_slots)) = (input.estimated_slots, available_slots)
    {
        if available_slots > 0 && estimated_slots > available_slots {
            reason_codes.insert(RchAdmissionReasonCode::InsufficientSlots);
        }
    }
}

fn worker_rejection_blocks_proof(
    input: &RchAdmissionCollectorInput,
    rejection: &RchAdmissionWorkerRejection,
) -> bool {
    input.rch_queue.selected_worker.is_none()
        || rejection.worker.is_none()
        || rejection.reason_code == RchAdmissionReasonCode::NoAdmissibleWorkers
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
    if normalized.contains("no_admissible")
        || normalized.contains("no_workers_passed_health")
        || normalized.contains("worker=null")
    {
        reason_codes.push(RchAdmissionReasonCode::NoAdmissibleWorkers);
    }
    if normalized.contains("critical_pressure")
        || normalized.contains("pressure-critical")
        || normalized.contains("pressure_state=critical")
        || normalized.contains("disk_free_below_critical_gb")
        || normalized.contains("disk_ratio_below_critical")
        || normalized.contains("disk_critical_without_fresh_telemetry")
    {
        reason_codes.push(RchAdmissionReasonCode::CriticalPressure);
    }
    if normalized.contains("telemetry_gap")
        || normalized.contains("stale_telemetry")
        || normalized.contains("disk_metrics_unavailable")
        || normalized.contains("disk_critical_without_fresh_telemetry")
    {
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
    if normalized.contains("worker_toolchain_missing_target")
        || normalized.contains("missing_target_stdlib")
        || normalized.contains("target may not be installed")
        || normalized.contains("rustup target add")
        || normalized.contains("error[e0463]")
        || (normalized.contains("can't find crate for")
            && (normalized.contains("`core`") || normalized.contains(" core")))
    {
        reason_codes.push(RchAdmissionReasonCode::WorkerToolchainMissingTarget);
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
    words: &[String],
    cargo_index: Option<usize>,
    command_env: &mut Vec<(String, String)>,
) {
    let limit = cargo_index.unwrap_or(words.len());
    for word in words.iter().take(limit) {
        if word == "env" || word == "--" || word == "rch" || word == "exec" || word == "diagnose" {
            continue;
        }
        if let Some((key, value)) = word.split_once('=') {
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

fn cargo_option_takes_value(word: &str) -> bool {
    matches!(
        word,
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

struct CargoAnalysisExplanationInput<'a> {
    explicit_jobs: Option<u32>,
    effective_jobs: u32,
    job_source: RchAdmissionCargoJobSource,
    estimated_slots: u32,
    installed_selector_estimated_slots: Option<u32>,
    slot_estimate_mismatch: bool,
    package_scope: &'a [String],
    test_scope: &'a [String],
    target_dir: Option<&'a str>,
    target_triple: Option<&'a str>,
}

fn cargo_analysis_explanation(input: CargoAnalysisExplanationInput<'_>) -> String {
    let CargoAnalysisExplanationInput {
        explicit_jobs,
        effective_jobs,
        job_source,
        estimated_slots,
        installed_selector_estimated_slots,
        slot_estimate_mismatch,
        package_scope,
        test_scope,
        target_dir,
        target_triple,
    } = input;

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
    let target_triple_phrase = target_triple.map_or_else(
        || "target_triple=unspecified".to_string(),
        |target_triple| format!("target_triple={target_triple}"),
    );

    format!(
        "{job_phrase}; estimated_slots={estimated_slots}; {selector_phrase}; {mismatch_phrase}; {package_phrase}; {test_phrase}; {target_phrase}; {target_triple_phrase}"
    )
}

fn proof_command_analysis_from_cargo(
    raw: String,
    words: &[String],
    cargo: &RchAdmissionCargoCommandAnalysis,
) -> RchAdmissionProofCommandAnalysis {
    let proof_kind = proof_kind_for_classification(&cargo.classification);
    let proof_scope = proof_scope_for_packages(&cargo.package_scope);
    let target_dir_hygiene = classify_target_dir_hygiene(cargo.target_dir.as_deref());
    let remote_required = env_assignment_equals_before_cargo(words, "RCH_REQUIRE_REMOTE", "1");
    let no_self_healing_env = env_assignment_equals_before_cargo(words, "RCH_NO_SELF_HEALING", "1");
    let local_fallback_requested =
        explicit_env_assignment_not_equal_before_cargo(words, "RCH_REQUIRE_REMOTE", "1")
            || explicit_env_assignment_not_equal_before_cargo(words, "RCH_NO_SELF_HEALING", "1");
    let rch_exec_wrapped = rch_exec_wrapped(words);
    let no_self_healing_flag = rch_no_self_healing_flag(words);
    let no_self_healing = no_self_healing_env && no_self_healing_flag;
    let mut risks = Vec::new();

    if cargo.classification == "non_cargo" {
        push_risk(&mut risks, RchAdmissionProofCommandRisk::NonCargoCommand);
    }
    if proof_kind.is_none() && cargo.classification != "non_cargo" {
        push_risk(
            &mut risks,
            RchAdmissionProofCommandRisk::UnsupportedProofKind,
        );
    }
    if cargo.package_scope.len() > 1 {
        push_risk(&mut risks, RchAdmissionProofCommandRisk::MultiPackageScope);
    }
    if !rch_exec_wrapped {
        push_risk(
            &mut risks,
            RchAdmissionProofCommandRisk::MissingRchExecWrapper,
        );
    }
    if !remote_required {
        push_risk(
            &mut risks,
            RchAdmissionProofCommandRisk::MissingRemoteRequiredEnv,
        );
    }
    if !no_self_healing_env {
        push_risk(
            &mut risks,
            RchAdmissionProofCommandRisk::MissingNoSelfHealingEnv,
        );
    }
    if local_fallback_requested {
        push_risk(
            &mut risks,
            RchAdmissionProofCommandRisk::LocalFallbackRequested,
        );
    }
    if !no_self_healing_flag {
        push_risk(
            &mut risks,
            RchAdmissionProofCommandRisk::MissingNoSelfHealingFlag,
        );
    }
    match target_dir_hygiene {
        RchAdmissionTargetDirHygiene::Missing => {
            push_risk(&mut risks, RchAdmissionProofCommandRisk::MissingTargetDir);
        }
        RchAdmissionTargetDirHygiene::SharedWorkspaceTarget => {
            push_risk(
                &mut risks,
                RchAdmissionProofCommandRisk::SharedWorkspaceTargetDir,
            );
        }
        RchAdmissionTargetDirHygiene::UniqueTmpTarget
        | RchAdmissionTargetDirHygiene::ExplicitNonTmpTarget => {}
    }

    let proof_intent_compatible = proof_kind.is_some()
        && proof_scope.is_some()
        && !risks.iter().any(|risk| {
            matches!(
                risk,
                RchAdmissionProofCommandRisk::NonCargoCommand
                    | RchAdmissionProofCommandRisk::UnsupportedProofKind
                    | RchAdmissionProofCommandRisk::MultiPackageScope
            )
        });
    let explanation = proof_command_explanation(
        ProofCommandExplanationFlags {
            remote_required,
            no_self_healing,
            rch_exec_wrapped,
            proof_intent_compatible,
        },
        target_dir_hygiene,
        cargo,
    );

    RchAdmissionProofCommandAnalysis {
        raw,
        normalized: cargo.normalized.clone(),
        classification: cargo.classification.clone(),
        proof_kind,
        proof_scope,
        package_scope: cargo.package_scope.clone(),
        test_scope: cargo.test_scope.clone(),
        target_dir: cargo.target_dir.clone(),
        target_triple: cargo.target_triple.clone(),
        estimated_slots: cargo.estimated_slots,
        target_dir_hygiene,
        remote_required,
        no_self_healing,
        rch_exec_wrapped,
        risks,
        proof_intent_compatible,
        explanation,
    }
}

fn proof_kind_for_classification(classification: &str) -> Option<ProofKind> {
    match classification {
        "cargo_test" => Some(ProofKind::Test),
        "cargo_check" => Some(ProofKind::Check),
        "cargo_clippy" => Some(ProofKind::Clippy),
        "cargo_fuzz" => Some(ProofKind::Fuzz),
        _ => None,
    }
}

fn proof_scope_for_packages(packages: &[String]) -> Option<ProofScope> {
    match packages {
        [package] => Some(ProofScope::Package {
            package: package.clone(),
        }),
        [] => Some(ProofScope::Workspace),
        _ => None,
    }
}

fn classify_target_dir_hygiene(target_dir: Option<&str>) -> RchAdmissionTargetDirHygiene {
    let Some(target_dir) = target_dir.map(str::trim).filter(|value| !value.is_empty()) else {
        return RchAdmissionTargetDirHygiene::Missing;
    };
    if matches!(target_dir, "target" | "./target") || target_dir.ends_with("/target") {
        return RchAdmissionTargetDirHygiene::SharedWorkspaceTarget;
    }
    if target_dir.starts_with("/tmp/ft-") && target_dir.ends_with("-target") {
        return RchAdmissionTargetDirHygiene::UniqueTmpTarget;
    }
    RchAdmissionTargetDirHygiene::ExplicitNonTmpTarget
}

fn push_risk(risks: &mut Vec<RchAdmissionProofCommandRisk>, risk: RchAdmissionProofCommandRisk) {
    if !risks.contains(&risk) {
        risks.push(risk);
    }
}

fn proof_command_replay_parts(raw: &str) -> (Vec<ProofIntentEnvVar>, Vec<String>) {
    let words = shell_words_lossy(raw);
    let argv_start = words
        .iter()
        .position(|word| !is_shell_env_assignment(word))
        .unwrap_or(words.len());
    let command_env = words
        .iter()
        .take(argv_start)
        .filter_map(|word| {
            let (name, value) = word.split_once('=')?;
            Some(ProofIntentEnvVar {
                name: name.to_string(),
                value: value.to_string(),
            })
        })
        .collect();
    let command_argv = words.into_iter().skip(argv_start).collect();
    (command_env, command_argv)
}

fn is_shell_env_assignment(word: &str) -> bool {
    let Some((name, _value)) = word.split_once('=') else {
        return false;
    };
    let Some(first) = name.chars().next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && name
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn env_assignment_equals_before_cargo(words: &[String], name: &str, expected: &str) -> bool {
    words_before_cargo(words).any(|word| {
        word.split_once('=')
            .is_some_and(|(key, value)| key == name && value == expected)
    })
}

fn explicit_env_assignment_not_equal_before_cargo(
    words: &[String],
    name: &str,
    expected: &str,
) -> bool {
    words_before_cargo(words).any(|word| {
        word.split_once('=')
            .is_some_and(|(key, value)| key == name && value != expected)
    })
}

fn words_before_cargo(words: &[String]) -> impl Iterator<Item = &String> {
    let limit = words
        .iter()
        .position(|word| word == "cargo")
        .unwrap_or(words.len());
    words.iter().take(limit)
}

fn rch_exec_wrapped(words: &[String]) -> bool {
    let Some(rch_index) = words.iter().position(|word| word == "rch") else {
        return false;
    };
    let Some(exec_index) = words
        .iter()
        .enumerate()
        .skip(rch_index + 1)
        .find_map(|(index, word)| (word == "exec").then_some(index))
    else {
        return false;
    };
    words
        .get(exec_index + 1)
        .is_some_and(|word| word.as_str() == "--")
}

fn rch_no_self_healing_flag(words: &[String]) -> bool {
    let Some(rch_index) = words.iter().position(|word| word == "rch") else {
        return false;
    };
    let exec_index = words
        .iter()
        .enumerate()
        .skip(rch_index + 1)
        .find_map(|(index, word)| (word == "exec").then_some(index))
        .unwrap_or(words.len());
    words
        .iter()
        .skip(rch_index + 1)
        .take(exec_index.saturating_sub(rch_index + 1))
        .any(|word| word == "--no-self-healing")
}

struct ProofCommandExplanationFlags {
    remote_required: bool,
    no_self_healing: bool,
    rch_exec_wrapped: bool,
    proof_intent_compatible: bool,
}

fn proof_command_explanation(
    flags: ProofCommandExplanationFlags,
    target_dir_hygiene: RchAdmissionTargetDirHygiene,
    cargo: &RchAdmissionCargoCommandAnalysis,
) -> String {
    let ProofCommandExplanationFlags {
        remote_required,
        no_self_healing,
        rch_exec_wrapped,
        proof_intent_compatible,
    } = flags;
    format!(
        "remote_required={remote_required}; no_self_healing={no_self_healing}; rch_exec_wrapped={rch_exec_wrapped}; target_dir_hygiene={target_dir_hygiene:?}; proof_intent_compatible={proof_intent_compatible}; {}",
        cargo.explanation
    )
}

fn admitted_preflight_summary(
    predicted_at_ms: u64,
    proof_command: &RchAdmissionProofCommandAnalysis,
    admission_report: &RchAdmissionReport,
) -> String {
    let worker_count = admission_report
        .rch_queue
        .workers_healthy
        .or(admission_report.rch_queue.worker_slots_available)
        .unwrap_or(0);
    let scope = match &proof_command.proof_scope {
        Some(ProofScope::Package { .. }) => "package-scoped",
        Some(ProofScope::Workspace) => "workspace-scoped",
        None => "unknown-scope",
    };
    format!("admitted: {worker_count} worker(s), {scope}; predicted_at_ms={predicted_at_ms}")
}

fn deferred_preflight_summary(
    predicted_at_ms: u64,
    admission_report: &RchAdmissionReport,
) -> String {
    let reason = admission_report
        .reason_codes
        .first()
        .map_or_else(|| "unknown".to_string(), |reason| format!("{reason:?}"));
    format!("deferred: {reason}; queue intent; predicted_at_ms={predicted_at_ms}")
}

// ── Live admission collector (ft-69gwh.9) ──────────────────────────────────
//
// `build_rch_admission_report` requires an `RchAdmissionCollectorInput`. Before
// this collector existed that input was only ever constructed in tests, so the
// whole admission analyzer/report path was inert in production. These
// read-only probes (`df` / `git status` / `rch -F json status` / filesystem
// writeability) gather live host state into a populated input behind
// `ft doctor --rch-admission`. Parsing is split into pure, unit-tested helpers;
// `collect_live_rch_admission_input` is the thin process-spawning orchestrator.

/// Parse the `Available` column (1024-byte blocks) from `df -k <path>` output
/// into a byte count. Locates the column by its header (`Avail`…) so it works
/// across the macOS and Linux `df` header layouts.
#[must_use]
pub fn parse_df_available_bytes(df_output: &str) -> Option<u64> {
    let mut lines = df_output.lines().filter(|line| !line.trim().is_empty());
    let header = lines.next()?;
    let avail_idx = header
        .split_whitespace()
        .position(|field| field.to_ascii_lowercase().starts_with("avail"))?;
    let data = lines.find(|line| line.split_whitespace().count() > avail_idx)?;
    let blocks: u64 = data.split_whitespace().nth(avail_idx)?.parse().ok()?;
    Some(blocks.saturating_mul(1024))
}

fn porcelain_category(status: &str) -> &'static str {
    if status == "??" {
        "untracked"
    } else if status.contains('D') {
        "deleted"
    } else if status.contains('A') {
        "added"
    } else if status.contains('R') {
        "renamed"
    } else if status.contains('M') {
        "modified"
    } else {
        "dirty_tree"
    }
}

/// Parse `git status --porcelain` into dirty-path diagnostics (status code +
/// derived category). Lines shorter than `XY path` are ignored.
#[must_use]
pub fn parse_git_porcelain_dirty(porcelain: &str) -> Vec<RchAdmissionGitDirtyPath> {
    porcelain
        .lines()
        .filter(|line| line.len() >= 4)
        .filter_map(|line| {
            let status = line.get(0..2).unwrap_or("  ").trim();
            let path = line.get(3..).unwrap_or("").trim();
            if path.is_empty() {
                return None;
            }
            let category = porcelain_category(status);
            let status_label = if status.is_empty() { "dirty" } else { status };
            Some(RchAdmissionGitDirtyPath::new(path, status_label, category))
        })
        .collect()
}

fn rch_status_data(value: &serde_json::Value) -> &serde_json::Value {
    value.get("data").unwrap_or(value)
}

/// Parse `rch -F json status` into a queue diagnostic (posture + worker/slot
/// counts). Returns `unknown()` if the JSON is malformed or missing fields.
#[must_use]
pub fn parse_rch_status_queue(json: &str) -> RchAdmissionQueueDiagnostic {
    let mut queue = RchAdmissionQueueDiagnostic::unknown();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return queue;
    };
    let data = rch_status_data(&value);
    if let Some(posture) = data.get("posture").and_then(serde_json::Value::as_str) {
        queue.posture = Some(posture.to_string());
    }
    let daemon = data
        .get("daemon")
        .and_then(|outer| outer.get("daemon"))
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let as_u32 = |node: &serde_json::Value, key: &str| -> Option<u32> {
        node.get(key)
            .and_then(serde_json::Value::as_u64)
            .map(|n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    queue.workers_total = as_u32(&daemon, "workers_total");
    queue.workers_healthy = as_u32(&daemon, "workers_healthy");
    queue.worker_slots_available = as_u32(&daemon, "slots_available");
    queue
}

/// Derive worker-rejection diagnostics from any non-healthy workers in the
/// `rch -F json status` worker list.
#[must_use]
pub fn parse_rch_status_worker_rejections(json: &str) -> Vec<RchAdmissionWorkerRejection> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let data = rch_status_data(&value);
    let Some(workers) = data
        .get("daemon")
        .and_then(|outer| outer.get("workers"))
        .and_then(serde_json::Value::as_array)
    else {
        return Vec::new();
    };
    workers
        .iter()
        .filter_map(|worker| {
            let status = worker
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            if status == "healthy" {
                return None;
            }
            let id = worker
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let (reason_code, severity) = match status {
                "unhealthy" | "critical" | "offline" => (
                    RchAdmissionReasonCode::NoAdmissibleWorkers,
                    RchAdmissionSeverity::Blocked,
                ),
                _ => (
                    RchAdmissionReasonCode::TelemetryGap,
                    RchAdmissionSeverity::Warning,
                ),
            };
            Some(RchAdmissionWorkerRejection::new(
                id,
                reason_code,
                format!("worker status={status}"),
                severity,
            ))
        })
        .collect()
}

fn run_probe(cmd: &str, args: &[&str], cwd: &std::path::Path) -> Option<String> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        None
    }
}

fn write_probe(dir: &std::path::Path, label: &str) -> RchAdmissionProbeDiagnostic {
    let probe_path = dir.join(".ft-rch-admission-write-probe");
    match std::fs::write(&probe_path, b"ft") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe_path);
            RchAdmissionProbeDiagnostic::ok(format!("{label} writeable"))
        }
        Err(err) => {
            let code = err
                .raw_os_error()
                .filter(|c| *c == 28)
                .map_or_else(|| format!("{label}.write_failed"), |_| "ENOSPC".to_string());
            RchAdmissionProbeDiagnostic::failed(code, format!("{label} write failed: {err}"))
        }
    }
}

fn path_writeable(path: &std::path::Path) -> Option<bool> {
    std::fs::metadata(path)
        .ok()
        .map(|meta| !meta.permissions().readonly())
}

/// Gather live host/disk/beads/rch-queue/git state into a populated
/// `RchAdmissionCollectorInput` for `proof_command`. Read-only: every probe is
/// a status query or a self-cleaning write probe; failures are recorded as
/// collector observations rather than aborting the report.
#[must_use]
pub fn collect_live_rch_admission_input(
    repo_root: &std::path::Path,
    proof_command: &str,
    generated_at_ms: u64,
) -> RchAdmissionCollectorInput {
    let mut input = RchAdmissionCollectorInput::new(
        generated_at_ms,
        "ft doctor --rch-admission",
        RchAdmissionCommandDiagnostic::new(proof_command),
    );

    // Local disk: repo volume + /private/tmp free space, plus write probes.
    let system_data_free_bytes = run_probe("df", &["-k", "."], repo_root)
        .as_deref()
        .and_then(parse_df_available_bytes);
    let private_tmp_free_bytes = run_probe("df", &["-k", "/private/tmp"], repo_root)
        .as_deref()
        .and_then(parse_df_available_bytes)
        .or_else(|| {
            run_probe("df", &["-k", "/tmp"], repo_root)
                .as_deref()
                .and_then(parse_df_available_bytes)
        });
    input.local_disk = RchAdmissionLocalDiskDiagnostic {
        system_data_free_bytes,
        private_tmp_free_bytes,
        repo_write_probe: write_probe(repo_root, "repo"),
        rch_cache_write_probe: write_probe(&std::env::temp_dir(), "cargo target tmp"),
    };

    // Beads writeability (DB + JSONL) under .beads/.
    let beads_dir = repo_root.join(".beads");
    let db_writeable = std::fs::read_dir(&beads_dir).ok().and_then(|entries| {
        entries
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
            })
            .and_then(|entry| path_writeable(&entry.path()))
    });
    input.beads = RchAdmissionBeadsDiagnostic {
        db_writeable,
        jsonl_writeable: path_writeable(&beads_dir.join("issues.jsonl")),
        active_bead: None,
        blocking_beads: Vec::new(),
    };

    // RCH queue posture + worker rejections (status query, never a build).
    match run_probe("rch", &["-F", "json", "status"], repo_root) {
        Some(json) => {
            input.rch_queue = parse_rch_status_queue(&json);
            input.worker_rejections = parse_rch_status_worker_rejections(&json);
        }
        None => input.collector_observations.push(
            RchAdmissionCollectorObservation::new(
                "rch.status",
                "rch -F json status",
                "rch status unavailable; queue posture unknown",
            )
            .error_category("probe_unavailable"),
        ),
    }

    // Git dirty tree (RCH syncs the working tree, so dirty paths matter).
    match run_probe("git", &["status", "--porcelain"], repo_root) {
        Some(porcelain) => {
            for dirty in parse_git_porcelain_dirty(&porcelain) {
                input = input.with_git_dirty_path(dirty);
            }
        }
        None => input.collector_observations.push(
            RchAdmissionCollectorObservation::new(
                "git.status",
                "git status --porcelain",
                "git status unavailable; dirty-tree state unknown",
            )
            .error_category("probe_unavailable"),
        ),
    }

    // Agent Mail is a chronically-slow shared singleton; do not block the
    // collector on it (AGENTS.md: proceed without agent-mail on failure).
    input.collector_observations.push(
        RchAdmissionCollectorObservation::new(
            "agent_mail",
            "skipped",
            "agent-mail probe skipped (bounded best-effort; shared singleton, chronic timeouts)",
        )
        .error_category("probe_skipped"),
    );

    input
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
            "cargo check -p mux --target x86_64-pc-windows-gnu --target-dir /tmp/target-rch --all-targets",
            [("CARGO_BUILD_JOBS", "2")],
            Some(2),
        );

        assert_eq!(analysis.classification, "cargo_check");
        assert_eq!(
            analysis.target_triple.as_deref(),
            Some("x86_64-pc-windows-gnu")
        );
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
        assert!(
            analysis
                .explanation
                .contains("target_triple=x86_64-pc-windows-gnu")
        );
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
            vec![
                RchAdmissionReasonCode::LocalEnoSpace,
                RchAdmissionReasonCode::TelemetryGap,
            ]
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
        let citation = report.citations.first();
        assert!(citation.is_some(), "expected local disk citation");
        let Some(citation) = citation else {
            return;
        };
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
    fn installed_rch_pressure_reason_strings_normalize_to_stable_codes() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.installed_pressure_reason_strings",
            intercepted_command(),
        )
        .with_collector_observation(
            RchAdmissionCollectorObservation::new(
                "rch.status.worker_pressure",
                "RCH_NO_SELF_HEALING=1 rch --json status --workers --jobs",
                "installed RCH reported current worker pressure codes",
            )
            .error_category(
                "worker=null; no_workers_passed_health; \
                 pressure_reason_code=disk_free_below_critical_gb; \
                 pressure_reason_code=disk_ratio_below_critical; \
                 pressure_reason_code=disk_critical_without_fresh_telemetry; \
                 pressure_reason_code=disk_metrics_unavailable",
            ),
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
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::TelemetryGap)
        );
    }

    #[test]
    fn missing_worker_target_stdlib_normalizes_to_toolchain_reason() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.missing_worker_target",
            RchAdmissionCommandDiagnostic::new(
                "rch exec -- cargo check -p window --lib --target x86_64-pc-windows-gnu",
            )
            .normalized("cargo check -p window --lib --target x86_64-pc-windows-gnu")
            .classification("cargo_check")
            .would_intercept(true)
            .target_dir("/tmp/ft-window-target"),
        )
        .with_collector_observation(
            RchAdmissionCollectorObservation::new(
                "rch.remote.cargo_stderr",
                "rch exec -- cargo check -p window --lib --target x86_64-pc-windows-gnu",
                "remote Cargo failed before crate checking because the worker is missing the requested Rust target",
            )
            .error_category(
                "error[E0463]: can't find crate for `core`; note: the x86_64-pc-windows-gnu target may not be installed; help: rustup target add x86_64-pc-windows-gnu",
            ),
        );

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::WorkerToolchainMissingTarget)
        );
        assert!(
            report
                .recommendations
                .iter()
                .find(|recommendation| {
                    recommendation.reason_code
                        == RchAdmissionReasonCode::WorkerToolchainMissingTarget
                })
                .is_some_and(|recommendation| recommendation.operator_approval_required)
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
            selected_worker: None,
            worker_slots_available: Some(0),
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
    fn rch_queue_with_no_blockers_is_runnable_without_unknown_reason() {
        let analysis = analyze_rch_admission_cargo_command(
            "cargo test -j 1 -p frankenterm-core rch_admission --lib",
            std::iter::empty::<(&str, &str)>(),
            Some(4),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.runnable_queue",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analysis)
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("healthy".to_string()),
            active_project_exclusion: false,
            active_builds: 0,
            queued_builds: 0,
            selected_worker: Some("vmi-healthy".to_string()),
            worker_slots_available: Some(2),
            workers_healthy: Some(1),
            workers_total: Some(8),
        });

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Runnable);
        assert!(report.reason_codes.is_empty());
        assert!(report.recommendations.is_empty());
        assert_eq!(report.cargo_jobs, Some(1));
        assert_eq!(report.estimated_slots, Some(1));
    }

    #[test]
    fn partial_capacity_selected_worker_does_not_inherit_other_worker_rejections() {
        let analysis = analyze_rch_admission_cargo_command(
            "cargo test --jobs=1 -p frankenterm-core-rch-types --lib",
            std::iter::empty::<(&str, &str)>(),
            Some(4),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.partial_capacity_selected_worker",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analysis)
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("degraded".to_string()),
            active_project_exclusion: false,
            active_builds: 1,
            queued_builds: 0,
            selected_worker: Some("vmi1264463".to_string()),
            worker_slots_available: Some(2),
            workers_healthy: Some(1),
            workers_total: Some(8),
        })
        .with_worker_rejection(RchAdmissionWorkerRejection::new(
            Some("vmi1149989"),
            RchAdmissionReasonCode::CriticalPressure,
            "disk_free_below_critical_gb",
            RchAdmissionSeverity::Critical,
        ))
        .with_worker_rejection(RchAdmissionWorkerRejection::new(
            Some("vmi1167313"),
            RchAdmissionReasonCode::CriticalPressure,
            "disk_ratio_below_critical",
            RchAdmissionSeverity::Critical,
        ));

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Runnable);
        assert!(
            !report
                .reason_codes
                .contains(&RchAdmissionReasonCode::InsufficientSlots)
        );
        assert!(
            !report
                .reason_codes
                .contains(&RchAdmissionReasonCode::CriticalPressure)
        );
        assert_eq!(
            report.rch_queue.selected_worker.as_deref(),
            Some("vmi1264463")
        );
        assert_eq!(report.rch_queue.worker_slots_available, Some(2));
    }

    #[test]
    fn insufficient_slots_uses_available_slots_when_reported() {
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.available_slots",
            intercepted_command(),
        )
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("degraded".to_string()),
            active_project_exclusion: false,
            active_builds: 1,
            queued_builds: 0,
            selected_worker: None,
            worker_slots_available: Some(2),
            workers_healthy: Some(1),
            workers_total: Some(8),
        })
        .with_estimated_slots(3);

        let report = build_rch_admission_report(&input);

        assert_eq!(report.proof_status, RchAdmissionProofStatus::Blocked);
        assert!(
            report
                .reason_codes
                .contains(&RchAdmissionReasonCode::InsufficientSlots)
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

    #[test]
    fn preflight_admits_remote_required_package_command_with_timestamp() {
        let proof_command = analyze_rch_admission_proof_command(
            "RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-7h5da94-cod8-target cargo test -p frankenterm-core --lib rch_admission",
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.preflight_admitted",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analyze_rch_admission_cargo_command(
            &proof_command.raw,
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        ))
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("healthy".to_string()),
            active_project_exclusion: false,
            active_builds: 0,
            queued_builds: 0,
            selected_worker: Some("vmi-healthy".to_string()),
            worker_slots_available: Some(1),
            workers_healthy: Some(1),
            workers_total: Some(8),
        });

        let preflight = build_rch_admission_preflight_report(&input, proof_command);

        assert_eq!(preflight.verdict, RchAdmissionPreflightVerdict::Admitted);
        assert_eq!(preflight.rch_admission_state, "admitted");
        assert_eq!(preflight.predicted_at_ms, 1_779_013_898_000);
        assert_eq!(preflight.selected_worker.as_deref(), Some("vmi-healthy"));
        assert_eq!(preflight.proof_command.proof_kind, Some(ProofKind::Test));
        assert!(matches!(
            preflight.proof_command.proof_scope,
            Some(ProofScope::Package { ref package }) if package == "frankenterm-core"
        ));
        assert!(preflight.proof_command.risks.is_empty());
        assert!(preflight.summary.contains("admitted: 1 worker"));
        assert!(preflight.summary.contains("package-scoped"));
    }

    #[test]
    fn preflight_defers_no_worker_remote_command_as_queueable_intent() -> Result<(), String> {
        let proof_command = analyze_rch_admission_proof_command(
            "RCH_REQUIRE_REMOTE=1 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-7h5da94-cod8-target cargo test -p frankenterm-core --lib rch_admission",
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.preflight_deferred",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analyze_rch_admission_cargo_command(
            &proof_command.raw,
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        ))
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("blocked".to_string()),
            active_project_exclusion: false,
            active_builds: 0,
            queued_builds: 0,
            selected_worker: None,
            worker_slots_available: Some(0),
            workers_healthy: Some(0),
            workers_total: Some(8),
        });

        let preflight = build_rch_admission_preflight_report(&input, proof_command);

        assert_eq!(preflight.verdict, RchAdmissionPreflightVerdict::Deferred);
        assert_eq!(preflight.rch_admission_state, "wait_rch");
        assert!(preflight.queue_intent_recommended);
        assert!(
            preflight
                .reason_codes
                .contains(&RchAdmissionReasonCode::NoAdmissibleWorkers)
        );
        assert!(preflight.summary.contains("queue intent"));

        let Some(entry) = preflight.to_deferred_proof_intent_queue_entry(
            "sha256:preflight-tree",
            None,
            Some("ft-7h5da.9.4".to_string()),
            None,
            ProofRedactionPolicy::Standard,
            1_779_013_898_000,
        ) else {
            return Err("deferred preflight did not yield a queueable proof intent".to_string());
        };
        entry.validate().map_err(|error| error.to_string())?;
        assert_eq!(entry.rch_admission_state, "wait_rch");
        assert_eq!(
            entry.target_dir.as_deref(),
            Some("/tmp/ft-7h5da94-cod8-target")
        );
        assert_eq!(entry.intent.kind, ProofKind::Test);
        assert_eq!(entry.intent.bead_id.as_deref(), Some("ft-7h5da.9.4"));
        assert!(entry.intent.required_remote);
        assert!(
            entry
                .command_env
                .iter()
                .any(|env| { env.name == "RCH_REQUIRE_REMOTE" && env.value == "1" })
        );
        assert_eq!(entry.command_argv.first().map(String::as_str), Some("rch"));
        Ok(())
    }

    #[test]
    fn preflight_invalidates_local_cargo_and_shared_target_dir() {
        let proof_command = analyze_rch_admission_proof_command(
            "cargo test -p frankenterm-core --target-dir target --lib rch_admission",
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.preflight_invalid",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analyze_rch_admission_cargo_command(
            &proof_command.raw,
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        ))
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("healthy".to_string()),
            active_project_exclusion: false,
            active_builds: 0,
            queued_builds: 0,
            selected_worker: Some("vmi-healthy".to_string()),
            worker_slots_available: Some(1),
            workers_healthy: Some(1),
            workers_total: Some(8),
        });

        let preflight = build_rch_admission_preflight_report(&input, proof_command);

        assert_eq!(preflight.verdict, RchAdmissionPreflightVerdict::Invalid);
        assert_eq!(preflight.rch_admission_state, "invalid");
        assert!(!preflight.queue_intent_recommended);
        assert!(
            preflight
                .proof_command
                .risks
                .contains(&RchAdmissionProofCommandRisk::MissingRchExecWrapper)
        );
        assert!(
            preflight
                .proof_command
                .risks
                .contains(&RchAdmissionProofCommandRisk::SharedWorkspaceTargetDir)
        );
    }

    #[test]
    fn preflight_blocks_explicit_local_fallback_request() {
        let proof_command = analyze_rch_admission_proof_command(
            "RCH_REQUIRE_REMOTE=0 RCH_NO_SELF_HEALING=1 rch --no-self-healing exec -- env CARGO_TARGET_DIR=/tmp/ft-7h5da94-cod8-target cargo test -p frankenterm-core --lib rch_admission",
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        );
        let input = RchAdmissionCollectorInput::new(
            1_779_013_898_000,
            "test.preflight_blocked",
            RchAdmissionCommandDiagnostic::new("placeholder"),
        )
        .with_cargo_command_analysis(&analyze_rch_admission_cargo_command(
            &proof_command.raw,
            std::iter::empty::<(&str, &str)>(),
            Some(1),
        ))
        .with_rch_queue(RchAdmissionQueueDiagnostic {
            posture: Some("healthy".to_string()),
            active_project_exclusion: false,
            active_builds: 0,
            queued_builds: 0,
            selected_worker: Some("vmi-healthy".to_string()),
            worker_slots_available: Some(1),
            workers_healthy: Some(1),
            workers_total: Some(8),
        });

        let preflight = build_rch_admission_preflight_report(&input, proof_command);

        assert_eq!(preflight.verdict, RchAdmissionPreflightVerdict::Blocked);
        assert_eq!(preflight.rch_admission_state, "blocked_command");
        assert!(!preflight.queue_intent_recommended);
        assert!(
            preflight
                .proof_command
                .risks
                .contains(&RchAdmissionProofCommandRisk::LocalFallbackRequested)
        );
        assert!(preflight.summary.contains("permits local fallback"));
    }

    // ── Live collector tests (ft-69gwh.9) ──

    #[test]
    fn parse_df_available_handles_macos_and_linux_headers() {
        let macos = "Filesystem   1024-blocks       Used Available Capacity iused      ifree %iused  Mounted on\n/dev/disk3s5  1948455240 1192741220 719410028    63% 6577842 7194100280    0%   /System/Volumes/Data\n";
        assert_eq!(
            parse_df_available_bytes(macos),
            Some(719_410_028_u64 * 1024)
        );
        let linux = "Filesystem     1K-blocks      Used Available Use% Mounted on\n/dev/sda1      102400000  20000000  82400000  20% /\n";
        assert_eq!(parse_df_available_bytes(linux), Some(82_400_000_u64 * 1024));
        assert_eq!(parse_df_available_bytes("garbage"), None);
        assert_eq!(parse_df_available_bytes(""), None);
    }

    #[test]
    fn parse_git_porcelain_categorizes_statuses() {
        let porcelain = " M src/lib.rs\n?? scratch/new.rs\nD  removed.rs\nA  added.rs\n";
        let dirty = parse_git_porcelain_dirty(porcelain);
        assert_eq!(dirty.len(), 4);
        assert_eq!(dirty[0].path, "src/lib.rs");
        assert_eq!(dirty[0].category, "modified");
        assert_eq!(dirty[1].category, "untracked");
        assert_eq!(dirty[2].category, "deleted");
        assert_eq!(dirty[3].category, "added");
        assert!(parse_git_porcelain_dirty("").is_empty());
    }

    #[test]
    fn parse_rch_status_queue_extracts_posture_and_counts() {
        let json = r#"{"data":{"posture":"degraded","daemon":{"daemon":{"workers_total":12,"workers_healthy":7,"slots_available":48}}}}"#;
        let queue = parse_rch_status_queue(json);
        assert_eq!(queue.posture.as_deref(), Some("degraded"));
        assert_eq!(queue.workers_total, Some(12));
        assert_eq!(queue.workers_healthy, Some(7));
        assert_eq!(queue.worker_slots_available, Some(48));
        // Malformed JSON degrades to unknown(), never panics.
        assert_eq!(
            parse_rch_status_queue("not json"),
            RchAdmissionQueueDiagnostic::unknown()
        );
    }

    #[test]
    fn parse_rch_status_worker_rejections_flags_unhealthy() {
        let json = r#"{"data":{"daemon":{"workers":[
            {"id":"vmi1","status":"healthy"},
            {"id":"vmi2","status":"unhealthy"},
            {"id":"vmi3","status":"offline"}
        ]}}}"#;
        let rejections = parse_rch_status_worker_rejections(json);
        assert_eq!(rejections.len(), 2);
        assert_eq!(rejections[0].worker.as_deref(), Some("vmi2"));
        assert_eq!(
            rejections[0].reason_code,
            RchAdmissionReasonCode::NoAdmissibleWorkers
        );
        assert!(parse_rch_status_worker_rejections("{}").is_empty());
    }

    #[test]
    fn collect_live_input_populates_source_and_is_probe_safe() {
        let dir = std::env::temp_dir();
        let input = collect_live_rch_admission_input(
            &dir,
            "cargo test -p frankenterm-core --lib",
            1_700_000_000_000,
        );
        assert_eq!(input.source, "ft doctor --rch-admission");
        // A real report can be built from the live input (the production gap).
        let report = build_rch_admission_report(&input);
        assert!(!report.source.is_empty());
        // Agent-mail is always recorded as a bounded skip rather than hanging.
        assert!(
            input
                .collector_observations
                .iter()
                .any(|obs| obs.source_id == "agent_mail")
        );
    }
}
