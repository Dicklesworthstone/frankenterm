//! Redacted mission-twin source snapshot contract.
//!
//! The mission twin is a simulation input surface. These types describe the
//! bounded, redacted facts it may consume from Beads, RCH, Agent Mail, git,
//! reservations, and the operating envelope without carrying raw pane text or
//! live mutation authority.

use serde::{Deserialize, Serialize};

pub const MISSION_TWIN_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
pub const MISSION_TWIN_SNAPSHOT_CONTRACT_ID: &str = "ft.mission_twin_snapshot.v1";
pub const MISSION_TWIN_SNAPSHOT_SOURCE_BEAD: &str = "ft-u7r37.1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinSnapshotEnvelope {
    pub schema_version: u32,
    pub contract_id: String,
    pub source_bead: String,
    pub snapshot_id: String,
    pub generated_at_ms: u64,
    #[serde(default)]
    pub raw_pane_content_stored: bool,
    pub forbidden_actions: Vec<MissionTwinForbiddenAction>,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    pub sources: MissionTwinSources,
    pub validation: MissionTwinValidationSummary,
}

impl MissionTwinSnapshotEnvelope {
    pub fn validate(&self) -> Result<(), MissionTwinSnapshotError> {
        if self.schema_version != MISSION_TWIN_SNAPSHOT_SCHEMA_VERSION {
            return Err(MissionTwinSnapshotError::InvalidSchemaVersion {
                found: self.schema_version,
            });
        }
        if self.contract_id != MISSION_TWIN_SNAPSHOT_CONTRACT_ID {
            return Err(MissionTwinSnapshotError::InvalidContractId {
                found: self.contract_id.clone(),
            });
        }
        if self.source_bead != MISSION_TWIN_SNAPSHOT_SOURCE_BEAD {
            return Err(MissionTwinSnapshotError::InvalidSourceBead {
                found: self.source_bead.clone(),
            });
        }
        if self.snapshot_id.trim().is_empty() {
            return Err(MissionTwinSnapshotError::MissingSnapshotId);
        }
        if self.generated_at_ms == 0 {
            return Err(MissionTwinSnapshotError::MissingGeneratedAt);
        }
        if self.raw_pane_content_stored {
            return Err(MissionTwinSnapshotError::RawPaneContentStored { source: "envelope" });
        }

        for required in MissionTwinForbiddenAction::required_set() {
            if !self.forbidden_actions.contains(required) {
                return Err(MissionTwinSnapshotError::MissingForbiddenAction { action: *required });
            }
        }

        validate_paths("artifact_paths", &self.artifact_paths)?;
        self.sources.validate()?;
        self.validation.validate()?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinSources {
    pub beads: BeadsMissionTwinSnapshot,
    pub rch: RchMissionTwinSnapshot,
    pub agent_mail: AgentMailMissionTwinSnapshot,
    pub git: GitMissionTwinSnapshot,
    pub reservations: ReservationsMissionTwinSnapshot,
    pub operating_envelope: OperatingEnvelopeMissionTwinSnapshot,
}

impl MissionTwinSources {
    pub fn validate(&self) -> Result<(), MissionTwinSnapshotError> {
        self.beads.evidence.validate("beads")?;
        self.rch.evidence.validate("rch")?;
        self.agent_mail.evidence.validate("agent_mail")?;
        self.git.evidence.validate("git")?;
        self.reservations.evidence.validate("reservations")?;
        self.operating_envelope
            .evidence
            .validate("operating_envelope")?;

        let dirty_paths = self.git.dirty_path_values();
        validate_paths("git.dirty_paths", &dirty_paths)?;
        validate_paths("git.overlap_paths", &self.git.overlap_paths)?;
        validate_paths(
            "operating_envelope.source_snapshot_artifact_paths",
            &self.operating_envelope.source_snapshot_artifact_paths,
        )?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEvidence {
    pub source_id: String,
    pub status: SourceStatus,
    pub freshness_state: FreshnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collected_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_ms: Option<u64>,
    pub evidence_level: EvidenceLevel,
    #[serde(default)]
    pub redacted: bool,
    #[serde(default)]
    pub raw_pane_content_stored: bool,
    #[serde(default)]
    pub reason_codes: Vec<String>,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
}

impl SourceEvidence {
    fn validate(&self, source: &'static str) -> Result<(), MissionTwinSnapshotError> {
        if self.source_id.trim().is_empty() {
            return Err(MissionTwinSnapshotError::MissingSourceId { source });
        }
        if self.source_id != source {
            return Err(MissionTwinSnapshotError::SourceIdMismatch {
                source,
                found: self.source_id.clone(),
            });
        }
        if self.status != SourceStatus::Unavailable && self.collected_at_ms.unwrap_or(0) == 0 {
            return Err(MissionTwinSnapshotError::MissingCollectedAt { source });
        }
        if !self.redacted {
            return Err(MissionTwinSnapshotError::UnredactedSource { source });
        }
        if self.raw_pane_content_stored {
            return Err(MissionTwinSnapshotError::RawPaneContentStored { source });
        }
        validate_paths(source, &self.artifact_paths)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadsMissionTwinSnapshot {
    #[serde(flatten)]
    pub evidence: SourceEvidence,
    pub ready_count: u32,
    pub blocked_count: u32,
    pub in_progress_count: u32,
    #[serde(default)]
    pub dependency_blockers: Vec<DependencyBlocker>,
    #[serde(default)]
    pub owner_states: Vec<BeadOwnerState>,
    #[serde(default)]
    pub stale_owner_candidates: Vec<StaleOwnerCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyBlocker {
    pub blocked_bead_id: String,
    pub blocking_bead_id: String,
    pub blocker_status: String,
    #[serde(default)]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BeadOwnerState {
    pub bead_id: String,
    pub assignee: String,
    pub owner_state: OwnerState,
    pub age_seconds: u64,
    pub last_activity_source: String,
    #[serde(default)]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleOwnerCandidate {
    pub bead_id: String,
    pub assignee: String,
    pub age_seconds: u64,
    pub last_activity_source: String,
    #[serde(default)]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RchMissionTwinSnapshot {
    #[serde(flatten)]
    pub evidence: SourceEvidence,
    pub admission_state: RchAdmissionState,
    pub healthy_workers: u32,
    pub total_workers: u32,
    pub critical_pressure_count: u32,
    #[serde(default)]
    pub admission_reasons: Vec<String>,
    #[serde(default)]
    pub blocked_proof_lanes: Vec<BlockedProofLane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedProofLane {
    pub bead_id: String,
    pub command_family: String,
    #[serde(default)]
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMailMissionTwinSnapshot {
    #[serde(flatten)]
    pub evidence: SourceEvidence,
    pub availability_state: AgentMailAvailabilityState,
    #[serde(default)]
    pub active_agents: Vec<ActiveAgentSummary>,
    #[serde(default)]
    pub fallback_reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveAgentSummary {
    pub agent_name: String,
    pub task_summary: String,
    pub last_active_age_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitMissionTwinSnapshot {
    #[serde(flatten)]
    pub evidence: SourceEvidence,
    pub branch: String,
    pub head: String,
    #[serde(default)]
    pub remote_heads: Vec<RemoteHead>,
    #[serde(default)]
    pub dirty_paths: Vec<DirtyPathSummary>,
    #[serde(default)]
    pub overlap_paths: Vec<String>,
    #[serde(default)]
    pub deletion_paths_present: bool,
}

impl GitMissionTwinSnapshot {
    fn dirty_path_values(&self) -> Vec<String> {
        self.dirty_paths
            .iter()
            .map(|entry| entry.path.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteHead {
    pub name: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirtyPathSummary {
    pub path: String,
    pub status: String,
    pub overlaps_owned_path: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationsMissionTwinSnapshot {
    #[serde(flatten)]
    pub evidence: SourceEvidence,
    #[serde(default)]
    pub active_reservations: Vec<ReservationSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservationSummary {
    pub holder: String,
    pub path_pattern: String,
    pub exclusive: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeMissionTwinSnapshot {
    #[serde(flatten)]
    pub evidence: SourceEvidence,
    pub verdict: OperatingEnvelopeVerdict,
    #[serde(default)]
    pub reason_codes: Vec<String>,
    #[serde(default)]
    pub source_snapshot_artifact_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinValidationSummary {
    pub validation_state: ValidationState,
    #[serde(default)]
    pub rejected_inputs: Vec<RejectedInputSummary>,
    #[serde(default)]
    pub destructive_action_hints: Vec<MissionTwinForbiddenAction>,
    pub ambiguous_timestamps_rejected: bool,
}

impl MissionTwinValidationSummary {
    fn validate(&self) -> Result<(), MissionTwinSnapshotError> {
        if !self.destructive_action_hints.is_empty() {
            return Err(MissionTwinSnapshotError::DestructiveActionHint {
                action: self.destructive_action_hints[0],
            });
        }
        if !self.ambiguous_timestamps_rejected {
            return Err(MissionTwinSnapshotError::AmbiguousTimestampPolicyMissing);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedInputSummary {
    pub input_id: String,
    pub reason_code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Available,
    Degraded,
    Unavailable,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    Fresh,
    Stale,
    Unknown,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    LiveCommand,
    RetainedArtifact,
    Fixture,
    ManualNote,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerState {
    Active,
    StaleCandidate,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RchAdmissionState {
    Ready,
    NotReady,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMailAvailabilityState {
    Healthy,
    Red,
    Fallback,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeVerdict {
    Admit,
    Deny,
    Shed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationState {
    Accepted,
    Rejected,
    OperatorReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinForbiddenAction {
    AgentMailServiceRepairRestart,
    RchServiceRepairRestart,
    WorkerMutation,
    BuildCancellation,
    FileDeletion,
    DestructiveGit,
    LocalCargoProof,
    PaneMutation,
    RawPaneContentStorage,
    BeadsMutation,
}

impl MissionTwinForbiddenAction {
    pub fn required_set() -> &'static [Self] {
        &[
            Self::AgentMailServiceRepairRestart,
            Self::RchServiceRepairRestart,
            Self::WorkerMutation,
            Self::BuildCancellation,
            Self::FileDeletion,
            Self::DestructiveGit,
            Self::LocalCargoProof,
            Self::PaneMutation,
            Self::RawPaneContentStorage,
            Self::BeadsMutation,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionTwinSnapshotError {
    InvalidSchemaVersion { found: u32 },
    InvalidContractId { found: String },
    InvalidSourceBead { found: String },
    MissingSnapshotId,
    MissingGeneratedAt,
    MissingSourceId { source: &'static str },
    SourceIdMismatch { source: &'static str, found: String },
    MissingCollectedAt { source: &'static str },
    MissingForbiddenAction { action: MissionTwinForbiddenAction },
    RawPaneContentStored { source: &'static str },
    UnredactedSource { source: &'static str },
    UnsafePath { field: &'static str, value: String },
    DestructiveActionHint { action: MissionTwinForbiddenAction },
    AmbiguousTimestampPolicyMissing,
}

impl std::fmt::Display for MissionTwinSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSchemaVersion { found } => {
                write!(f, "invalid mission twin snapshot schema version: {found}")
            }
            Self::InvalidContractId { found } => {
                write!(f, "invalid mission twin snapshot contract id: {found}")
            }
            Self::InvalidSourceBead { found } => {
                write!(f, "invalid mission twin snapshot source bead: {found}")
            }
            Self::MissingSnapshotId => write!(f, "mission twin snapshot id is required"),
            Self::MissingGeneratedAt => {
                write!(
                    f,
                    "mission twin generated_at_ms must be a positive epoch-ms value"
                )
            }
            Self::MissingSourceId { source } => {
                write!(f, "{source} source_id is required")
            }
            Self::SourceIdMismatch { source, found } => {
                write!(
                    f,
                    "{source} source_id does not match expected source: {found}"
                )
            }
            Self::MissingCollectedAt { source } => {
                write!(f, "{source} collected_at_ms is required unless unavailable")
            }
            Self::MissingForbiddenAction { action } => {
                write!(f, "missing forbidden action {action:?}")
            }
            Self::RawPaneContentStored { source } => {
                write!(f, "{source} stores raw pane content")
            }
            Self::UnredactedSource { source } => write!(f, "{source} is not redacted"),
            Self::UnsafePath { field, value } => {
                write!(
                    f,
                    "{field} contains unsafe repository-relative path: {value}"
                )
            }
            Self::DestructiveActionHint { action } => {
                write!(
                    f,
                    "mission twin output includes destructive action hint {action:?}"
                )
            }
            Self::AmbiguousTimestampPolicyMissing => {
                write!(f, "ambiguous timestamp rejection policy is missing")
            }
        }
    }
}

impl std::error::Error for MissionTwinSnapshotError {}

pub fn is_safe_repo_relative_path(path: &str) -> bool {
    if path.is_empty()
        || path == "."
        || path == ".."
        || path.starts_with('/')
        || path.starts_with("./")
        || path.starts_with("../")
        || path.starts_with('~')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains("://")
    {
        return false;
    }

    path.split('/').all(|segment| {
        !segment.is_empty() && segment != "." && segment != ".." && segment != ".git"
    })
}

fn validate_paths(field: &'static str, paths: &[String]) -> Result<(), MissionTwinSnapshotError> {
    for path in paths {
        if !is_safe_repo_relative_path(path) {
            return Err(MissionTwinSnapshotError::UnsafePath {
                field,
                value: path.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_repo_relative_path_rejects_escape_shapes() {
        for path in [
            "",
            ".",
            "..",
            "/tmp/snapshot.json",
            "./snapshot.json",
            "../snapshot.json",
            "fixtures/../snapshot.json",
            "fixtures/.git/config",
            "fixtures\\snapshot.json",
            "https://example.invalid/snapshot.json",
            "fixtures/mission-twin/snapshot/",
        ] {
            assert!(
                !is_safe_repo_relative_path(path),
                "expected unsafe path to be rejected: {path}"
            );
        }

        assert!(is_safe_repo_relative_path(
            "fixtures/mission-twin/snapshot/valid/healthy.json"
        ));
    }

    #[test]
    fn validation_rejects_raw_pane_content() {
        let mut snapshot = valid_minimal_snapshot();
        snapshot.sources.beads.evidence.raw_pane_content_stored = true;

        assert_eq!(
            snapshot.validate(),
            Err(MissionTwinSnapshotError::RawPaneContentStored { source: "beads" })
        );
    }

    #[test]
    fn validation_rejects_destructive_action_hints() {
        let mut snapshot = valid_minimal_snapshot();
        snapshot
            .validation
            .destructive_action_hints
            .push(MissionTwinForbiddenAction::FileDeletion);

        assert_eq!(
            snapshot.validate(),
            Err(MissionTwinSnapshotError::DestructiveActionHint {
                action: MissionTwinForbiddenAction::FileDeletion
            })
        );
    }

    fn valid_minimal_snapshot() -> MissionTwinSnapshotEnvelope {
        MissionTwinSnapshotEnvelope {
            schema_version: MISSION_TWIN_SNAPSHOT_SCHEMA_VERSION,
            contract_id: MISSION_TWIN_SNAPSHOT_CONTRACT_ID.to_string(),
            source_bead: MISSION_TWIN_SNAPSHOT_SOURCE_BEAD.to_string(),
            snapshot_id: "unit-healthy".to_string(),
            generated_at_ms: 1,
            raw_pane_content_stored: false,
            forbidden_actions: MissionTwinForbiddenAction::required_set().to_vec(),
            artifact_paths: vec!["fixtures/mission-twin/snapshot/valid/healthy.json".to_string()],
            sources: MissionTwinSources {
                beads: BeadsMissionTwinSnapshot {
                    evidence: evidence("beads"),
                    ready_count: 1,
                    blocked_count: 0,
                    in_progress_count: 0,
                    dependency_blockers: Vec::new(),
                    owner_states: Vec::new(),
                    stale_owner_candidates: Vec::new(),
                },
                rch: RchMissionTwinSnapshot {
                    evidence: evidence("rch"),
                    admission_state: RchAdmissionState::Ready,
                    healthy_workers: 8,
                    total_workers: 8,
                    critical_pressure_count: 0,
                    admission_reasons: Vec::new(),
                    blocked_proof_lanes: Vec::new(),
                },
                agent_mail: AgentMailMissionTwinSnapshot {
                    evidence: evidence("agent_mail"),
                    availability_state: AgentMailAvailabilityState::Healthy,
                    active_agents: Vec::new(),
                    fallback_reason_codes: Vec::new(),
                },
                git: GitMissionTwinSnapshot {
                    evidence: evidence("git"),
                    branch: "main".to_string(),
                    head: "0123456789abcdef0123456789abcdef01234567".to_string(),
                    remote_heads: Vec::new(),
                    dirty_paths: Vec::new(),
                    overlap_paths: Vec::new(),
                    deletion_paths_present: false,
                },
                reservations: ReservationsMissionTwinSnapshot {
                    evidence: evidence("reservations"),
                    active_reservations: Vec::new(),
                },
                operating_envelope: OperatingEnvelopeMissionTwinSnapshot {
                    evidence: evidence("operating_envelope"),
                    verdict: OperatingEnvelopeVerdict::Admit,
                    reason_codes: Vec::new(),
                    source_snapshot_artifact_paths: Vec::new(),
                },
            },
            validation: MissionTwinValidationSummary {
                validation_state: ValidationState::Accepted,
                rejected_inputs: Vec::new(),
                destructive_action_hints: Vec::new(),
                ambiguous_timestamps_rejected: true,
            },
        }
    }

    fn evidence(source_id: &str) -> SourceEvidence {
        SourceEvidence {
            source_id: source_id.to_string(),
            status: SourceStatus::Available,
            freshness_state: FreshnessState::Fresh,
            collected_at_ms: Some(1),
            freshness_ms: Some(1),
            evidence_level: EvidenceLevel::Fixture,
            redacted: true,
            raw_pane_content_stored: false,
            reason_codes: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }
}
