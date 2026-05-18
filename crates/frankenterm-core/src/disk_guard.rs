//! Read-only disk-guard collector normalization.
//!
//! The disk guard is a preflight contract for ENOSPC-sensitive agent work. This
//! module never deletes files, cleans target directories, repairs services,
//! restarts Agent Mail or RCH, mutates workers, cancels builds, or runs Cargo.
//! It normalizes already-collected write/status facts plus bounded filesystem
//! samples into the static `ft.disk_guard.v1` artifact shape.

#![allow(clippy::module_name_repetitions, clippy::too_many_arguments)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[cfg(unix)]
use nix::sys::statvfs::statvfs;
use serde::{Deserialize, Serialize};

pub const DISK_GUARD_CONTRACT_ID: &str = "ft.disk_guard.v1";
pub const DISK_GUARD_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardDecision {
    Proceed,
    StaticOnly,
    ExternalScratchOnly,
    Block,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardProbeId {
    SystemDataVolume,
    PrivateTmp,
    RepoWriteProbe,
    BeadsDbWriteability,
    BeadsJsonlExportability,
    AgentMailDbOpen,
    RchCacheWriteability,
    ExternalScratch,
}

impl DiskGuardProbeId {
    #[must_use]
    pub const fn required() -> [Self; 8] {
        [
            Self::SystemDataVolume,
            Self::PrivateTmp,
            Self::RepoWriteProbe,
            Self::BeadsDbWriteability,
            Self::BeadsJsonlExportability,
            Self::AgentMailDbOpen,
            Self::RchCacheWriteability,
            Self::ExternalScratch,
        ]
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SystemDataVolume => "system_data_volume",
            Self::PrivateTmp => "private_tmp",
            Self::RepoWriteProbe => "repo_write_probe",
            Self::BeadsDbWriteability => "beads_db_writeability",
            Self::BeadsJsonlExportability => "beads_jsonl_exportability",
            Self::AgentMailDbOpen => "agent_mail_db_open",
            Self::RchCacheWriteability => "rch_cache_writeability",
            Self::ExternalScratch => "external_scratch",
        }
    }

    #[must_use]
    pub const fn reason_prefix(self) -> &'static str {
        match self {
            Self::SystemDataVolume => "disk.system_data_volume",
            Self::PrivateTmp => "disk.private_tmp",
            Self::RepoWriteProbe => "write_probe.repo",
            Self::BeadsDbWriteability => "beads.db",
            Self::BeadsJsonlExportability => "beads.jsonl",
            Self::AgentMailDbOpen => "agent_mail.db",
            Self::RchCacheWriteability => "rch.cache",
            Self::ExternalScratch => "external_scratch",
        }
    }

    #[must_use]
    pub const fn is_local_space_probe(self) -> bool {
        matches!(self, Self::SystemDataVolume | Self::PrivateTmp)
    }

    #[must_use]
    pub const fn is_write_precondition(self) -> bool {
        matches!(
            self,
            Self::RepoWriteProbe
                | Self::BeadsDbWriteability
                | Self::BeadsJsonlExportability
                | Self::RchCacheWriteability
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardProbeState {
    Pass,
    Warn,
    Fail,
    Blocked,
    Degraded,
    Unavailable,
    Skipped,
    Unknown,
}

impl DiskGuardProbeState {
    #[must_use]
    pub const fn is_hard_failure(self) -> bool {
        matches!(self, Self::Fail | Self::Blocked)
    }

    #[must_use]
    pub const fn is_evidence_gap(self) -> bool {
        matches!(self, Self::Skipped | Self::Unknown | Self::Unavailable)
    }

    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Warn | Self::Degraded | Self::Unavailable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardSeverity {
    Green,
    Yellow,
    Red,
    Black,
    Unknown,
}

impl DiskGuardSeverity {
    #[must_use]
    pub const fn is_hard_failure(self) -> bool {
        matches!(self, Self::Black)
    }

    #[must_use]
    pub const fn is_degraded(self) -> bool {
        matches!(self, Self::Yellow | Self::Red | Self::Unknown)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardPerformedAction {
    StatFilesystem,
    BoundedWriteProbe,
    ReadStatus,
    ReadRetainedArtifact,
    ReadCleanupInventory,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardForbiddenAction {
    DeleteFile,
    DeleteDirectory,
    CleanTarget,
    RepairAgentMail,
    RestartAgentMail,
    RestartRch,
    MutateWorkerMirror,
    CancelBuild,
    RunLocalCargoProof,
    DestructiveGit,
}

impl DiskGuardForbiddenAction {
    #[must_use]
    pub fn stable_all() -> Vec<Self> {
        vec![
            Self::DeleteFile,
            Self::DeleteDirectory,
            Self::CleanTarget,
            Self::RepairAgentMail,
            Self::RestartAgentMail,
            Self::RestartRch,
            Self::MutateWorkerMirror,
            Self::CancelBuild,
            Self::RunLocalCargoProof,
            Self::DestructiveGit,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskGuardSideEffectPolicy {
    pub read_only: bool,
    pub cleanup_requires_operator_approval: bool,
    pub performed_actions: Vec<DiskGuardPerformedAction>,
    pub forbidden_actions: Vec<DiskGuardForbiddenAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardCleanupKind {
    CargoTarget,
    CargoHomeRegistry,
    RchCargoHome,
    RchTarget,
    ProjectCache,
    TempProofArtifact,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardCleanupRiskTier {
    Low,
    Medium,
    High,
    Protected,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskGuardLiveUseState {
    NotReferenced,
    Referenced,
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskGuardCleanupCandidateInput {
    pub path: String,
    pub project: String,
    pub kind: DiskGuardCleanupKind,
    pub size_bytes: u64,
    pub modified_at_ms: Option<u64>,
    pub has_cachedir_tag: Option<bool>,
    pub lsof_reference_count: Option<u32>,
    pub process_reference_count: Option<u32>,
    pub artifact_paths: Vec<String>,
}

impl DiskGuardCleanupCandidateInput {
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        project: impl Into<String>,
        kind: DiskGuardCleanupKind,
        size_bytes: u64,
    ) -> Self {
        Self {
            path: nonempty_string(path, "unknown"),
            project: nonempty_string(project, "unknown"),
            kind,
            size_bytes,
            modified_at_ms: None,
            has_cachedir_tag: None,
            lsof_reference_count: None,
            process_reference_count: None,
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_modified_at_ms(mut self, modified_at_ms: u64) -> Self {
        self.modified_at_ms = Some(modified_at_ms);
        self
    }

    #[must_use]
    pub fn with_cachedir_tag(mut self, has_cachedir_tag: bool) -> Self {
        self.has_cachedir_tag = Some(has_cachedir_tag);
        self
    }

    #[must_use]
    pub fn with_lsof_reference_count(mut self, reference_count: u32) -> Self {
        self.lsof_reference_count = Some(reference_count);
        self
    }

    #[must_use]
    pub fn with_process_reference_count(mut self, reference_count: u32) -> Self {
        self.process_reference_count = Some(reference_count);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, artifact_path: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.artifact_paths, artifact_path);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskGuardCleanupCandidate {
    pub path: String,
    pub project: String,
    pub kind: DiskGuardCleanupKind,
    pub risk_tier: DiskGuardCleanupRiskTier,
    pub operator_approval_required: bool,
    pub automatic_cleanup_allowed: bool,
    pub size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_cachedir_tag: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lsof_reference_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_reference_count: Option<u32>,
    pub live_use: DiskGuardLiveUseState,
    pub reason_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
    pub next_safe_action: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskGuardProbe {
    pub probe_id: DiskGuardProbeId,
    pub source: String,
    pub sampled_at_ms: u64,
    pub state: DiskGuardProbeState,
    pub severity: DiskGuardSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
    pub reason_codes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_paths: Vec<String>,
    pub next_safe_action: String,
}

impl DiskGuardProbe {
    #[must_use]
    pub fn new(
        probe_id: DiskGuardProbeId,
        source: impl Into<String>,
        sampled_at_ms: u64,
        state: DiskGuardProbeState,
        severity: DiskGuardSeverity,
        reason_code: impl Into<String>,
        next_safe_action: impl Into<String>,
    ) -> Self {
        Self {
            probe_id,
            source: nonempty_string(source, "unknown"),
            sampled_at_ms,
            state,
            severity,
            path: None,
            free_bytes: None,
            total_bytes: None,
            threshold_bytes: None,
            probe_result: None,
            error_category: None,
            reason_codes: vec![nonempty_reason_code(reason_code, probe_id)],
            artifact_paths: Vec::new(),
            next_safe_action: nonempty_string(
                next_safe_action,
                default_next_safe_action(probe_id, state),
            ),
        }
    }

    #[must_use]
    pub fn not_collected(probe_id: DiskGuardProbeId, sampled_at_ms: u64) -> Self {
        Self::new(
            probe_id,
            "not_collected",
            sampled_at_ms,
            DiskGuardProbeState::Skipped,
            DiskGuardSeverity::Unknown,
            format!("source.{}.not_collected", probe_id.as_str()),
            "Collect read-only probe evidence before relying on this guard.",
        )
        .with_probe_result("required probe was not collected")
    }

    #[must_use]
    pub fn filesystem_sample(
        probe_id: DiskGuardProbeId,
        source: impl Into<String>,
        sampled_at_ms: u64,
        path: impl Into<String>,
        free_bytes: u64,
        total_bytes: u64,
        threshold_bytes: u64,
    ) -> Self {
        let path = nonempty_string(path, "unknown");
        let (state, severity, suffix, result, next_safe_action) =
            classify_filesystem_sample(probe_id, free_bytes, total_bytes, threshold_bytes);

        Self::new(
            probe_id,
            source,
            sampled_at_ms,
            state,
            severity,
            format!("{}.{}", probe_id.reason_prefix(), suffix),
            next_safe_action,
        )
        .with_path(path)
        .with_space(free_bytes, total_bytes, threshold_bytes)
        .with_probe_result(result)
    }

    #[must_use]
    pub fn status_probe(
        probe_id: DiskGuardProbeId,
        source: impl Into<String>,
        sampled_at_ms: u64,
        state: DiskGuardProbeState,
        severity: DiskGuardSeverity,
        probe_result: impl Into<String>,
        reason_code: impl Into<String>,
        next_safe_action: impl Into<String>,
    ) -> Self {
        Self::new(
            probe_id,
            source,
            sampled_at_ms,
            state,
            severity,
            reason_code,
            next_safe_action,
        )
        .with_probe_result(probe_result)
    }

    #[must_use]
    pub fn unavailable(
        probe_id: DiskGuardProbeId,
        source: impl Into<String>,
        sampled_at_ms: u64,
        path: impl Into<String>,
        error_category: impl Into<String>,
    ) -> Self {
        Self::new(
            probe_id,
            source,
            sampled_at_ms,
            DiskGuardProbeState::Unavailable,
            DiskGuardSeverity::Unknown,
            format!("source.{}.unavailable", probe_id.as_str()),
            "Retain the failure artifact and do not treat this probe as healthy.",
        )
        .with_path(path)
        .with_probe_result("probe source unavailable")
        .with_error_category(error_category)
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(nonempty_string(path, "unknown"));
        self
    }

    #[must_use]
    pub fn with_space(mut self, free_bytes: u64, total_bytes: u64, threshold_bytes: u64) -> Self {
        self.free_bytes = Some(free_bytes);
        self.total_bytes = Some(total_bytes);
        self.threshold_bytes = Some(threshold_bytes);
        self
    }

    #[must_use]
    pub fn with_probe_result(mut self, probe_result: impl Into<String>) -> Self {
        self.probe_result = Some(nonempty_string(probe_result, "unknown"));
        self
    }

    #[must_use]
    pub fn with_error_category(mut self, error_category: impl Into<String>) -> Self {
        self.error_category = Some(nonempty_string(error_category, "unknown"));
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_nonempty_unique(
            &mut self.reason_codes,
            nonempty_reason_code(reason_code, self.probe_id),
        );
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, artifact_path: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.artifact_paths, artifact_path);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskGuardCollectorInput {
    pub generated_at_ms: u64,
    pub guard_id: String,
    pub workspace_root: String,
    pub probes: Vec<DiskGuardProbe>,
    pub cleanup_candidates: Vec<DiskGuardCleanupCandidateInput>,
    pub artifact_paths: Vec<String>,
}

impl DiskGuardCollectorInput {
    #[must_use]
    pub fn new(
        generated_at_ms: u64,
        guard_id: impl Into<String>,
        workspace_root: impl Into<String>,
    ) -> Self {
        Self {
            generated_at_ms,
            guard_id: nonempty_string(guard_id, "disk_guard.unknown"),
            workspace_root: nonempty_string(workspace_root, "unknown"),
            probes: Vec::new(),
            cleanup_candidates: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_probe(mut self, probe: DiskGuardProbe) -> Self {
        self.probes.push(probe);
        self
    }

    #[must_use]
    pub fn with_cleanup_candidate(mut self, candidate: DiskGuardCleanupCandidateInput) -> Self {
        self.cleanup_candidates.push(candidate);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, artifact_path: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.artifact_paths, artifact_path);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskGuardReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub guard_id: String,
    pub workspace_root: String,
    pub decision: DiskGuardDecision,
    pub side_effect_policy: DiskGuardSideEffectPolicy,
    pub probes: Vec<DiskGuardProbe>,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_candidates: Vec<DiskGuardCleanupCandidate>,
}

#[must_use]
pub fn collect_filesystem_probe(
    probe_id: DiskGuardProbeId,
    path: impl AsRef<Path>,
    threshold_bytes: u64,
    sampled_at_ms: u64,
    source: impl Into<String>,
) -> DiskGuardProbe {
    let path = path.as_ref();
    let path_label = path.display().to_string();
    match read_filesystem_space(path) {
        Ok((free_bytes, total_bytes)) => DiskGuardProbe::filesystem_sample(
            probe_id,
            source,
            sampled_at_ms,
            path_label,
            free_bytes,
            total_bytes,
            threshold_bytes,
        ),
        Err(error) => DiskGuardProbe::unavailable(
            probe_id,
            source,
            sampled_at_ms,
            path_label,
            format!("statvfs.{error}"),
        ),
    }
}

#[must_use]
pub fn build_disk_guard_report(input: &DiskGuardCollectorInput) -> DiskGuardReport {
    let mut probe_by_id = BTreeMap::new();
    let mut reason_codes = BTreeSet::new();
    let mut artifact_paths = input.artifact_paths.clone();
    let mut duplicate_probe = false;
    let cleanup_candidates = input
        .cleanup_candidates
        .iter()
        .map(|candidate| classify_cleanup_candidate(candidate, input.generated_at_ms))
        .collect::<Vec<_>>();

    for candidate in &cleanup_candidates {
        for reason_code in &candidate.reason_codes {
            reason_codes.insert(reason_code.clone());
        }
        for artifact_path in &candidate.artifact_paths {
            push_nonempty_unique(&mut artifact_paths, artifact_path.clone());
        }
    }

    for probe in &input.probes {
        if probe_by_id.insert(probe.probe_id, probe.clone()).is_some() {
            duplicate_probe = true;
            reason_codes.insert(format!("source.{}.duplicate", probe.probe_id.as_str()));
        }
    }

    let mut missing_required = false;
    let mut probes = Vec::new();
    for probe_id in DiskGuardProbeId::required() {
        let probe = if let Some(probe) = probe_by_id.remove(&probe_id) {
            probe
        } else {
            missing_required = true;
            DiskGuardProbe::not_collected(probe_id, input.generated_at_ms)
        };

        for reason_code in &probe.reason_codes {
            reason_codes.insert(reason_code.clone());
        }
        for artifact_path in &probe.artifact_paths {
            push_nonempty_unique(&mut artifact_paths, artifact_path.clone());
        }
        probes.push(probe);
    }

    if missing_required {
        reason_codes.insert("fail_closed.missing_required_probe".to_string());
    }
    if duplicate_probe {
        reason_codes.insert("fail_closed.duplicate_probe".to_string());
    }
    reason_codes.insert("policy.cleanup_requires_operator_approval".to_string());

    let decision = classify_decision(&probes, missing_required || duplicate_probe);
    match decision {
        DiskGuardDecision::Proceed => {
            reason_codes.insert("disk.guard.healthy".to_string());
        }
        DiskGuardDecision::StaticOnly => {
            reason_codes.insert("disk.guard.static_only".to_string());
        }
        DiskGuardDecision::ExternalScratchOnly => {
            reason_codes.insert("external_scratch.only".to_string());
        }
        DiskGuardDecision::Block => {
            reason_codes.insert("disk.guard.blocked".to_string());
        }
        DiskGuardDecision::Unknown => {
            reason_codes.insert("disk.guard.unknown".to_string());
        }
    }

    DiskGuardReport {
        schema_version: DISK_GUARD_SCHEMA_VERSION,
        contract_id: DISK_GUARD_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        guard_id: input.guard_id.clone(),
        workspace_root: input.workspace_root.clone(),
        decision,
        side_effect_policy: side_effect_policy_for(&probes, !cleanup_candidates.is_empty()),
        probes,
        reason_codes: reason_codes.into_iter().collect(),
        artifact_paths,
        cleanup_candidates,
    }
}

fn classify_decision(probes: &[DiskGuardProbe], required_evidence_gap: bool) -> DiskGuardDecision {
    let write_precondition_failed = probes.iter().any(|probe| {
        probe.probe_id.is_write_precondition()
            && (probe.state.is_hard_failure() || probe.severity.is_hard_failure())
    });
    if write_precondition_failed {
        return DiskGuardDecision::Block;
    }

    let local_space_failed = probes.iter().any(|probe| {
        probe.probe_id.is_local_space_probe()
            && (probe.state.is_hard_failure() || probe.severity.is_hard_failure())
    });
    let external_scratch_available = probes.iter().any(|probe| {
        probe.probe_id == DiskGuardProbeId::ExternalScratch
            && probe.state == DiskGuardProbeState::Pass
            && matches!(
                probe.severity,
                DiskGuardSeverity::Green | DiskGuardSeverity::Yellow
            )
    });
    if local_space_failed {
        return if external_scratch_available {
            DiskGuardDecision::ExternalScratchOnly
        } else {
            DiskGuardDecision::Block
        };
    }

    if required_evidence_gap {
        return DiskGuardDecision::Block;
    }

    let any_evidence_gap = probes
        .iter()
        .any(|probe| probe.state.is_evidence_gap() || probe.severity == DiskGuardSeverity::Unknown);
    if any_evidence_gap {
        return DiskGuardDecision::Unknown;
    }

    let degraded = probes
        .iter()
        .any(|probe| probe.state.is_degraded() || probe.severity.is_degraded());
    if degraded {
        return DiskGuardDecision::StaticOnly;
    }

    DiskGuardDecision::Proceed
}

fn side_effect_policy_for(
    probes: &[DiskGuardProbe],
    cleanup_inventory_collected: bool,
) -> DiskGuardSideEffectPolicy {
    let mut performed_actions = BTreeSet::new();
    for probe in probes {
        if probe.source == "not_collected" {
            performed_actions.insert(DiskGuardPerformedAction::NotCollected);
        }
        if probe.free_bytes.is_some()
            || probe.total_bytes.is_some()
            || probe.threshold_bytes.is_some()
        {
            performed_actions.insert(DiskGuardPerformedAction::StatFilesystem);
        }
        if probe.probe_id.is_write_precondition() {
            performed_actions.insert(DiskGuardPerformedAction::BoundedWriteProbe);
        }
        if matches!(
            probe.probe_id,
            DiskGuardProbeId::BeadsDbWriteability
                | DiskGuardProbeId::BeadsJsonlExportability
                | DiskGuardProbeId::AgentMailDbOpen
                | DiskGuardProbeId::RchCacheWriteability
        ) {
            performed_actions.insert(DiskGuardPerformedAction::ReadStatus);
        }
        if !probe.artifact_paths.is_empty() {
            performed_actions.insert(DiskGuardPerformedAction::ReadRetainedArtifact);
        }
    }

    if cleanup_inventory_collected {
        performed_actions.insert(DiskGuardPerformedAction::ReadCleanupInventory);
    }

    if performed_actions.is_empty() {
        performed_actions.insert(DiskGuardPerformedAction::NotCollected);
    }

    DiskGuardSideEffectPolicy {
        read_only: true,
        cleanup_requires_operator_approval: true,
        performed_actions: performed_actions.into_iter().collect(),
        forbidden_actions: DiskGuardForbiddenAction::stable_all(),
    }
}

fn classify_filesystem_sample(
    probe_id: DiskGuardProbeId,
    free_bytes: u64,
    total_bytes: u64,
    threshold_bytes: u64,
) -> (
    DiskGuardProbeState,
    DiskGuardSeverity,
    &'static str,
    &'static str,
    &'static str,
) {
    if total_bytes == 0 {
        return (
            DiskGuardProbeState::Unavailable,
            DiskGuardSeverity::Unknown,
            "unavailable",
            "filesystem sample unavailable",
            "Retain the failure artifact and do not treat this probe as healthy.",
        );
    }

    if threshold_bytes == 0 || free_bytes >= threshold_bytes {
        return (
            DiskGuardProbeState::Pass,
            DiskGuardSeverity::Green,
            if probe_id == DiskGuardProbeId::ExternalScratch {
                "available"
            } else {
                "healthy"
            },
            "free space is above configured floor",
            default_next_safe_action(probe_id, DiskGuardProbeState::Pass),
        );
    }

    let warning_floor = (threshold_bytes / 2).max(1);
    if free_bytes >= warning_floor {
        return (
            DiskGuardProbeState::Warn,
            DiskGuardSeverity::Yellow,
            "below_warning_floor",
            "free space is below preferred floor",
            "Keep work read-mostly until the disk guard returns to green.",
        );
    }

    (
        DiskGuardProbeState::Fail,
        DiskGuardSeverity::Black,
        "below_floor",
        "below recovery helper floor",
        default_next_safe_action(probe_id, DiskGuardProbeState::Fail),
    )
}

#[cfg(unix)]
#[allow(clippy::useless_conversion)]
fn read_filesystem_space(path: &Path) -> Result<(u64, u64), String> {
    let vfs = statvfs(path).map_err(|error| error.to_string())?;
    let block_size = u64::from(vfs.fragment_size().max(1));
    let total_blocks = u64::from(vfs.blocks());
    let available_blocks = u64::from(vfs.blocks_available());
    let total_bytes = total_blocks.saturating_mul(block_size);
    let available_bytes = available_blocks.saturating_mul(block_size);
    Ok((available_bytes.min(total_bytes), total_bytes))
}

#[cfg(not(unix))]
fn read_filesystem_space(path: &Path) -> Result<(u64, u64), String> {
    let _ = path;
    Err("unsupported_platform".to_string())
}

const CLEANUP_RECENT_MTIME_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const CLEANUP_LARGE_CANDIDATE_FLOOR_BYTES: u64 = 1024 * 1024 * 1024;

fn classify_cleanup_candidate(
    input: &DiskGuardCleanupCandidateInput,
    generated_at_ms: u64,
) -> DiskGuardCleanupCandidate {
    let live_use = cleanup_live_use(input);
    let age_ms = input
        .modified_at_ms
        .map(|modified_at_ms| generated_at_ms.saturating_sub(modified_at_ms));
    let future_mtime = input
        .modified_at_ms
        .is_some_and(|modified_at_ms| modified_at_ms > generated_at_ms);
    let mut reason_codes = BTreeSet::from([
        "cleanup_candidate.operator_approval_required".to_string(),
        "cleanup_candidate.no_automatic_deletion".to_string(),
    ]);

    match input.has_cachedir_tag {
        Some(true) => {
            reason_codes.insert("cleanup_candidate.cachedir_tag_present".to_string());
        }
        Some(false) => {
            reason_codes.insert("cleanup_candidate.cachedir_tag_absent".to_string());
        }
        None => {
            reason_codes.insert("cleanup_candidate.cachedir_tag_not_checked".to_string());
        }
    }

    match input.modified_at_ms {
        Some(_) if future_mtime => {
            reason_codes.insert("cleanup_candidate.future_mtime".to_string());
        }
        Some(_) => {
            if age_ms.unwrap_or(0) < CLEANUP_RECENT_MTIME_WINDOW_MS {
                reason_codes.insert("cleanup_candidate.recent_mtime".to_string());
            } else {
                reason_codes.insert("cleanup_candidate.stale_mtime".to_string());
            }
        }
        None => {
            reason_codes.insert("cleanup_candidate.mtime_not_checked".to_string());
        }
    }

    if input.size_bytes >= CLEANUP_LARGE_CANDIDATE_FLOOR_BYTES {
        reason_codes.insert("cleanup_candidate.large_candidate".to_string());
    } else {
        reason_codes.insert("cleanup_candidate.small_candidate".to_string());
    }

    if input.kind == DiskGuardCleanupKind::Unknown {
        reason_codes.insert("cleanup_candidate.kind_unknown".to_string());
    }

    match live_use {
        DiskGuardLiveUseState::Referenced => {
            reason_codes.insert("cleanup_candidate.live_reference".to_string());
        }
        DiskGuardLiveUseState::NotReferenced => {
            reason_codes.insert("cleanup_candidate.no_live_reference".to_string());
        }
        DiskGuardLiveUseState::NotChecked => {
            reason_codes.insert("cleanup_candidate.live_reference_not_checked".to_string());
        }
    }

    let path_is_protected = cleanup_path_is_protected(&input.path);
    if path_is_protected {
        reason_codes.insert("cleanup_candidate.protected_path".to_string());
    }

    let risk_tier = cleanup_risk_tier(input, age_ms, live_use, path_is_protected, future_mtime);

    DiskGuardCleanupCandidate {
        path: input.path.clone(),
        project: input.project.clone(),
        kind: input.kind,
        risk_tier,
        operator_approval_required: true,
        automatic_cleanup_allowed: false,
        size_bytes: input.size_bytes,
        modified_at_ms: input.modified_at_ms,
        age_ms,
        has_cachedir_tag: input.has_cachedir_tag,
        lsof_reference_count: input.lsof_reference_count,
        process_reference_count: input.process_reference_count,
        live_use,
        reason_codes: reason_codes.into_iter().collect(),
        artifact_paths: input.artifact_paths.clone(),
        next_safe_action: cleanup_next_safe_action(risk_tier).to_string(),
    }
}

fn cleanup_live_use(input: &DiskGuardCleanupCandidateInput) -> DiskGuardLiveUseState {
    let lsof_count = input.lsof_reference_count.unwrap_or(0);
    let process_count = input.process_reference_count.unwrap_or(0);
    if lsof_count > 0 || process_count > 0 {
        DiskGuardLiveUseState::Referenced
    } else if input.lsof_reference_count.is_some() || input.process_reference_count.is_some() {
        DiskGuardLiveUseState::NotReferenced
    } else {
        DiskGuardLiveUseState::NotChecked
    }
}

fn cleanup_path_is_protected(path: &str) -> bool {
    let mut previous_component = None;
    for component in Path::new(path)
        .components()
        .filter_map(|component| component.as_os_str().to_str())
    {
        if component == ".git" {
            return true;
        }
        if previous_component == Some("crates") && component == "frankenterm-core" {
            return true;
        }
        previous_component = Some(component);
    }
    false
}

fn cleanup_risk_tier(
    input: &DiskGuardCleanupCandidateInput,
    age_ms: Option<u64>,
    live_use: DiskGuardLiveUseState,
    path_is_protected: bool,
    future_mtime: bool,
) -> DiskGuardCleanupRiskTier {
    if path_is_protected {
        return DiskGuardCleanupRiskTier::Protected;
    }
    if live_use == DiskGuardLiveUseState::Referenced || future_mtime {
        return DiskGuardCleanupRiskTier::High;
    }
    if !matches!(age_ms, Some(age_ms) if age_ms >= CLEANUP_RECENT_MTIME_WINDOW_MS) {
        return DiskGuardCleanupRiskTier::High;
    }
    if input.size_bytes < CLEANUP_LARGE_CANDIDATE_FLOOR_BYTES
        || input.has_cachedir_tag != Some(true)
        || input.kind == DiskGuardCleanupKind::Unknown
        || live_use == DiskGuardLiveUseState::NotChecked
    {
        return DiskGuardCleanupRiskTier::Medium;
    }

    DiskGuardCleanupRiskTier::Low
}

fn cleanup_next_safe_action(risk_tier: DiskGuardCleanupRiskTier) -> &'static str {
    match risk_tier {
        DiskGuardCleanupRiskTier::Low => {
            "Eligible for operator review; no automatic cleanup command is emitted."
        }
        DiskGuardCleanupRiskTier::Medium => {
            "Collect more evidence before asking an operator to approve cleanup."
        }
        DiskGuardCleanupRiskTier::High => {
            "Do not delete; re-sample after live references, recent writes, or evidence gaps clear."
        }
        DiskGuardCleanupRiskTier::Protected => {
            "Do not delete; this path is protected by FrankenTerm repository policy."
        }
        DiskGuardCleanupRiskTier::Unknown => {
            "Treat as unsafe until the candidate can be reclassified."
        }
    }
}

fn default_next_safe_action(
    probe_id: DiskGuardProbeId,
    state: DiskGuardProbeState,
) -> &'static str {
    match (probe_id, state) {
        (DiskGuardProbeId::SystemDataVolume, DiskGuardProbeState::Pass)
        | (DiskGuardProbeId::PrivateTmp, DiskGuardProbeState::Pass) => {
            "Proceed within normal proof and edit policy."
        }
        (DiskGuardProbeId::SystemDataVolume, _) | (DiskGuardProbeId::PrivateTmp, _) => {
            "Use external scratch artifacts or obtain explicit operator cleanup approval before write-heavy work."
        }
        (DiskGuardProbeId::RepoWriteProbe, DiskGuardProbeState::Pass) => {
            "Repository writes are allowed within owned paths."
        }
        (DiskGuardProbeId::RepoWriteProbe, _) => {
            "Do not apply patches in the shared checkout until the write probe recovers."
        }
        (DiskGuardProbeId::BeadsDbWriteability, DiskGuardProbeState::Pass)
        | (DiskGuardProbeId::BeadsJsonlExportability, DiskGuardProbeState::Pass) => {
            "Use normal Beads coordination."
        }
        (DiskGuardProbeId::BeadsDbWriteability, _)
        | (DiskGuardProbeId::BeadsJsonlExportability, _) => {
            "Record handoff externally until Beads writeability recovers."
        }
        (DiskGuardProbeId::AgentMailDbOpen, DiskGuardProbeState::Pass) => {
            "Use Agent Mail plus Beads coordination."
        }
        (DiskGuardProbeId::AgentMailDbOpen, _) => {
            "Do not repair or restart Agent Mail; use Beads fallback coordination."
        }
        (DiskGuardProbeId::RchCacheWriteability, DiskGuardProbeState::Pass) => {
            "RCH proof lanes may be admitted when worker selection also passes."
        }
        (DiskGuardProbeId::RchCacheWriteability, _) => {
            "Do not launch material RCH proof lanes until the cache probe recovers."
        }
        (DiskGuardProbeId::ExternalScratch, DiskGuardProbeState::Pass) => {
            "External scratch is available for retained artifacts."
        }
        (DiskGuardProbeId::ExternalScratch, _) => {
            "Do not rely on external scratch until availability is re-collected."
        }
    }
}

fn nonempty_string(value: impl Into<String>, fallback: &'static str) -> String {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn nonempty_reason_code(value: impl Into<String>, probe_id: DiskGuardProbeId) -> String {
    let value = nonempty_string(value, "unknown");
    if value.contains('.') {
        value
    } else {
        format!("{}.{}", probe_id.reason_prefix(), value)
    }
}

fn push_nonempty_unique(values: &mut Vec<String>, value: impl Into<String>) {
    let value = nonempty_string(value, "unknown");
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_779_014_102_000;

    fn healthy_probe(probe_id: DiskGuardProbeId) -> DiskGuardProbe {
        match probe_id {
            DiskGuardProbeId::SystemDataVolume | DiskGuardProbeId::PrivateTmp => {
                DiskGuardProbe::filesystem_sample(
                    probe_id,
                    "fixture",
                    NOW_MS,
                    format!("/{}", probe_id.as_str()),
                    20 * 1024 * 1024 * 1024,
                    100 * 1024 * 1024 * 1024,
                    1024 * 1024 * 1024,
                )
            }
            DiskGuardProbeId::ExternalScratch => DiskGuardProbe::filesystem_sample(
                probe_id,
                "fixture",
                NOW_MS,
                "/Volumes/USB_NVME",
                200 * 1024 * 1024 * 1024,
                500 * 1024 * 1024 * 1024,
                1024 * 1024 * 1024,
            ),
            DiskGuardProbeId::RepoWriteProbe => DiskGuardProbe::status_probe(
                probe_id,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Pass,
                DiskGuardSeverity::Green,
                "bounded write probe passed",
                "write_probe.repo.pass",
                "Repository writes are allowed within owned paths.",
            ),
            DiskGuardProbeId::BeadsDbWriteability => DiskGuardProbe::status_probe(
                probe_id,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Pass,
                DiskGuardSeverity::Green,
                "Beads DB is writeable",
                "beads.db.writeable",
                "Use normal Beads coordination.",
            ),
            DiskGuardProbeId::BeadsJsonlExportability => DiskGuardProbe::status_probe(
                probe_id,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Pass,
                DiskGuardSeverity::Green,
                "JSONL export is clean",
                "beads.jsonl.export_clean",
                "Use normal Beads coordination.",
            ),
            DiskGuardProbeId::AgentMailDbOpen => DiskGuardProbe::status_probe(
                probe_id,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Pass,
                DiskGuardSeverity::Green,
                "Agent Mail DB opens read-only",
                "agent_mail.db.open",
                "Use Agent Mail plus Beads coordination.",
            ),
            DiskGuardProbeId::RchCacheWriteability => DiskGuardProbe::status_probe(
                probe_id,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Pass,
                DiskGuardSeverity::Green,
                "RCH cache is writeable",
                "rch.cache.writeable",
                "RCH proof lanes may be admitted when worker selection also passes.",
            ),
        }
    }

    fn healthy_input() -> DiskGuardCollectorInput {
        let mut input = DiskGuardCollectorInput::new(NOW_MS, "test.healthy", "/repo/frankenterm");
        for probe_id in DiskGuardProbeId::required() {
            input = input.with_probe(healthy_probe(probe_id));
        }
        input
    }

    #[test]
    fn healthy_probe_set_proceeds_with_all_required_ids() {
        let report = build_disk_guard_report(
            &healthy_input().with_artifact_path("fixtures/disk-guard/valid/healthy.json"),
        );

        assert_eq!(report.contract_id, DISK_GUARD_CONTRACT_ID);
        assert_eq!(report.decision, DiskGuardDecision::Proceed);
        assert_eq!(report.probes.len(), DiskGuardProbeId::required().len());
        assert_eq!(
            report
                .probes
                .iter()
                .map(|probe| probe.probe_id)
                .collect::<Vec<_>>(),
            DiskGuardProbeId::required().to_vec()
        );
        assert!(report.side_effect_policy.read_only);
        assert!(report.side_effect_policy.cleanup_requires_operator_approval);
        assert!(
            report
                .side_effect_policy
                .forbidden_actions
                .contains(&DiskGuardForbiddenAction::RunLocalCargoProof)
        );
        assert!(
            report
                .reason_codes
                .contains(&"policy.cleanup_requires_operator_approval".to_string())
        );
    }

    #[test]
    fn missing_required_probes_fail_closed() {
        let report = build_disk_guard_report(&DiskGuardCollectorInput::new(
            NOW_MS,
            "test.missing",
            "/repo/frankenterm",
        ));

        assert_eq!(report.decision, DiskGuardDecision::Block);
        assert_eq!(report.probes.len(), DiskGuardProbeId::required().len());
        assert!(
            report
                .reason_codes
                .contains(&"fail_closed.missing_required_probe".to_string())
        );
        assert!(report.probes.iter().all(|probe| {
            probe.state == DiskGuardProbeState::Skipped
                && probe.source == "not_collected"
                && probe.reason_codes.iter().any(|reason| {
                    reason == &format!("source.{}.not_collected", probe.probe_id.as_str())
                })
        }));
        assert!(
            report
                .side_effect_policy
                .performed_actions
                .contains(&DiskGuardPerformedAction::NotCollected)
        );
    }

    #[test]
    fn local_enospc_with_external_scratch_degrades_to_external_scratch_only() {
        let mut input = healthy_input();
        input.probes.retain(|probe| {
            !matches!(
                probe.probe_id,
                DiskGuardProbeId::SystemDataVolume | DiskGuardProbeId::PrivateTmp
            )
        });
        input = input
            .with_probe(DiskGuardProbe::filesystem_sample(
                DiskGuardProbeId::SystemDataVolume,
                "fixture",
                NOW_MS,
                "/System/Volumes/Data",
                279_506_944,
                1_995_218_165_760,
                1_073_741_824,
            ))
            .with_probe(DiskGuardProbe::filesystem_sample(
                DiskGuardProbeId::PrivateTmp,
                "fixture",
                NOW_MS,
                "/private/tmp",
                279_506_944,
                1_995_218_165_760,
                1_073_741_824,
            ));

        let report = build_disk_guard_report(&input);

        assert_eq!(report.decision, DiskGuardDecision::ExternalScratchOnly);
        assert!(
            report
                .reason_codes
                .contains(&"external_scratch.only".to_string())
        );
        assert!(
            report
                .reason_codes
                .iter()
                .any(|reason| reason == "disk.system_data_volume.below_floor")
        );
        assert!(
            report
                .reason_codes
                .iter()
                .any(|reason| reason == "disk.private_tmp.below_floor")
        );
    }

    #[test]
    fn write_precondition_failure_blocks_even_with_external_scratch() {
        let mut input = healthy_input();
        input.probes.retain(|probe| {
            probe.probe_id != DiskGuardProbeId::RepoWriteProbe
                && probe.probe_id != DiskGuardProbeId::BeadsDbWriteability
        });
        input = input
            .with_probe(
                DiskGuardProbe::status_probe(
                    DiskGuardProbeId::RepoWriteProbe,
                    "fixture",
                    NOW_MS,
                    DiskGuardProbeState::Fail,
                    DiskGuardSeverity::Black,
                    "bounded write probe failed",
                    "write_probe.repo.failed",
                    "Use external scratch only; do not apply patches in the shared checkout.",
                )
                .with_error_category("eno_space"),
            )
            .with_probe(DiskGuardProbe::status_probe(
                DiskGuardProbeId::BeadsDbWriteability,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Blocked,
                DiskGuardSeverity::Black,
                "Beads DB write is blocked",
                "beads.db.write_blocked",
                "Do not rely on DB-only tracker updates until sync recovers.",
            ));

        let report = build_disk_guard_report(&input);

        assert_eq!(report.decision, DiskGuardDecision::Block);
        assert!(
            report
                .reason_codes
                .contains(&"write_probe.repo.failed".to_string())
        );
        assert!(
            report
                .reason_codes
                .contains(&"disk.guard.blocked".to_string())
        );
    }

    #[test]
    fn degraded_status_without_hard_failure_is_static_only() {
        let mut input = healthy_input();
        input
            .probes
            .retain(|probe| probe.probe_id != DiskGuardProbeId::AgentMailDbOpen);
        input = input.with_probe(
            DiskGuardProbe::status_probe(
                DiskGuardProbeId::AgentMailDbOpen,
                "fixture",
                NOW_MS,
                DiskGuardProbeState::Degraded,
                DiskGuardSeverity::Red,
                "health responds but bootstrap is unavailable",
                "agent_mail.degraded_read_only",
                "Do not repair or restart Agent Mail; use Beads-only fallback coordination.",
            )
            .with_error_category("database_corruption_detected"),
        );

        let report = build_disk_guard_report(&input);

        assert_eq!(report.decision, DiskGuardDecision::StaticOnly);
        assert!(
            report
                .reason_codes
                .contains(&"disk.guard.static_only".to_string())
        );
    }

    #[test]
    fn cleanup_inventory_is_advisory_for_old_cached_targets() {
        let input = healthy_input().with_cleanup_candidate(
            DiskGuardCleanupCandidateInput::new(
                "/repo/frankenterm/target/old-proof",
                "frankenterm",
                DiskGuardCleanupKind::CargoTarget,
                16 * 1024 * 1024 * 1024,
            )
            .with_modified_at_ms(NOW_MS - CLEANUP_RECENT_MTIME_WINDOW_MS - 1)
            .with_cachedir_tag(true)
            .with_lsof_reference_count(0)
            .with_process_reference_count(0)
            .with_artifact_path("fixtures/disk-guard/valid/cleanup-inventory.json"),
        );

        let report = build_disk_guard_report(&input);
        let candidate = report
            .cleanup_candidates
            .first()
            .expect("cleanup candidate is retained");

        assert_eq!(candidate.risk_tier, DiskGuardCleanupRiskTier::Low);
        assert!(candidate.operator_approval_required);
        assert!(!candidate.automatic_cleanup_allowed);
        assert_eq!(candidate.live_use, DiskGuardLiveUseState::NotReferenced);
        assert!(
            candidate
                .reason_codes
                .contains(&"cleanup_candidate.no_automatic_deletion".to_string())
        );
        assert!(
            report
                .side_effect_policy
                .performed_actions
                .contains(&DiskGuardPerformedAction::ReadCleanupInventory)
        );
    }

    #[test]
    fn cleanup_inventory_protects_core_paths() {
        let input = healthy_input().with_cleanup_candidate(
            DiskGuardCleanupCandidateInput::new(
                "/repo/frankenterm/crates/frankenterm-core/target",
                "frankenterm",
                DiskGuardCleanupKind::CargoTarget,
                20 * 1024 * 1024 * 1024,
            )
            .with_modified_at_ms(NOW_MS - CLEANUP_RECENT_MTIME_WINDOW_MS - 1)
            .with_cachedir_tag(true)
            .with_lsof_reference_count(0)
            .with_process_reference_count(0),
        );

        let report = build_disk_guard_report(&input);
        let candidate = report
            .cleanup_candidates
            .first()
            .expect("cleanup candidate is retained");

        assert_eq!(candidate.risk_tier, DiskGuardCleanupRiskTier::Protected);
        assert!(!candidate.automatic_cleanup_allowed);
        assert!(
            candidate
                .reason_codes
                .contains(&"cleanup_candidate.protected_path".to_string())
        );
    }

    #[test]
    fn cleanup_inventory_live_references_are_high_risk() {
        let input = healthy_input().with_cleanup_candidate(
            DiskGuardCleanupCandidateInput::new(
                "/repo/frankenterm/target/active-proof",
                "frankenterm",
                DiskGuardCleanupKind::CargoTarget,
                10 * 1024 * 1024 * 1024,
            )
            .with_modified_at_ms(NOW_MS - CLEANUP_RECENT_MTIME_WINDOW_MS - 1)
            .with_cachedir_tag(true)
            .with_lsof_reference_count(2)
            .with_process_reference_count(1),
        );

        let report = build_disk_guard_report(&input);
        let candidate = report
            .cleanup_candidates
            .first()
            .expect("cleanup candidate is retained");

        assert_eq!(candidate.risk_tier, DiskGuardCleanupRiskTier::High);
        assert_eq!(candidate.live_use, DiskGuardLiveUseState::Referenced);
        assert!(
            candidate
                .reason_codes
                .contains(&"cleanup_candidate.live_reference".to_string())
        );
    }

    #[test]
    fn serialized_report_matches_schema_field_names() {
        let report = build_disk_guard_report(&healthy_input());
        let value = serde_json::to_value(&report).expect("report serializes");

        assert_eq!(value["contract_id"].as_str(), Some(DISK_GUARD_CONTRACT_ID));
        assert_eq!(value["decision"].as_str(), Some("proceed"));
        assert_eq!(
            value["probes"][0]["probe_id"].as_str(),
            Some(DiskGuardProbeId::SystemDataVolume.as_str())
        );
        assert_eq!(
            value["side_effect_policy"]["read_only"].as_bool(),
            Some(true)
        );
        assert!(
            value["side_effect_policy"]["forbidden_actions"]
                .as_array()
                .expect("forbidden actions array")
                .iter()
                .any(|action| action.as_str() == Some("run_local_cargo_proof"))
        );
    }
}
