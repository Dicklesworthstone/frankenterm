//! Read-only blocker-radar DTOs and normalization.
//!
//! This module intentionally does not execute subprocesses. It receives bounded,
//! already-redacted collector observations from future CLI/robot surfaces and
//! normalizes them into the `ft.blocker_radar.v1` contract.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const BLOCKER_RADAR_CONTRACT_ID: &str = "ft.blocker_radar.v1";
pub const BLOCKER_RADAR_SCHEMA_VERSION: u16 = 1;
pub const BLOCKER_RADAR_COMMAND_OUTPUT_MAX_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarEvidenceState {
    Actionable,
    WaitingExternal,
    WaitingOwner,
    StalePossible,
    DirtyOverlap,
    RchSubstrateBlocked,
    CiQueued,
    CiZeroJobs,
    ArtifactMissing,
    MailUnavailable,
    Degraded,
    Unknown,
}

impl BlockerRadarEvidenceState {
    fn priority_rank(self) -> u8 {
        match self {
            Self::DirtyOverlap => 110,
            Self::RchSubstrateBlocked => 105,
            Self::ArtifactMissing => 100,
            Self::CiZeroJobs => 95,
            Self::CiQueued => 90,
            Self::MailUnavailable => 85,
            Self::WaitingOwner => 80,
            Self::WaitingExternal => 75,
            Self::StalePossible => 70,
            Self::Degraded => 60,
            Self::Unknown => 50,
            Self::Actionable => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarSourceKind {
    Rch,
    GitHubActions,
    AgentMail,
    Beads,
    Git,
    Manual,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarSeverity {
    Info,
    Warning,
    Blocked,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarSubstrate {
    Rch,
    GitHubActions,
    AgentMail,
    Beads,
    Git,
    PackageArtifact,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarActionKind {
    RecheckStatus,
    InspectArtifact,
    AddBeadsComment,
    WaitForOwner,
    ChooseReadyBead,
    RunBvRobotTriage,
    RunSwarmTick,
    FileFollowupBead,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarFailureClass {
    SourceRegression,
    PrivacyViolation,
    EnvironmentBlocked,
    UnavailableEvidence,
    ExternalQueueBlocked,
    DirtyTreeBlocked,
    OwnerHandoffRequired,
    TargetHardwareSkipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerRadarObservationStatus {
    PassActionable,
    RchSubstrateBlocked,
    RchLocalFallbackRefused,
    CiQueued,
    CiZeroJobs,
    ArtifactMissing,
    MailUnavailable,
    StalePossible,
    ActiveOwnerFresh,
    DirtyOverlap,
    DegradedUnavailable,
    Unknown,
}

impl BlockerRadarObservationStatus {
    fn evidence_state(self) -> BlockerRadarEvidenceState {
        match self {
            Self::PassActionable => BlockerRadarEvidenceState::Actionable,
            Self::RchSubstrateBlocked | Self::RchLocalFallbackRefused => {
                BlockerRadarEvidenceState::RchSubstrateBlocked
            }
            Self::CiQueued => BlockerRadarEvidenceState::CiQueued,
            Self::CiZeroJobs => BlockerRadarEvidenceState::CiZeroJobs,
            Self::ArtifactMissing => BlockerRadarEvidenceState::ArtifactMissing,
            Self::MailUnavailable => BlockerRadarEvidenceState::MailUnavailable,
            Self::StalePossible => BlockerRadarEvidenceState::StalePossible,
            Self::ActiveOwnerFresh => BlockerRadarEvidenceState::WaitingOwner,
            Self::DirtyOverlap => BlockerRadarEvidenceState::DirtyOverlap,
            Self::DegradedUnavailable => BlockerRadarEvidenceState::Degraded,
            Self::Unknown => BlockerRadarEvidenceState::Unknown,
        }
    }

    fn default_reason_code(self) -> &'static str {
        match self {
            Self::PassActionable => "evidence.actionable",
            Self::RchSubstrateBlocked => "rch.substrate_blocked",
            Self::RchLocalFallbackRefused => "rch.local_fallback_refused",
            Self::CiQueued => "ci.queued",
            Self::CiZeroJobs => "ci.zero_jobs",
            Self::ArtifactMissing => "artifact.missing",
            Self::MailUnavailable => "agent_mail.unavailable",
            Self::StalePossible => "beads.owner_stale_possible",
            Self::ActiveOwnerFresh => "beads.active_owner_fresh",
            Self::DirtyOverlap => "git.dirty_overlap",
            Self::DegradedUnavailable => "source.degraded",
            Self::Unknown => "evidence.unknown",
        }
    }

    fn severity(self) -> BlockerRadarSeverity {
        match self {
            Self::PassActionable => BlockerRadarSeverity::Info,
            Self::CiQueued | Self::StalePossible | Self::DegradedUnavailable | Self::Unknown => {
                BlockerRadarSeverity::Warning
            }
            Self::RchSubstrateBlocked
            | Self::RchLocalFallbackRefused
            | Self::CiZeroJobs
            | Self::ArtifactMissing
            | Self::MailUnavailable
            | Self::ActiveOwnerFresh
            | Self::DirtyOverlap => BlockerRadarSeverity::Blocked,
        }
    }

    fn action_kind(self) -> BlockerRadarActionKind {
        match self {
            Self::PassActionable => BlockerRadarActionKind::ChooseReadyBead,
            Self::RchSubstrateBlocked | Self::RchLocalFallbackRefused => {
                BlockerRadarActionKind::FileFollowupBead
            }
            Self::CiQueued | Self::CiZeroJobs => BlockerRadarActionKind::RecheckStatus,
            Self::ArtifactMissing => BlockerRadarActionKind::InspectArtifact,
            Self::MailUnavailable => BlockerRadarActionKind::RunSwarmTick,
            Self::StalePossible => BlockerRadarActionKind::AddBeadsComment,
            Self::ActiveOwnerFresh | Self::DirtyOverlap => BlockerRadarActionKind::WaitForOwner,
            Self::DegradedUnavailable | Self::Unknown => BlockerRadarActionKind::RunBvRobotTriage,
        }
    }

    fn failure_class(self) -> BlockerRadarFailureClass {
        match self {
            Self::PassActionable => BlockerRadarFailureClass::UnavailableEvidence,
            Self::RchSubstrateBlocked | Self::RchLocalFallbackRefused => {
                BlockerRadarFailureClass::EnvironmentBlocked
            }
            Self::CiQueued | Self::CiZeroJobs | Self::ArtifactMissing => {
                BlockerRadarFailureClass::ExternalQueueBlocked
            }
            Self::MailUnavailable | Self::DegradedUnavailable | Self::Unknown => {
                BlockerRadarFailureClass::UnavailableEvidence
            }
            Self::StalePossible | Self::ActiveOwnerFresh => {
                BlockerRadarFailureClass::OwnerHandoffRequired
            }
            Self::DirtyOverlap => BlockerRadarFailureClass::DirtyTreeBlocked,
        }
    }

    fn is_blocker(self) -> bool {
        self != Self::PassActionable
    }

    fn is_external_queue(self) -> bool {
        matches!(
            self,
            Self::RchSubstrateBlocked
                | Self::RchLocalFallbackRefused
                | Self::CiQueued
                | Self::CiZeroJobs
                | Self::ArtifactMissing
        )
    }

    fn is_unavailable_source(self) -> bool {
        matches!(
            self,
            Self::MailUnavailable | Self::DegradedUnavailable | Self::Unknown
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarInput {
    pub generated_at_ms: u64,
    pub source: String,
    pub observations: Vec<BlockerRadarCollectorObservation>,
    pub artifact_paths: Vec<String>,
}

impl BlockerRadarInput {
    #[must_use]
    pub fn new(generated_at_ms: u64, source: impl Into<String>) -> Self {
        Self {
            generated_at_ms,
            source: source.into(),
            observations: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_observation(mut self, observation: BlockerRadarCollectorObservation) -> Self {
        self.observations.push(observation);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, artifact_path: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.artifact_paths, artifact_path);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarCollectorObservation {
    pub source_id: String,
    pub source_kind: BlockerRadarSourceKind,
    pub status: BlockerRadarObservationStatus,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub live: bool,
    pub summary: String,
    pub reason_codes: Vec<String>,
    pub dependency_ids: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub affected_paths: Vec<String>,
    pub owner: Option<String>,
    pub updated_at_ms: Option<u64>,
    pub run_id: Option<String>,
    pub url: Option<String>,
    pub worker_id: Option<String>,
    pub artifact_name: Option<String>,
}

impl BlockerRadarCollectorObservation {
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        source_kind: BlockerRadarSourceKind,
        status: BlockerRadarObservationStatus,
        command_or_api: impl Into<String>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            source_id: nonempty_string(source_id, "source.unknown"),
            source_kind,
            status,
            collected_at_ms: None,
            freshness_ms: None,
            command_or_api: nonempty_string(command_or_api, "collector.unavailable"),
            live: false,
            summary: nonempty_string(summary, "collector evidence unavailable"),
            reason_codes: vec![status.default_reason_code().to_string()],
            dependency_ids: Vec::new(),
            artifact_paths: Vec::new(),
            affected_paths: Vec::new(),
            owner: None,
            updated_at_ms: None,
            run_id: None,
            url: None,
            worker_id: None,
            artifact_name: None,
        }
    }

    #[must_use]
    pub fn live(mut self, collected_at_ms: u64, freshness_ms: u64) -> Self {
        self.live = true;
        self.collected_at_ms = Some(collected_at_ms);
        self.freshness_ms = Some(freshness_ms);
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_dependency_id(mut self, dependency_id: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.dependency_ids, dependency_id);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, artifact_path: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.artifact_paths, artifact_path);
        self
    }

    #[must_use]
    pub fn with_affected_path(mut self, path: impl Into<String>) -> Self {
        push_nonempty_unique(&mut self.affected_paths, path);
        self
    }

    #[must_use]
    pub fn with_owner(mut self, owner: impl Into<String>, updated_at_ms: Option<u64>) -> Self {
        self.owner = Some(nonempty_string(owner, "unknown"));
        self.updated_at_ms = updated_at_ms;
        self
    }

    #[must_use]
    pub fn with_run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = Some(nonempty_string(run_id, "run.unknown"));
        self
    }

    #[must_use]
    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(nonempty_string(url, "about:blank"));
        self
    }

    #[must_use]
    pub fn with_worker_id(mut self, worker_id: impl Into<String>) -> Self {
        self.worker_id = Some(nonempty_string(worker_id, "worker.unknown"));
        self
    }

    #[must_use]
    pub fn with_artifact_name(mut self, artifact_name: impl Into<String>) -> Self {
        self.artifact_name = Some(nonempty_string(artifact_name, "artifact.unknown"));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarSourceSnapshot {
    pub source_id: String,
    pub source_kind: BlockerRadarSourceKind,
    pub evidence_state: BlockerRadarEvidenceState,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub live: bool,
    pub redacted: bool,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarBlocker {
    pub blocker_id: String,
    pub evidence_state: BlockerRadarEvidenceState,
    pub severity: BlockerRadarSeverity,
    pub summary: String,
    pub source_ids: Vec<String>,
    pub citation_ids: Vec<String>,
    pub dependency_ids: Vec<String>,
    pub next_action_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarActiveAgent {
    pub agent_name: String,
    pub active_beads: Vec<String>,
    pub evidence_state: BlockerRadarEvidenceState,
    pub updated_at_ms: Option<u64>,
    pub stale_over_threshold: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarDirtyOverlap {
    pub path: String,
    pub status: String,
    pub risk_level: BlockerRadarSeverity,
    pub expected_owner: Option<String>,
    pub related_bead_ids: Vec<String>,
    pub recommendation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarExternalQueue {
    pub queue_id: String,
    pub substrate: BlockerRadarSubstrate,
    pub evidence_state: BlockerRadarEvidenceState,
    pub run_id: Option<String>,
    pub url: Option<String>,
    pub worker_id: Option<String>,
    pub artifact_name: Option<String>,
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarNextAction {
    pub action_id: String,
    pub action_kind: BlockerRadarActionKind,
    pub mutation_allowed: bool,
    pub operator_summary: String,
    pub suggested_command: Option<String>,
    pub reason_codes: Vec<String>,
    pub citation_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarForbiddenAction {
    pub command_pattern: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarCitation {
    pub citation_id: String,
    pub source_id: String,
    pub summary: String,
    pub redacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarUnavailableSource {
    pub source_kind: BlockerRadarSourceKind,
    pub evidence_state: BlockerRadarEvidenceState,
    pub reason_codes: Vec<String>,
    pub failure_class: BlockerRadarFailureClass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarRedactionPolicy {
    pub raw_pane_content_allowed: bool,
    pub raw_prompt_allowed: bool,
    pub bounded_citations_only: bool,
    pub secret_redaction_required: bool,
    pub command_output_max_bytes: u64,
}

impl Default for BlockerRadarRedactionPolicy {
    fn default() -> Self {
        Self {
            raw_pane_content_allowed: false,
            raw_prompt_allowed: false,
            bounded_citations_only: true,
            secret_redaction_required: true,
            command_output_max_bytes: BLOCKER_RADAR_COMMAND_OUTPUT_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerRadarReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub overall_state: BlockerRadarEvidenceState,
    pub sources: Vec<BlockerRadarSourceSnapshot>,
    pub blockers: Vec<BlockerRadarBlocker>,
    pub active_agents: Vec<BlockerRadarActiveAgent>,
    pub dirty_overlap: Vec<BlockerRadarDirtyOverlap>,
    pub external_queues: Vec<BlockerRadarExternalQueue>,
    pub next_actions: Vec<BlockerRadarNextAction>,
    pub forbidden_actions: Vec<BlockerRadarForbiddenAction>,
    pub citations: Vec<BlockerRadarCitation>,
    pub unavailable_sources: Vec<BlockerRadarUnavailableSource>,
    pub redaction_policy: BlockerRadarRedactionPolicy,
    pub raw_pane_content_stored: bool,
    pub artifact_paths: Vec<String>,
}

#[must_use]
pub fn build_blocker_radar_report(input: &BlockerRadarInput) -> BlockerRadarReport {
    let observations = if input.observations.is_empty() {
        vec![BlockerRadarCollectorObservation::new(
            "manual.unknown",
            BlockerRadarSourceKind::Manual,
            BlockerRadarObservationStatus::Unknown,
            "manual",
            "no blocker-radar observations were provided",
        )]
    } else {
        input.observations.clone()
    };

    let sources = observations.iter().map(source_snapshot).collect::<Vec<_>>();
    let citations = observations
        .iter()
        .map(citation_for_observation)
        .collect::<Vec<_>>();
    let next_actions = next_actions_for_observations(&observations);
    let blockers = observations
        .iter()
        .filter(|observation| observation.status.is_blocker())
        .map(blocker_for_observation)
        .collect::<Vec<_>>();
    let active_agents = observations
        .iter()
        .filter_map(active_agent_for_observation)
        .collect::<Vec<_>>();
    let dirty_overlap = observations
        .iter()
        .flat_map(dirty_overlap_for_observation)
        .collect::<Vec<_>>();
    let external_queues = observations
        .iter()
        .filter(|observation| observation.status.is_external_queue())
        .map(external_queue_for_observation)
        .collect::<Vec<_>>();
    let unavailable_sources = observations
        .iter()
        .filter(|observation| observation.status.is_unavailable_source())
        .map(unavailable_source_for_observation)
        .collect::<Vec<_>>();
    let mut artifact_paths = input.artifact_paths.clone();
    for observation in &observations {
        for artifact_path in &observation.artifact_paths {
            push_nonempty_unique(&mut artifact_paths, artifact_path.clone());
        }
    }

    BlockerRadarReport {
        schema_version: BLOCKER_RADAR_SCHEMA_VERSION,
        contract_id: BLOCKER_RADAR_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        source: nonempty_string(input.source.clone(), "blocker_radar.unknown"),
        overall_state: overall_state(&observations),
        sources,
        blockers,
        active_agents,
        dirty_overlap,
        external_queues,
        next_actions,
        forbidden_actions: forbidden_actions(),
        citations,
        unavailable_sources,
        redaction_policy: BlockerRadarRedactionPolicy::default(),
        raw_pane_content_stored: false,
        artifact_paths,
    }
}

fn source_snapshot(observation: &BlockerRadarCollectorObservation) -> BlockerRadarSourceSnapshot {
    BlockerRadarSourceSnapshot {
        source_id: observation.source_id.clone(),
        source_kind: observation.source_kind,
        evidence_state: observation.status.evidence_state(),
        collected_at_ms: observation.collected_at_ms,
        freshness_ms: observation.freshness_ms,
        command_or_api: observation.command_or_api.clone(),
        live: observation.live,
        redacted: true,
        reason_codes: nonempty_reason_codes(
            &observation.reason_codes,
            observation.status.default_reason_code(),
        ),
        artifact_paths: observation.artifact_paths.clone(),
    }
}

fn citation_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> BlockerRadarCitation {
    BlockerRadarCitation {
        citation_id: citation_id(&observation.source_id),
        source_id: observation.source_id.clone(),
        summary: observation.summary.clone(),
        redacted: true,
    }
}

fn blocker_for_observation(observation: &BlockerRadarCollectorObservation) -> BlockerRadarBlocker {
    let action_id = action_id(observation);
    BlockerRadarBlocker {
        blocker_id: format!("blocker.{}", observation.source_id),
        evidence_state: observation.status.evidence_state(),
        severity: observation.status.severity(),
        summary: observation.summary.clone(),
        source_ids: vec![observation.source_id.clone()],
        citation_ids: vec![citation_id(&observation.source_id)],
        dependency_ids: observation.dependency_ids.clone(),
        next_action_ids: vec![action_id],
    }
}

fn active_agent_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> Option<BlockerRadarActiveAgent> {
    if !matches!(
        observation.status,
        BlockerRadarObservationStatus::ActiveOwnerFresh
            | BlockerRadarObservationStatus::StalePossible
    ) {
        return None;
    }

    Some(BlockerRadarActiveAgent {
        agent_name: observation
            .owner
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        active_beads: observation.dependency_ids.clone(),
        evidence_state: observation.status.evidence_state(),
        updated_at_ms: observation.updated_at_ms,
        stale_over_threshold: observation.status == BlockerRadarObservationStatus::StalePossible,
    })
}

fn dirty_overlap_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> Vec<BlockerRadarDirtyOverlap> {
    if observation.status != BlockerRadarObservationStatus::DirtyOverlap {
        return Vec::new();
    }

    let paths = if observation.affected_paths.is_empty() {
        vec!["unknown".to_string()]
    } else {
        observation.affected_paths.clone()
    };

    paths
        .into_iter()
        .map(|path| BlockerRadarDirtyOverlap {
            path,
            status: observation.summary.clone(),
            risk_level: observation.status.severity(),
            expected_owner: observation.owner.clone(),
            related_bead_ids: observation.dependency_ids.clone(),
            recommendation: "do not stage or edit overlapping paths until ownership is clear"
                .to_string(),
        })
        .collect()
}

fn external_queue_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> BlockerRadarExternalQueue {
    BlockerRadarExternalQueue {
        queue_id: observation
            .run_id
            .clone()
            .or_else(|| observation.artifact_name.clone())
            .unwrap_or_else(|| observation.source_id.clone()),
        substrate: substrate_for_observation(observation),
        evidence_state: observation.status.evidence_state(),
        run_id: observation.run_id.clone(),
        url: observation.url.clone(),
        worker_id: observation.worker_id.clone(),
        artifact_name: observation.artifact_name.clone(),
        source_ids: vec![observation.source_id.clone()],
    }
}

fn unavailable_source_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> BlockerRadarUnavailableSource {
    BlockerRadarUnavailableSource {
        source_kind: observation.source_kind,
        evidence_state: observation.status.evidence_state(),
        reason_codes: nonempty_reason_codes(
            &observation.reason_codes,
            observation.status.default_reason_code(),
        ),
        failure_class: observation.status.failure_class(),
    }
}

fn next_actions_for_observations(
    observations: &[BlockerRadarCollectorObservation],
) -> Vec<BlockerRadarNextAction> {
    let mut seen = BTreeSet::new();
    let mut actions = Vec::new();
    for observation in observations {
        let action = next_action_for_observation(observation);
        if seen.insert(action.action_id.clone()) {
            actions.push(action);
        }
    }
    actions
}

fn next_action_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> BlockerRadarNextAction {
    let action_kind = observation.status.action_kind();
    BlockerRadarNextAction {
        action_id: action_id(observation),
        action_kind,
        mutation_allowed: false,
        operator_summary: operator_summary_for_action(observation),
        suggested_command: suggested_command(action_kind),
        reason_codes: nonempty_reason_codes(
            &observation.reason_codes,
            observation.status.default_reason_code(),
        ),
        citation_ids: vec![citation_id(&observation.source_id)],
    }
}

fn operator_summary_for_action(observation: &BlockerRadarCollectorObservation) -> String {
    match observation.status {
        BlockerRadarObservationStatus::PassActionable => {
            "choose the next ready bead after confirming ownership and dirty paths".to_string()
        }
        BlockerRadarObservationStatus::RchSubstrateBlocked
        | BlockerRadarObservationStatus::RchLocalFallbackRefused => {
            "record the RCH substrate blocker; do not count sync or setup chatter as proof"
                .to_string()
        }
        BlockerRadarObservationStatus::CiQueued | BlockerRadarObservationStatus::CiZeroJobs => {
            "recheck the current CI run or check suite without cancelling or rerunning it"
                .to_string()
        }
        BlockerRadarObservationStatus::ArtifactMissing => {
            "inspect retained artifact metadata before unblocking package or proof work".to_string()
        }
        BlockerRadarObservationStatus::MailUnavailable => {
            "use the Agent Mail fallback snapshot and continue coordination through Beads/git"
                .to_string()
        }
        BlockerRadarObservationStatus::StalePossible => {
            "comment with evidence before reopening or taking over the bead".to_string()
        }
        BlockerRadarObservationStatus::ActiveOwnerFresh => {
            "wait for the current owner or request a handoff".to_string()
        }
        BlockerRadarObservationStatus::DirtyOverlap => {
            "avoid editing or staging the overlapping dirty paths".to_string()
        }
        BlockerRadarObservationStatus::DegradedUnavailable
        | BlockerRadarObservationStatus::Unknown => {
            "refresh read-only triage before claiming safety".to_string()
        }
    }
}

fn suggested_command(action_kind: BlockerRadarActionKind) -> Option<String> {
    match action_kind {
        BlockerRadarActionKind::RecheckStatus => {
            Some("gh run view --json status,conclusion,jobs".to_string())
        }
        BlockerRadarActionKind::ChooseReadyBead => Some("br ready --json".to_string()),
        BlockerRadarActionKind::RunBvRobotTriage => Some("bv --robot-triage".to_string()),
        BlockerRadarActionKind::RunSwarmTick => {
            Some("scripts/swarm-tick.sh --agent-mail-fallback frankenterm".to_string())
        }
        BlockerRadarActionKind::InspectArtifact
        | BlockerRadarActionKind::AddBeadsComment
        | BlockerRadarActionKind::WaitForOwner
        | BlockerRadarActionKind::FileFollowupBead
        | BlockerRadarActionKind::None => None,
    }
}

fn substrate_for_observation(
    observation: &BlockerRadarCollectorObservation,
) -> BlockerRadarSubstrate {
    if observation.status == BlockerRadarObservationStatus::ArtifactMissing {
        return BlockerRadarSubstrate::PackageArtifact;
    }

    match observation.source_kind {
        BlockerRadarSourceKind::Rch => BlockerRadarSubstrate::Rch,
        BlockerRadarSourceKind::GitHubActions => BlockerRadarSubstrate::GitHubActions,
        BlockerRadarSourceKind::AgentMail => BlockerRadarSubstrate::AgentMail,
        BlockerRadarSourceKind::Beads => BlockerRadarSubstrate::Beads,
        BlockerRadarSourceKind::Git => BlockerRadarSubstrate::Git,
        BlockerRadarSourceKind::Manual | BlockerRadarSourceKind::Fixture => {
            BlockerRadarSubstrate::Unknown
        }
    }
}

fn overall_state(observations: &[BlockerRadarCollectorObservation]) -> BlockerRadarEvidenceState {
    observations
        .iter()
        .map(|observation| observation.status.evidence_state())
        .max_by_key(|state| state.priority_rank())
        .unwrap_or(BlockerRadarEvidenceState::Unknown)
}

fn action_id(observation: &BlockerRadarCollectorObservation) -> String {
    format!(
        "action.{}.{}",
        action_fragment(observation.status.action_kind()),
        observation.source_id
    )
}

fn action_fragment(action_kind: BlockerRadarActionKind) -> &'static str {
    match action_kind {
        BlockerRadarActionKind::RecheckStatus => "recheck_status",
        BlockerRadarActionKind::InspectArtifact => "inspect_artifact",
        BlockerRadarActionKind::AddBeadsComment => "add_beads_comment",
        BlockerRadarActionKind::WaitForOwner => "wait_for_owner",
        BlockerRadarActionKind::ChooseReadyBead => "choose_ready_bead",
        BlockerRadarActionKind::RunBvRobotTriage => "run_bv_robot_triage",
        BlockerRadarActionKind::RunSwarmTick => "run_swarm_tick",
        BlockerRadarActionKind::FileFollowupBead => "file_followup_bead",
        BlockerRadarActionKind::None => "none",
    }
}

fn citation_id(source_id: &str) -> String {
    format!("citation.{source_id}")
}

fn forbidden_actions() -> Vec<BlockerRadarForbiddenAction> {
    [
        (
            "am service restart",
            "Agent Mail is a shared singleton and must not be restarted by this read-only radar",
        ),
        (
            "am doctor fix",
            "Agent Mail repair is outside blocker-radar collection",
        ),
        (
            "kill am",
            "Killing shared mail processes would disrupt other agents",
        ),
        (
            "rch daemon restart",
            "RCH rollout and restart require explicit operator approval",
        ),
        (
            "git reset --hard",
            "Destructive git commands require explicit user approval",
        ),
        (
            "git clean -fd",
            "Destructive filesystem cleanup requires explicit user approval",
        ),
    ]
    .into_iter()
    .map(|(command_pattern, reason)| BlockerRadarForbiddenAction {
        command_pattern: command_pattern.to_string(),
        reason: reason.to_string(),
    })
    .collect()
}

fn nonempty_reason_codes(values: &[String], fallback: &str) -> Vec<String> {
    let mut result = values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>();
    if result.is_empty() {
        result.push(fallback.to_string());
    }
    result
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
    if value.trim().is_empty() || values.contains(&value) {
        return;
    }
    values.push(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_input(observation: BlockerRadarCollectorObservation) -> BlockerRadarReport {
        build_blocker_radar_report(
            &BlockerRadarInput::new(1_770_000_000_001, "test.blocker_radar")
                .with_observation(observation),
        )
    }

    #[test]
    fn blocker_radar_pass_actionable_has_no_blockers() {
        let report = fixture_input(BlockerRadarCollectorObservation::new(
            "beads.ready",
            BlockerRadarSourceKind::Beads,
            BlockerRadarObservationStatus::PassActionable,
            "br ready --json",
            "ready bead exists and no owner conflict was reported",
        ));

        assert_eq!(report.contract_id, BLOCKER_RADAR_CONTRACT_ID);
        assert_eq!(report.overall_state, BlockerRadarEvidenceState::Actionable);
        assert!(report.blockers.is_empty());
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::ChooseReadyBead
        );
        assert!(!report.next_actions[0].mutation_allowed);
    }

    #[test]
    fn blocker_radar_ci_queued_uses_external_queue_row() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "ci.run.42",
                BlockerRadarSourceKind::GitHubActions,
                BlockerRadarObservationStatus::CiQueued,
                "gh run view --json status,conclusion,jobs",
                "GitHub Actions run is queued",
            )
            .with_run_id("42")
            .with_url("https://github.example/runs/42"),
        );

        assert_eq!(report.overall_state, BlockerRadarEvidenceState::CiQueued);
        assert_eq!(report.external_queues.len(), 1);
        assert_eq!(
            report.external_queues[0].substrate,
            BlockerRadarSubstrate::GitHubActions
        );
        assert_eq!(report.external_queues[0].run_id.as_deref(), Some("42"));
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::RecheckStatus
        );
    }

    #[test]
    fn blocker_radar_ci_zero_jobs_is_distinct_from_ci_queued() {
        let report = fixture_input(BlockerRadarCollectorObservation::new(
            "ci.suite.zero",
            BlockerRadarSourceKind::GitHubActions,
            BlockerRadarObservationStatus::CiZeroJobs,
            "gh run view --json jobs",
            "check suite has materialized zero jobs",
        ));

        assert_eq!(report.overall_state, BlockerRadarEvidenceState::CiZeroJobs);
        assert_eq!(
            report.blockers[0].evidence_state,
            BlockerRadarEvidenceState::CiZeroJobs
        );
    }

    #[test]
    fn blocker_radar_artifact_missing_uses_package_artifact_substrate() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "artifact.dist-macos-aarch64",
                BlockerRadarSourceKind::GitHubActions,
                BlockerRadarObservationStatus::ArtifactMissing,
                "gh run view --json artifacts",
                "required macOS package artifact is missing",
            )
            .with_artifact_name("dist-macos-aarch64"),
        );

        assert_eq!(
            report.overall_state,
            BlockerRadarEvidenceState::ArtifactMissing
        );
        assert_eq!(
            report.external_queues[0].substrate,
            BlockerRadarSubstrate::PackageArtifact
        );
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::InspectArtifact
        );
    }

    #[test]
    fn blocker_radar_rch_substrate_blocked_keeps_worker_and_followup_action() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "rch.build.298",
                BlockerRadarSourceKind::Rch,
                BlockerRadarObservationStatus::RchSubstrateBlocked,
                "rch status --workers --jobs --json",
                "RCH failed before Cargo reached a source verdict",
            )
            .with_worker_id("vmi1153651")
            .with_dependency_id("29837227949293785"),
        );

        assert_eq!(
            report.overall_state,
            BlockerRadarEvidenceState::RchSubstrateBlocked
        );
        assert_eq!(
            report.external_queues[0].worker_id.as_deref(),
            Some("vmi1153651")
        );
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::FileFollowupBead
        );
    }

    #[test]
    fn blocker_radar_local_fallback_refused_is_rch_substrate_blocked() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "rch.require_remote",
                BlockerRadarSourceKind::Rch,
                BlockerRadarObservationStatus::RchLocalFallbackRefused,
                "rch exec",
                "local fallback was refused by RCH_REQUIRE_REMOTE",
            )
            .with_reason_code("rch.require_remote"),
        );

        assert_eq!(
            report.sources[0].evidence_state,
            BlockerRadarEvidenceState::RchSubstrateBlocked
        );
        assert!(
            report.sources[0]
                .reason_codes
                .contains(&"rch.local_fallback_refused".to_string())
        );
    }

    #[test]
    fn blocker_radar_mail_unavailable_adds_unavailable_source_and_fallback_action() {
        let report = fixture_input(BlockerRadarCollectorObservation::new(
            "mail.health",
            BlockerRadarSourceKind::AgentMail,
            BlockerRadarObservationStatus::MailUnavailable,
            "scripts/swarm-tick.sh --agent-mail-fallback frankenterm",
            "Agent Mail health check failed and fallback snapshot is active",
        ));

        assert_eq!(
            report.overall_state,
            BlockerRadarEvidenceState::MailUnavailable
        );
        assert_eq!(report.unavailable_sources.len(), 1);
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::RunSwarmTick
        );
    }

    #[test]
    fn blocker_radar_stale_possible_does_not_reopen_owner_automatically() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "beads.owner.stale",
                BlockerRadarSourceKind::Beads,
                BlockerRadarObservationStatus::StalePossible,
                "br show ft-example",
                "owner may be stale but handoff is not proven",
            )
            .with_owner("BlueLake", Some(1_770_000_000_000))
            .with_dependency_id("ft-example"),
        );

        assert_eq!(
            report.overall_state,
            BlockerRadarEvidenceState::StalePossible
        );
        assert!(report.active_agents[0].stale_over_threshold);
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::AddBeadsComment
        );
        assert!(report.next_actions[0].suggested_command.is_none());
    }

    #[test]
    fn blocker_radar_active_not_stale_waits_for_owner() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "beads.owner.fresh",
                BlockerRadarSourceKind::Beads,
                BlockerRadarObservationStatus::ActiveOwnerFresh,
                "br show ft-active",
                "active owner updated inside the stale threshold",
            )
            .with_owner("GreenLake", Some(1_770_000_000_000))
            .with_dependency_id("ft-active"),
        );

        assert_eq!(
            report.overall_state,
            BlockerRadarEvidenceState::WaitingOwner
        );
        assert_eq!(report.active_agents[0].agent_name, "GreenLake");
        assert!(!report.active_agents[0].stale_over_threshold);
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::WaitForOwner
        );
    }

    #[test]
    fn blocker_radar_dirty_overlap_keeps_path_and_never_stages_it() {
        let report = fixture_input(
            BlockerRadarCollectorObservation::new(
                "git.status",
                BlockerRadarSourceKind::Git,
                BlockerRadarObservationStatus::DirtyOverlap,
                "git status --short --branch",
                "dirty tracked path overlaps another active lane",
            )
            .with_affected_path("crates/frankenterm-core/src/context_horizon.rs")
            .with_owner("other-agent", None)
            .with_dependency_id("ft-r920m.4"),
        );

        assert_eq!(
            report.overall_state,
            BlockerRadarEvidenceState::DirtyOverlap
        );
        assert_eq!(
            report.dirty_overlap[0].path,
            "crates/frankenterm-core/src/context_horizon.rs"
        );
        assert_eq!(
            report.next_actions[0].action_kind,
            BlockerRadarActionKind::WaitForOwner
        );
    }

    #[test]
    fn blocker_radar_degraded_and_empty_inputs_fail_closed() {
        let empty =
            build_blocker_radar_report(&BlockerRadarInput::new(1_770_000_000_001, "test.empty"));
        assert_eq!(empty.overall_state, BlockerRadarEvidenceState::Unknown);
        assert_eq!(empty.unavailable_sources.len(), 1);

        let degraded = fixture_input(BlockerRadarCollectorObservation::new(
            "gh.timeout",
            BlockerRadarSourceKind::GitHubActions,
            BlockerRadarObservationStatus::DegradedUnavailable,
            "gh run view --json status,conclusion,jobs",
            "GitHub CLI timed out before returning bounded status",
        ));
        assert_eq!(degraded.overall_state, BlockerRadarEvidenceState::Degraded);
        assert_eq!(degraded.unavailable_sources.len(), 1);
    }

    #[test]
    fn blocker_radar_serialization_forbids_raw_prompt_or_pane_content() {
        let report = fixture_input(BlockerRadarCollectorObservation::new(
            "fixture.privacy",
            BlockerRadarSourceKind::Fixture,
            BlockerRadarObservationStatus::PassActionable,
            "fixture",
            "bounded fixture evidence",
        ));
        let json = serde_json::to_value(&report).expect("blocker radar serializes");

        assert_eq!(json["raw_pane_content_stored"], false);
        assert_eq!(json["redaction_policy"]["raw_pane_content_allowed"], false);
        assert_eq!(json["redaction_policy"]["raw_prompt_allowed"], false);
        assert_eq!(json["citations"][0]["redacted"], true);
        assert_eq!(json["sources"][0]["redacted"], true);
    }
}
