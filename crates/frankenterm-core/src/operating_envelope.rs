//! Side-effect-free swarm operating-envelope planning.
//!
//! The planner consumes already-collected, redacted coordination facts and
//! returns ranked admission windows. It does not shell out, claim Beads, mutate
//! panes, repair services, cancel RCH work, run Cargo, or inspect pane text.

#![allow(clippy::similar_names)]

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[cfg(feature = "subprocess-bridge")]
use crate::beads_types::{BeadReadinessReport, BeadResolverReasonCode, BeadStatusCounts};

pub const OPERATING_ENVELOPE_CONTRACT_ID: &str = "ft.operating_envelope.v1";
pub const OPERATING_ENVELOPE_SCHEMA_VERSION: u16 = 1;
pub const OPERATING_ENVELOPE_MCP_CURRENT_URI: &str = "wa://operating-envelope/current";
pub const OPERATING_ENVELOPE_MCP_RUN_URI_TEMPLATE: &str = "wa://operating-envelope/runs/{run_id}";
pub const OPERATING_ENVELOPE_SURFACE_SAFETY_NOTICE: &str = "read-only plan; not permission to mutate panes, claim Beads, repair or restart services, \
     cancel RCH work, count local Cargo as proof, capture raw pane content, or drain workers";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperatingEnvelopeScenario {
    Current,
    Healthy,
    Degraded,
    Blocked,
    Emergency,
}

impl OperatingEnvelopeScenario {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Blocked => "blocked",
            Self::Emergency => "emergency",
        }
    }

    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "" | "current" | "current-unavailable" | "unavailable" => Some(Self::Current),
            "healthy" | "green" => Some(Self::Healthy),
            "degraded" | "agent-mail-unavailable" | "yellow" => Some(Self::Degraded),
            "blocked" | "rch-blocked" | "black" => Some(Self::Blocked),
            "emergency" | "kill-switch" | "capacity-black" => Some(Self::Emergency),
            _ => None,
        }
    }
}

impl Default for OperatingEnvelopeScenario {
    fn default() -> Self {
        Self::Current
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeSurface {
    Status,
    Explain,
}

impl OperatingEnvelopeSurface {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Explain => "explain",
        }
    }

    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "status" | "summary" => Some(Self::Status),
            "explain" | "reason" | "reason_code" => Some(Self::Explain),
            _ => None,
        }
    }
}

impl Default for OperatingEnvelopeSurface {
    fn default() -> Self {
        Self::Status
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeControllerMode {
    DryRun,
    AdmissionPreview,
    AttestationPreview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeTargetProfile {
    DeveloperLaptop,
    Target64Core256g,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeProofState {
    Measured,
    Simulated,
    Stale,
    Unavailable,
    SkippedNotProven,
    NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeCapacityPressureTier {
    Green,
    Yellow,
    Red,
    Black,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeSourceKind {
    CapacityResource,
    Rch,
    Beads,
    AgentMail,
    Git,
    RobotInventory,
    BlockerRadar,
    Manual,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeSourceState {
    Available,
    Degraded,
    Unavailable,
    Stale,
    Blocked,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeFreshnessState {
    Fresh,
    WithinBudget,
    Stale,
    Unknown,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeEvidenceLevel {
    LiveCommand,
    RetainedArtifact,
    Fixture,
    ManualNote,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeRedactionState {
    NotCollected,
    Redacted,
    RawForbidden,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeEvidenceCategory {
    CapacityPressure,
    TargetHardware,
    RchWorkerSelection,
    RchActiveProjectExclusion,
    RchTopologyPreflight,
    RchCargoReached,
    BeadsReadyQueue,
    BeadsBlockedQueue,
    BeadsInProgress,
    BeadsStaleCandidate,
    AgentMailAvailability,
    GitDirtyTree,
    GitUntrackedPaths,
    DeletionRisk,
    DirtyPathOverlap,
    RobotInventory,
    RedactionPosture,
    ManualNote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeEvidenceState {
    Pass,
    Warn,
    Fail,
    Blocked,
    Unavailable,
    Stale,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeOutcome {
    Admit,
    Defer,
    Degrade,
    Shed,
    Block,
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeTier {
    Green,
    Yellow,
    Orange,
    Red,
    Black,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeConfidence {
    Proven,
    Measured,
    Mixed,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeDirtyTreeState {
    Clean,
    DirtyNonOverlap,
    DirtyOverlap,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeWindowClass {
    Open,
    ReadOnly,
    StaticOnly,
    RchProofAllowed,
    ClaimAllowed,
    Defer,
    Wait,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingEnvelopeActionClass {
    ReadStatus,
    ClaimBead,
    AddBeadsComment,
    CreateBead,
    EditFiles,
    RunStaticCheck,
    RunRchProof,
    RequestApproval,
    Wait,
    AgentMailRepair,
    BuildCancellation,
    LocalCargoProof,
    RawPaneContent,
    RawPaneContentCapture,
    RchDaemonRestart,
    ServiceRestart,
    WorkerDrain,
    PaneMutation,
    ServiceMutation,
    DestructiveFilesystem,
    DestructiveGit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeTargetClass {
    pub profile: OperatingEnvelopeTargetProfile,
    pub requested_agent_count: u32,
    pub requested_cpu_cores: u32,
    pub requested_memory_gib: u32,
    pub proof_state: OperatingEnvelopeProofState,
    pub reason_codes: Vec<String>,
}

impl OperatingEnvelopeTargetClass {
    #[must_use]
    pub fn developer_laptop() -> Self {
        Self {
            profile: OperatingEnvelopeTargetProfile::DeveloperLaptop,
            requested_agent_count: 8,
            requested_cpu_cores: 8,
            requested_memory_gib: 32,
            proof_state: OperatingEnvelopeProofState::NotRequired,
            reason_codes: vec!["target_hardware.not_required".to_string()],
        }
    }

    #[must_use]
    pub fn target_64_core_256g() -> Self {
        Self {
            profile: OperatingEnvelopeTargetProfile::Target64Core256g,
            requested_agent_count: 64,
            requested_cpu_cores: 64,
            requested_memory_gib: 256,
            proof_state: OperatingEnvelopeProofState::SkippedNotProven,
            reason_codes: vec!["target_hardware.skipped_not_proven".to_string()],
        }
    }

    #[must_use]
    pub fn proof_state(mut self, proof_state: OperatingEnvelopeProofState) -> Self {
        self.proof_state = proof_state;
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeRedactionPosture {
    pub pane_content: OperatingEnvelopeRedactionState,
    pub secret_material: OperatingEnvelopeRedactionState,
    pub notes: Vec<String>,
}

impl Default for OperatingEnvelopeRedactionPosture {
    fn default() -> Self {
        Self {
            pane_content: OperatingEnvelopeRedactionState::NotCollected,
            secret_material: OperatingEnvelopeRedactionState::NotCollected,
            notes: vec!["source summary only".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeEvidenceItem {
    pub evidence_id: String,
    pub category: OperatingEnvelopeEvidenceCategory,
    pub state: OperatingEnvelopeEvidenceState,
    pub subject: String,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopeEvidenceItem {
    #[must_use]
    pub fn new(
        category: OperatingEnvelopeEvidenceCategory,
        state: OperatingEnvelopeEvidenceState,
        subject: impl Into<String>,
    ) -> Self {
        let subject = bounded_string(subject, "unknown");
        Self {
            evidence_id: format!("evidence-{}", slug(&subject)),
            category,
            state,
            subject,
            reason_codes: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, path: impl Into<String>) -> Self {
        push_unique(&mut self.artifact_paths, path);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeSourceSnapshot {
    pub source_id: String,
    pub source_kind: OperatingEnvelopeSourceKind,
    pub state: OperatingEnvelopeSourceState,
    pub freshness_state: OperatingEnvelopeFreshnessState,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub command_or_api: String,
    pub evidence_level: OperatingEnvelopeEvidenceLevel,
    pub redacted: bool,
    pub redaction_posture: OperatingEnvelopeRedactionPosture,
    pub raw_pane_content_stored: bool,
    pub reason_codes: Vec<String>,
    pub unavailable_reason: Option<String>,
    pub degraded_reason: Option<String>,
    pub evidence: Vec<OperatingEnvelopeEvidenceItem>,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopeSourceSnapshot {
    #[must_use]
    pub fn new(source_id: impl Into<String>, source_kind: OperatingEnvelopeSourceKind) -> Self {
        let source_id = bounded_string(source_id, "source.unknown");
        Self {
            source_id: source_id.clone(),
            source_kind,
            state: OperatingEnvelopeSourceState::Available,
            freshness_state: OperatingEnvelopeFreshnessState::Fresh,
            collected_at_ms: None,
            freshness_ms: None,
            command_or_api: "operating_envelope.core".to_string(),
            evidence_level: OperatingEnvelopeEvidenceLevel::ManualNote,
            redacted: true,
            redaction_posture: OperatingEnvelopeRedactionPosture::default(),
            raw_pane_content_stored: false,
            reason_codes: Vec::new(),
            unavailable_reason: None,
            degraded_reason: None,
            evidence: vec![OperatingEnvelopeEvidenceItem::new(
                OperatingEnvelopeEvidenceCategory::ManualNote,
                OperatingEnvelopeEvidenceState::Pass,
                source_id,
            )],
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn state(mut self, state: OperatingEnvelopeSourceState) -> Self {
        self.state = state;
        self
    }

    #[must_use]
    pub fn evidence_level(mut self, evidence_level: OperatingEnvelopeEvidenceLevel) -> Self {
        self.evidence_level = evidence_level;
        self
    }

    #[must_use]
    pub fn unavailable(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Unavailable;
        let reason_code = reason_code.into();
        push_unique(&mut self.reason_codes, reason_code.clone());
        self.unavailable_reason = Some(reason_code);
        self
    }

    #[must_use]
    pub fn degraded(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Degraded;
        let reason_code = reason_code.into();
        push_unique(&mut self.reason_codes, reason_code.clone());
        self.degraded_reason = Some(reason_code);
        self
    }

    #[must_use]
    pub fn blocked(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Blocked;
        let reason_code = reason_code.into();
        push_unique(&mut self.reason_codes, reason_code.clone());
        self.degraded_reason = Some(reason_code);
        self
    }

    #[must_use]
    pub fn stale(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::Stale;
        self.freshness_state = OperatingEnvelopeFreshnessState::Stale;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn not_collected(mut self, reason_code: impl Into<String>) -> Self {
        self.state = OperatingEnvelopeSourceState::NotCollected;
        self.freshness_state = OperatingEnvelopeFreshnessState::NotCollected;
        self.evidence_level = OperatingEnvelopeEvidenceLevel::NotCollected;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: OperatingEnvelopeEvidenceItem) -> Self {
        self.evidence.push(evidence);
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, path: impl Into<String>) -> Self {
        push_unique(&mut self.artifact_paths, path);
        self
    }

    #[must_use]
    pub fn raw_pane_content_stored(mut self) -> Self {
        self.raw_pane_content_stored = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeInputDomains {
    pub capacity_resource: OperatingEnvelopeSourceSnapshot,
    pub rch: OperatingEnvelopeSourceSnapshot,
    pub beads: OperatingEnvelopeSourceSnapshot,
    pub agent_mail: OperatingEnvelopeSourceSnapshot,
    pub git: OperatingEnvelopeSourceSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub robot_inventory: Option<OperatingEnvelopeSourceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeSourceProvenance {
    pub source_id: String,
    pub command_or_api: String,
    pub evidence_level: OperatingEnvelopeEvidenceLevel,
    pub collected_at_ms: Option<u64>,
    pub freshness_ms: Option<u64>,
    pub freshness_state: OperatingEnvelopeFreshnessState,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopeSourceProvenance {
    #[must_use]
    pub fn new(
        source_id: impl Into<String>,
        command_or_api: impl Into<String>,
        evidence_level: OperatingEnvelopeEvidenceLevel,
    ) -> Self {
        Self {
            source_id: bounded_string(source_id, "source.unknown"),
            command_or_api: bounded_string(command_or_api, "operating_envelope.adapter"),
            evidence_level,
            collected_at_ms: None,
            freshness_ms: None,
            freshness_state: OperatingEnvelopeFreshnessState::Unknown,
            artifact_paths: Vec::new(),
        }
    }

    #[must_use]
    pub fn retained_artifact(
        source_id: impl Into<String>,
        command_or_api: impl Into<String>,
    ) -> Self {
        Self::new(
            source_id,
            command_or_api,
            OperatingEnvelopeEvidenceLevel::RetainedArtifact,
        )
    }

    #[must_use]
    pub fn live_command(source_id: impl Into<String>, command_or_api: impl Into<String>) -> Self {
        Self::new(
            source_id,
            command_or_api,
            OperatingEnvelopeEvidenceLevel::LiveCommand,
        )
    }

    #[must_use]
    pub fn with_freshness(
        mut self,
        collected_at_ms: Option<u64>,
        freshness_ms: Option<u64>,
        freshness_state: OperatingEnvelopeFreshnessState,
    ) -> Self {
        self.collected_at_ms = collected_at_ms;
        self.freshness_ms = freshness_ms;
        self.freshness_state = freshness_state;
        self
    }

    #[must_use]
    pub fn with_artifact_path(mut self, path: impl Into<String>) -> Self {
        push_unique(
            &mut self.artifact_paths,
            bounded_string(path, "artifact.unknown"),
        );
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct OperatingEnvelopeBeadsSourceInput {
    pub provenance: OperatingEnvelopeSourceProvenance,
    pub ready_count: usize,
    pub blocked_count: usize,
    pub in_progress_count: usize,
    pub stale_candidate_count: usize,
    pub dependency_impact_count: usize,
    pub graph_cycle_count: usize,
    pub active_assignee_count: usize,
    pub dirty_overlap_risk: bool,
    pub partial_graph_data: bool,
    pub unavailable_reason: Option<String>,
}

impl OperatingEnvelopeBeadsSourceInput {
    #[must_use]
    pub fn new(provenance: OperatingEnvelopeSourceProvenance) -> Self {
        Self {
            provenance,
            ready_count: 0,
            blocked_count: 0,
            in_progress_count: 0,
            stale_candidate_count: 0,
            dependency_impact_count: 0,
            graph_cycle_count: 0,
            active_assignee_count: 0,
            dirty_overlap_risk: false,
            partial_graph_data: false,
            unavailable_reason: None,
        }
    }

    #[must_use]
    #[cfg(feature = "subprocess-bridge")]
    pub fn from_readiness_report(
        provenance: OperatingEnvelopeSourceProvenance,
        status_counts: &BeadStatusCounts,
        readiness: &BeadReadinessReport,
    ) -> Self {
        let graph_cycle_count = usize::from(
            readiness
                .degraded_reason_codes
                .contains(&BeadResolverReasonCode::CyclicDependencyGraph),
        );
        let partial_graph_data = readiness.degraded_reason_codes.iter().any(|reason| {
            matches!(
                reason,
                BeadResolverReasonCode::MissingDependencyNode
                    | BeadResolverReasonCode::PartialGraphData
            )
        });
        let dependency_impact_count = readiness
            .candidates
            .iter()
            .map(|candidate| candidate.transitive_unblock_count)
            .sum();

        Self {
            provenance,
            ready_count: readiness.ready_count(),
            blocked_count: status_counts.blocked,
            in_progress_count: status_counts.in_progress,
            stale_candidate_count: 0,
            dependency_impact_count,
            graph_cycle_count,
            active_assignee_count: 0,
            dirty_overlap_risk: false,
            partial_graph_data,
            unavailable_reason: None,
        }
    }

    #[must_use]
    pub fn to_source_snapshot(&self) -> OperatingEnvelopeSourceSnapshot {
        let mut snapshot = adapter_snapshot(&self.provenance, OperatingEnvelopeSourceKind::Beads);

        if let Some(reason) = normalized_optional_reason(self.unavailable_reason.as_deref()) {
            snapshot = snapshot.unavailable(reason.clone());
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsReadyQueue,
                OperatingEnvelopeEvidenceState::Unavailable,
                "beads unavailable",
                reason,
            ));
            return snapshot;
        }

        if self.ready_count == 0 {
            push_unique(&mut snapshot.reason_codes, "beads.ready_empty".to_string());
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsReadyQueue,
                OperatingEnvelopeEvidenceState::Warn,
                "ready=0",
                "beads.ready_empty",
            ));
        } else {
            push_unique(
                &mut snapshot.reason_codes,
                "beads.ready_available".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsReadyQueue,
                OperatingEnvelopeEvidenceState::Pass,
                format!("ready={}", self.ready_count),
                "beads.ready_available",
            ));
        }

        if self.blocked_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "beads.blocked_present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsBlockedQueue,
                OperatingEnvelopeEvidenceState::Warn,
                format!("blocked={}", self.blocked_count),
                "beads.blocked_present",
            ));
        }

        if self.in_progress_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "beads.in_progress_present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsInProgress,
                OperatingEnvelopeEvidenceState::Warn,
                format!("in_progress={}", self.in_progress_count),
                "beads.in_progress_present",
            ));
        }

        if self.active_assignee_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "assignee_overlap.active".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsInProgress,
                OperatingEnvelopeEvidenceState::Blocked,
                format!("active_assignees={}", self.active_assignee_count),
                "assignee_overlap.active",
            ));
        }

        if self.stale_candidate_count > 0 {
            snapshot = snapshot.stale("beads.stale_candidate");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsStaleCandidate,
                OperatingEnvelopeEvidenceState::Stale,
                format!("stale_candidates={}", self.stale_candidate_count),
                "beads.stale_candidate",
            ));
        }

        if self.dependency_impact_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "beads.dependency_impact_present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsBlockedQueue,
                OperatingEnvelopeEvidenceState::Warn,
                format!("dependency_impact={}", self.dependency_impact_count),
                "beads.dependency_impact_present",
            ));
        }

        if self.graph_cycle_count > 0 {
            snapshot = snapshot.degraded("beads.graph_cycles");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsBlockedQueue,
                OperatingEnvelopeEvidenceState::Fail,
                format!("graph_cycles={}", self.graph_cycle_count),
                "beads.graph_cycles",
            ));
        }

        if self.partial_graph_data {
            snapshot = snapshot.degraded("beads.partial_graph_data");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::BeadsBlockedQueue,
                OperatingEnvelopeEvidenceState::Warn,
                "partial graph data",
                "beads.partial_graph_data",
            ));
        }

        if self.dirty_overlap_risk {
            snapshot = snapshot.blocked("dirty_overlap.present");
            push_unique(
                &mut snapshot.reason_codes,
                "beads.dirty_overlap_risk".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::DirtyPathOverlap,
                OperatingEnvelopeEvidenceState::Blocked,
                "beads dirty overlap",
                "dirty_overlap.present",
            ));
        }

        snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct OperatingEnvelopeRchSourceInput {
    pub provenance: OperatingEnvelopeSourceProvenance,
    pub healthy_worker_count: usize,
    pub active_build_count: usize,
    pub queued_build_count: usize,
    pub no_workers_passed_health: bool,
    pub active_project_exclusion: bool,
    pub topology_preflight_failed: bool,
    pub remote_cargo_reached: Option<bool>,
    pub remote_cargo_verdict_pass: bool,
    pub critical_pressure: bool,
    pub unavailable_reason: Option<String>,
}

impl OperatingEnvelopeRchSourceInput {
    #[must_use]
    pub fn new(provenance: OperatingEnvelopeSourceProvenance) -> Self {
        Self {
            provenance,
            healthy_worker_count: 0,
            active_build_count: 0,
            queued_build_count: 0,
            no_workers_passed_health: false,
            active_project_exclusion: false,
            topology_preflight_failed: false,
            remote_cargo_reached: None,
            remote_cargo_verdict_pass: false,
            critical_pressure: false,
            unavailable_reason: None,
        }
    }

    #[must_use]
    pub fn to_source_snapshot(&self) -> OperatingEnvelopeSourceSnapshot {
        let mut snapshot = adapter_snapshot(&self.provenance, OperatingEnvelopeSourceKind::Rch);

        if let Some(reason) = normalized_optional_reason(self.unavailable_reason.as_deref()) {
            snapshot = snapshot.unavailable(reason.clone());
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchWorkerSelection,
                OperatingEnvelopeEvidenceState::Unavailable,
                "rch unavailable",
                reason,
            ));
            return snapshot;
        }

        if self.healthy_worker_count > 0 {
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchWorkerSelection,
                OperatingEnvelopeEvidenceState::Pass,
                format!("healthy_workers={}", self.healthy_worker_count),
                "rch.workers_healthy",
            ));
        }

        if self.active_build_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "rch.active_builds_present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchWorkerSelection,
                OperatingEnvelopeEvidenceState::Warn,
                format!("active_builds={}", self.active_build_count),
                "rch.active_builds_present",
            ));
        }

        if self.queued_build_count > 0 {
            push_unique(&mut snapshot.reason_codes, "rch.queue_present".to_string());
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchWorkerSelection,
                OperatingEnvelopeEvidenceState::Warn,
                format!("queued_builds={}", self.queued_build_count),
                "rch.queue_present",
            ));
        }

        if self.no_workers_passed_health {
            snapshot = snapshot.blocked("rch.no_workers_passed_health");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchWorkerSelection,
                OperatingEnvelopeEvidenceState::Blocked,
                "worker=null",
                "rch.no_workers_passed_health",
            ));
        }

        if self.critical_pressure {
            snapshot = snapshot.blocked("rch.no_admissible_workers.critical_pressure");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchWorkerSelection,
                OperatingEnvelopeEvidenceState::Blocked,
                "critical pressure",
                "rch.no_admissible_workers.critical_pressure",
            ));
        }

        if self.active_project_exclusion {
            snapshot = snapshot.blocked("rch.active_project_exclusion");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchActiveProjectExclusion,
                OperatingEnvelopeEvidenceState::Blocked,
                "same project active",
                "rch.active_project_exclusion",
            ));
        }

        if self.topology_preflight_failed {
            snapshot = snapshot.blocked("rch.topology_preflight_failed");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::RchTopologyPreflight,
                OperatingEnvelopeEvidenceState::Fail,
                "topology preflight",
                "rch.topology_preflight_failed",
            ));
        }

        match self.remote_cargo_reached {
            Some(true) => {
                push_unique(
                    &mut snapshot.reason_codes,
                    "rch.remote_cargo_reached_true".to_string(),
                );
                snapshot.evidence.push(evidence(
                    OperatingEnvelopeEvidenceCategory::RchCargoReached,
                    OperatingEnvelopeEvidenceState::Pass,
                    "remote Cargo reached",
                    "rch.remote_cargo_reached_true",
                ));
            }
            Some(false) => {
                snapshot = snapshot.degraded("proof.insufficient");
                push_unique(
                    &mut snapshot.reason_codes,
                    "rch.remote_cargo_reached_false".to_string(),
                );
                snapshot.evidence.push(evidence(
                    OperatingEnvelopeEvidenceCategory::RchCargoReached,
                    OperatingEnvelopeEvidenceState::Fail,
                    "remote Cargo not reached",
                    "rch.remote_cargo_reached_false",
                ));
            }
            None => {
                snapshot = snapshot.degraded("proof.insufficient");
                push_unique(
                    &mut snapshot.reason_codes,
                    "rch.remote_cargo_reached_unknown".to_string(),
                );
                snapshot.evidence.push(
                    OperatingEnvelopeEvidenceItem::new(
                        OperatingEnvelopeEvidenceCategory::RchCargoReached,
                        OperatingEnvelopeEvidenceState::Unknown,
                        "remote Cargo reach unknown",
                    )
                    .with_reason_code("rch.remote_cargo_reached_unknown"),
                );
            }
        }

        if self.remote_cargo_verdict_pass {
            push_unique(
                &mut snapshot.reason_codes,
                "rch.cargo_verdict.pass".to_string(),
            );
        }

        snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct OperatingEnvelopeAgentMailSourceInput {
    pub provenance: OperatingEnvelopeSourceProvenance,
    pub available: bool,
    pub degraded: bool,
    pub database_error: bool,
    pub ack_required_count: usize,
    pub unread_count: usize,
    pub forbidden_service_remediation_seen: bool,
    pub unavailable_reason: Option<String>,
}

impl OperatingEnvelopeAgentMailSourceInput {
    #[must_use]
    pub fn new(provenance: OperatingEnvelopeSourceProvenance) -> Self {
        Self {
            provenance,
            available: true,
            degraded: false,
            database_error: false,
            ack_required_count: 0,
            unread_count: 0,
            forbidden_service_remediation_seen: false,
            unavailable_reason: None,
        }
    }

    #[must_use]
    pub fn to_source_snapshot(&self) -> OperatingEnvelopeSourceSnapshot {
        let mut snapshot =
            adapter_snapshot(&self.provenance, OperatingEnvelopeSourceKind::AgentMail);

        if self.database_error {
            snapshot = snapshot.unavailable("agent_mail.database_error");
            push_unique(
                &mut snapshot.reason_codes,
                "agent_mail.unavailable_after_retry".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::AgentMailAvailability,
                OperatingEnvelopeEvidenceState::Unavailable,
                "database durability error",
                "agent_mail.database_error",
            ));
        } else if let Some(reason) = normalized_optional_reason(self.unavailable_reason.as_deref())
        {
            snapshot = snapshot.unavailable(reason.clone());
            push_unique(
                &mut snapshot.reason_codes,
                "agent_mail.unavailable_after_retry".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::AgentMailAvailability,
                OperatingEnvelopeEvidenceState::Unavailable,
                "agent mail unavailable",
                reason,
            ));
        } else if self.degraded || !self.available {
            snapshot = snapshot.degraded("agent_mail.degraded");
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::AgentMailAvailability,
                OperatingEnvelopeEvidenceState::Warn,
                "agent mail degraded",
                "agent_mail.degraded",
            ));
        } else {
            push_unique(
                &mut snapshot.reason_codes,
                "agent_mail.available".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::AgentMailAvailability,
                OperatingEnvelopeEvidenceState::Pass,
                "agent mail available",
                "agent_mail.available",
            ));
        }

        if self.ack_required_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "agent_mail.ack_required_present".to_string(),
            );
        }
        if self.unread_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "agent_mail.unread_present".to_string(),
            );
        }
        if self.forbidden_service_remediation_seen {
            push_unique(
                &mut snapshot.reason_codes,
                "agent_mail.service_remediation_forbidden".to_string(),
            );
        }

        snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct OperatingEnvelopeGitSourceInput {
    pub provenance: OperatingEnvelopeSourceProvenance,
    pub branch_name: String,
    pub tracked_dirty_count: usize,
    pub untracked_count: usize,
    pub deleted_path_count: usize,
    pub deleted_frankenterm_core_path_count: usize,
    pub dirty_overlap_count: usize,
    pub shared_tracker_dirty: bool,
    pub unavailable_reason: Option<String>,
}

impl OperatingEnvelopeGitSourceInput {
    #[must_use]
    pub fn new(
        provenance: OperatingEnvelopeSourceProvenance,
        branch_name: impl Into<String>,
    ) -> Self {
        Self {
            provenance,
            branch_name: bounded_string(branch_name, "unknown"),
            tracked_dirty_count: 0,
            untracked_count: 0,
            deleted_path_count: 0,
            deleted_frankenterm_core_path_count: 0,
            dirty_overlap_count: 0,
            shared_tracker_dirty: false,
            unavailable_reason: None,
        }
    }

    #[must_use]
    pub fn to_source_snapshot(&self) -> OperatingEnvelopeSourceSnapshot {
        let mut snapshot = adapter_snapshot(&self.provenance, OperatingEnvelopeSourceKind::Git);

        if let Some(reason) = normalized_optional_reason(self.unavailable_reason.as_deref()) {
            snapshot = snapshot.unavailable(reason.clone());
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::GitDirtyTree,
                OperatingEnvelopeEvidenceState::Unavailable,
                "git unavailable",
                reason,
            ));
            return snapshot;
        }

        snapshot.evidence.push(OperatingEnvelopeEvidenceItem::new(
            OperatingEnvelopeEvidenceCategory::ManualNote,
            OperatingEnvelopeEvidenceState::Pass,
            format!("branch={}", self.branch_name),
        ));

        if self.deleted_frankenterm_core_path_count > 0 {
            snapshot = snapshot.blocked("deletion_risk.frankenterm_core");
            push_unique(
                &mut snapshot.reason_codes,
                "deletion_risk.present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::DeletionRisk,
                OperatingEnvelopeEvidenceState::Blocked,
                format!(
                    "deleted_frankenterm_core_paths={}",
                    self.deleted_frankenterm_core_path_count
                ),
                "deletion_risk.frankenterm_core",
            ));
        } else if self.deleted_path_count > 0 {
            snapshot = snapshot.blocked("git.deleted_paths_present");
            push_unique(
                &mut snapshot.reason_codes,
                "deletion_risk.present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::DeletionRisk,
                OperatingEnvelopeEvidenceState::Blocked,
                format!("deleted_paths={}", self.deleted_path_count),
                "git.deleted_paths_present",
            ));
        }

        if self.dirty_overlap_count > 0 {
            snapshot = snapshot.blocked("dirty_overlap.present");
            push_unique(
                &mut snapshot.reason_codes,
                "git.dirty_overlap_risk".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::DirtyPathOverlap,
                OperatingEnvelopeEvidenceState::Blocked,
                format!("dirty_overlap_paths={}", self.dirty_overlap_count),
                "dirty_overlap.present",
            ));
        }

        if self.tracked_dirty_count > 0 || self.shared_tracker_dirty {
            push_unique(
                &mut snapshot.reason_codes,
                "git.dirty_non_overlap".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::GitDirtyTree,
                OperatingEnvelopeEvidenceState::Warn,
                format!("tracked_dirty={}", self.tracked_dirty_count),
                "git.dirty_non_overlap",
            ));
        }

        if self.shared_tracker_dirty {
            push_unique(
                &mut snapshot.reason_codes,
                "git.shared_tracker_dirty".to_string(),
            );
        }

        if self.untracked_count > 0 {
            push_unique(
                &mut snapshot.reason_codes,
                "git.untracked_paths_present".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::GitUntrackedPaths,
                OperatingEnvelopeEvidenceState::Warn,
                format!("untracked={}", self.untracked_count),
                "git.untracked_paths_present",
            ));
        }

        if snapshot.reason_codes.is_empty() {
            push_unique(
                &mut snapshot.reason_codes,
                "git.clean_for_scope".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::GitDirtyTree,
                OperatingEnvelopeEvidenceState::Pass,
                "clean",
                "git.clean_for_scope",
            ));
        }

        snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeCapacitySourceInput {
    pub provenance: OperatingEnvelopeSourceProvenance,
    pub pressure_tier: OperatingEnvelopeCapacityPressureTier,
    pub target_class_proof_state: OperatingEnvelopeProofState,
    pub kill_switch_active: bool,
    pub unavailable_reason: Option<String>,
}

impl OperatingEnvelopeCapacitySourceInput {
    #[must_use]
    pub fn new(provenance: OperatingEnvelopeSourceProvenance) -> Self {
        Self {
            provenance,
            pressure_tier: OperatingEnvelopeCapacityPressureTier::Unknown,
            target_class_proof_state: OperatingEnvelopeProofState::Unavailable,
            kill_switch_active: false,
            unavailable_reason: None,
        }
    }

    #[must_use]
    pub fn to_source_snapshot(&self) -> OperatingEnvelopeSourceSnapshot {
        let mut snapshot = adapter_snapshot(
            &self.provenance,
            OperatingEnvelopeSourceKind::CapacityResource,
        );

        if let Some(reason) = normalized_optional_reason(self.unavailable_reason.as_deref()) {
            snapshot = snapshot.unavailable(reason.clone());
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::CapacityPressure,
                OperatingEnvelopeEvidenceState::Unavailable,
                "capacity unavailable",
                reason,
            ));
            return snapshot;
        }

        let (pressure_reason, pressure_state) = match self.pressure_tier {
            OperatingEnvelopeCapacityPressureTier::Green => {
                ("capacity.green", OperatingEnvelopeEvidenceState::Pass)
            }
            OperatingEnvelopeCapacityPressureTier::Yellow => {
                ("capacity.yellow", OperatingEnvelopeEvidenceState::Warn)
            }
            OperatingEnvelopeCapacityPressureTier::Red => {
                ("capacity.red", OperatingEnvelopeEvidenceState::Fail)
            }
            OperatingEnvelopeCapacityPressureTier::Black => {
                ("capacity.black", OperatingEnvelopeEvidenceState::Blocked)
            }
            OperatingEnvelopeCapacityPressureTier::Unknown => (
                "capacity.pressure_unknown",
                OperatingEnvelopeEvidenceState::Unknown,
            ),
        };
        push_unique(&mut snapshot.reason_codes, pressure_reason.to_string());
        if self.pressure_tier == OperatingEnvelopeCapacityPressureTier::Unknown {
            snapshot = snapshot.degraded("capacity.pressure_unknown");
            push_unique(&mut snapshot.reason_codes, "telemetry.stale".to_string());
        }
        snapshot.evidence.push(evidence(
            OperatingEnvelopeEvidenceCategory::CapacityPressure,
            pressure_state,
            format!("pressure={:?}", self.pressure_tier),
            pressure_reason,
        ));

        if self.kill_switch_active {
            snapshot = snapshot.blocked("capacity.kill_switch_active");
            push_unique(
                &mut snapshot.reason_codes,
                "policy.kill_switch_active".to_string(),
            );
            snapshot.evidence.push(evidence(
                OperatingEnvelopeEvidenceCategory::CapacityPressure,
                OperatingEnvelopeEvidenceState::Blocked,
                "kill switch active",
                "capacity.kill_switch_active",
            ));
        }

        match self.target_class_proof_state {
            OperatingEnvelopeProofState::Measured | OperatingEnvelopeProofState::NotRequired => {
                snapshot.evidence.push(evidence(
                    OperatingEnvelopeEvidenceCategory::TargetHardware,
                    OperatingEnvelopeEvidenceState::Pass,
                    "target class proof available",
                    "target_hardware.proven_or_not_required",
                ));
            }
            OperatingEnvelopeProofState::SkippedNotProven
            | OperatingEnvelopeProofState::Unavailable => {
                push_unique(
                    &mut snapshot.reason_codes,
                    "capacity.target_class_unproven".to_string(),
                );
                snapshot.evidence.push(evidence(
                    OperatingEnvelopeEvidenceCategory::TargetHardware,
                    OperatingEnvelopeEvidenceState::Unavailable,
                    "target class proof unavailable",
                    "capacity.target_class_unproven",
                ));
            }
            OperatingEnvelopeProofState::Simulated | OperatingEnvelopeProofState::Stale => {
                push_unique(
                    &mut snapshot.reason_codes,
                    "target_hardware.skipped_not_proven".to_string(),
                );
                snapshot.evidence.push(evidence(
                    OperatingEnvelopeEvidenceCategory::TargetHardware,
                    OperatingEnvelopeEvidenceState::Warn,
                    "target class proof not measured",
                    "target_hardware.skipped_not_proven",
                ));
            }
        }

        snapshot
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeSourceAdapterInputs {
    pub capacity_resource: OperatingEnvelopeCapacitySourceInput,
    pub rch: OperatingEnvelopeRchSourceInput,
    pub beads: OperatingEnvelopeBeadsSourceInput,
    pub agent_mail: OperatingEnvelopeAgentMailSourceInput,
    pub git: OperatingEnvelopeGitSourceInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub robot_inventory: Option<OperatingEnvelopeSourceSnapshot>,
}

impl OperatingEnvelopeSourceAdapterInputs {
    #[must_use]
    pub fn new(
        capacity_resource: OperatingEnvelopeCapacitySourceInput,
        rch: OperatingEnvelopeRchSourceInput,
        beads: OperatingEnvelopeBeadsSourceInput,
        agent_mail: OperatingEnvelopeAgentMailSourceInput,
        git: OperatingEnvelopeGitSourceInput,
    ) -> Self {
        Self {
            capacity_resource,
            rch,
            beads,
            agent_mail,
            git,
            robot_inventory: None,
        }
    }

    #[must_use]
    pub fn with_robot_inventory(
        mut self,
        robot_inventory: OperatingEnvelopeSourceSnapshot,
    ) -> Self {
        self.robot_inventory = Some(robot_inventory);
        self
    }

    #[must_use]
    pub fn into_input_domains(self) -> OperatingEnvelopeInputDomains {
        OperatingEnvelopeInputDomains {
            capacity_resource: self.capacity_resource.to_source_snapshot(),
            rch: self.rch.to_source_snapshot(),
            beads: self.beads.to_source_snapshot(),
            agent_mail: self.agent_mail.to_source_snapshot(),
            git: self.git.to_source_snapshot(),
            robot_inventory: self.robot_inventory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeBudgets {
    pub interactive_agents: u32,
    pub long_rch_proof_lanes: u32,
    pub gui_renderer_stress_jobs: u32,
    pub docs_static_checks: u32,
    pub no_mock_e2e_harnesses: u32,
}

impl OperatingEnvelopeBudgets {
    #[must_use]
    pub const fn conservative() -> Self {
        Self {
            interactive_agents: 4,
            long_rch_proof_lanes: 1,
            gui_renderer_stress_jobs: 0,
            docs_static_checks: 2,
            no_mock_e2e_harnesses: 0,
        }
    }

    #[must_use]
    pub const fn target_class() -> Self {
        Self {
            interactive_agents: 32,
            long_rch_proof_lanes: 8,
            gui_renderer_stress_jobs: 2,
            docs_static_checks: 8,
            no_mock_e2e_harnesses: 2,
        }
    }
}

impl Default for OperatingEnvelopeBudgets {
    fn default() -> Self {
        Self::conservative()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopePlannerInput {
    pub generated_at_ms: u64,
    pub envelope_id: String,
    pub objective_id: String,
    pub controller_mode: OperatingEnvelopeControllerMode,
    pub target_class: OperatingEnvelopeTargetClass,
    pub input_domains: OperatingEnvelopeInputDomains,
    pub budgets: OperatingEnvelopeBudgets,
    pub artifact_paths: Vec<String>,
}

impl OperatingEnvelopePlannerInput {
    #[must_use]
    pub fn new(
        generated_at_ms: u64,
        envelope_id: impl Into<String>,
        objective_id: impl Into<String>,
        input_domains: OperatingEnvelopeInputDomains,
    ) -> Self {
        Self {
            generated_at_ms,
            envelope_id: bounded_string(envelope_id, "envelope.unknown"),
            objective_id: bounded_string(objective_id, "objective.unknown"),
            controller_mode: OperatingEnvelopeControllerMode::DryRun,
            target_class: OperatingEnvelopeTargetClass::developer_laptop(),
            input_domains,
            budgets: OperatingEnvelopeBudgets::default(),
            artifact_paths: vec!["docs/json-schema/ft-operating-envelope.json".to_string()],
        }
    }

    #[must_use]
    pub fn target_class(mut self, target_class: OperatingEnvelopeTargetClass) -> Self {
        self.target_class = target_class;
        self
    }

    #[must_use]
    pub fn budgets(mut self, budgets: OperatingEnvelopeBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    fn sources(&self) -> Vec<&OperatingEnvelopeSourceSnapshot> {
        let mut sources = vec![
            &self.input_domains.capacity_resource,
            &self.input_domains.rch,
            &self.input_domains.beads,
            &self.input_domains.agent_mail,
            &self.input_domains.git,
        ];
        if let Some(robot_inventory) = &self.input_domains.robot_inventory {
            sources.push(robot_inventory);
        }
        sources
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeDecision {
    pub decision_id: String,
    pub outcome: OperatingEnvelopeOutcome,
    pub envelope_tier: OperatingEnvelopeTier,
    pub confidence: OperatingEnvelopeConfidence,
    pub target_hardware_state: OperatingEnvelopeProofState,
    pub rch_proof_state: OperatingEnvelopeProofState,
    pub agent_mail_state: OperatingEnvelopeSourceState,
    pub dirty_tree_state: OperatingEnvelopeDirtyTreeState,
    pub max_parallel_agents: u32,
    pub max_parallel_proofs: u32,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeAdmissionWindow {
    pub window_id: String,
    pub window_class: OperatingEnvelopeWindowClass,
    pub starts_at_ms: Option<u64>,
    pub expires_at_ms: Option<u64>,
    pub max_parallel_agents: u32,
    pub max_parallel_proofs: u32,
    pub permitted_action_classes: Vec<OperatingEnvelopeActionClass>,
    pub forbidden_action_classes: Vec<OperatingEnvelopeActionClass>,
    pub source_ids: Vec<String>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeFailClosedPolicy {
    pub missing_signal_behavior: String,
    pub stale_signal_behavior: String,
    pub contradictory_signal_behavior: String,
    pub privacy_redaction_behavior: String,
    pub target_hardware_gap_behavior: String,
    pub reason_codes: Vec<String>,
}

impl Default for OperatingEnvelopeFailClosedPolicy {
    fn default() -> Self {
        Self {
            missing_signal_behavior: "lower_envelope".to_string(),
            stale_signal_behavior: "lower_envelope".to_string(),
            contradictory_signal_behavior: "block".to_string(),
            privacy_redaction_behavior: "lower_envelope".to_string(),
            target_hardware_gap_behavior: "defer_target_class_claim".to_string(),
            reason_codes: vec![
                "fail_closed.lower_missing".to_string(),
                "fail_closed.block_contradiction".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct OperatingEnvelopeSideEffectPolicy {
    pub dry_run_only: bool,
    pub raw_pane_content_allowed: bool,
    pub pane_mutation_allowed: bool,
    pub service_mutation_allowed: bool,
    pub destructive_actions_allowed: bool,
    pub local_cargo_proof_allowed: bool,
}

impl Default for OperatingEnvelopeSideEffectPolicy {
    fn default() -> Self {
        Self {
            dry_run_only: true,
            raw_pane_content_allowed: false,
            pane_mutation_allowed: false,
            service_mutation_allowed: false,
            destructive_actions_allowed: false,
            local_cargo_proof_allowed: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopeRedactionPolicy {
    pub policy_id: String,
    pub raw_pane_content_allowed: bool,
    pub secret_material_allowed: bool,
    pub notes: Vec<String>,
}

impl Default for OperatingEnvelopeRedactionPolicy {
    fn default() -> Self {
        Self {
            policy_id: "ft.redaction.operating_envelope.v1".to_string(),
            raw_pane_content_allowed: false,
            secret_material_allowed: false,
            notes: vec!["Envelope stores source summaries and reason codes only.".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingEnvelopePlan {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub envelope_id: String,
    pub objective_id: String,
    pub controller_mode: OperatingEnvelopeControllerMode,
    pub target_class: OperatingEnvelopeTargetClass,
    pub input_domains: OperatingEnvelopeInputDomains,
    pub decision: OperatingEnvelopeDecision,
    pub admission_windows: Vec<OperatingEnvelopeAdmissionWindow>,
    pub fail_closed_policy: OperatingEnvelopeFailClosedPolicy,
    pub side_effect_policy: OperatingEnvelopeSideEffectPolicy,
    pub redaction_policy: OperatingEnvelopeRedactionPolicy,
    pub raw_pane_content_stored: bool,
    pub reason_codes: Vec<String>,
    pub artifact_paths: Vec<String>,
}

#[must_use]
pub fn plan_operating_envelope(input: OperatingEnvelopePlannerInput) -> OperatingEnvelopePlan {
    let facts = EnvelopeFacts::from_input(&input);
    let decision = decision_for(&input, &facts);
    let mut admission_windows = admission_windows_for(&input, &facts, &decision);
    stable_rank_windows(&mut admission_windows);

    OperatingEnvelopePlan {
        schema_version: OPERATING_ENVELOPE_SCHEMA_VERSION,
        contract_id: OPERATING_ENVELOPE_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        envelope_id: input.envelope_id,
        objective_id: input.objective_id,
        controller_mode: input.controller_mode,
        target_class: input.target_class,
        input_domains: input.input_domains,
        decision: decision.clone(),
        admission_windows,
        fail_closed_policy: OperatingEnvelopeFailClosedPolicy::default(),
        side_effect_policy: OperatingEnvelopeSideEffectPolicy::default(),
        redaction_policy: OperatingEnvelopeRedactionPolicy::default(),
        raw_pane_content_stored: false,
        reason_codes: decision.reason_codes,
        artifact_paths: input.artifact_paths,
    }
}

#[must_use]
pub fn operating_envelope_run_id(generated_at_ms: u64) -> String {
    format!("operating-envelope-{generated_at_ms}")
}

#[must_use]
pub fn operating_envelope_run_uri(run_id: &str) -> String {
    format!("wa://operating-envelope/runs/{run_id}")
}

#[must_use]
pub fn operating_envelope_mcp_resources(generated_at_ms: u64) -> serde_json::Value {
    let run_id = operating_envelope_run_id(generated_at_ms);
    serde_json::json!([
        {
            "kind": "current",
            "uri": OPERATING_ENVELOPE_MCP_CURRENT_URI,
            "implemented": true,
            "read_only": true,
            "live_mutation_allowed": false,
            "mime_type": "application/json",
        },
        {
            "kind": "run_artifact",
            "uri": operating_envelope_run_uri(&run_id),
            "uri_template": OPERATING_ENVELOPE_MCP_RUN_URI_TEMPLATE,
            "run_id": run_id,
            "implemented": true,
            "read_only": true,
            "live_mutation_allowed": false,
            "mime_type": "application/json",
        }
    ])
}

#[must_use]
pub fn operating_envelope_input_for_scenario(
    generated_at_ms: u64,
    envelope_id: impl Into<String>,
    objective_id: impl Into<String>,
    scenario: OperatingEnvelopeScenario,
) -> OperatingEnvelopePlannerInput {
    let input_domains = match scenario {
        OperatingEnvelopeScenario::Current => current_unavailable_domains(generated_at_ms),
        OperatingEnvelopeScenario::Healthy => scenario_domains(generated_at_ms, scenario),
        OperatingEnvelopeScenario::Degraded => scenario_domains(generated_at_ms, scenario),
        OperatingEnvelopeScenario::Blocked => scenario_domains(generated_at_ms, scenario),
        OperatingEnvelopeScenario::Emergency => scenario_domains(generated_at_ms, scenario),
    };

    OperatingEnvelopePlannerInput::new(generated_at_ms, envelope_id, objective_id, input_domains)
}

#[must_use]
pub fn build_operating_envelope_surface_data(
    plan: &OperatingEnvelopePlan,
    surface: OperatingEnvelopeSurface,
    source: &str,
    explain_reason_code: Option<&str>,
) -> serde_json::Value {
    let explain = (surface == OperatingEnvelopeSurface::Explain)
        .then(|| operating_envelope_explain(plan, explain_reason_code.and_then(non_empty_trimmed)));

    serde_json::json!({
        "schema_version": plan.schema_version,
        "contract_id": plan.contract_id,
        "surface": surface.as_str(),
        "generated_at_ms": plan.generated_at_ms,
        "source": source,
        "dry_run": true,
        "raw_pane_content_stored": plan.raw_pane_content_stored,
        "live_mutation_allowed": false,
        "side_effects_executed": false,
        "safety_notice": OPERATING_ENVELOPE_SURFACE_SAFETY_NOTICE,
        "summary": operating_envelope_summary(plan),
        "decision": plan.decision,
        "admission_windows": plan.admission_windows,
        "mcp_resources": operating_envelope_mcp_resources(plan.generated_at_ms),
        "source_states": operating_envelope_source_states(plan),
        "proof_requirements": operating_envelope_proof_requirements(plan),
        "required_approvals": operating_envelope_required_approvals(),
        "forbidden_action_classes": operating_envelope_forbidden_action_classes(plan),
        "explain": explain,
    })
}

fn non_empty_trimmed(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn current_unavailable_domains(generated_at_ms: u64) -> OperatingEnvelopeInputDomains {
    OperatingEnvelopeInputDomains {
        capacity_resource: not_collected_snapshot(
            "current.capacity_resource",
            OperatingEnvelopeSourceKind::CapacityResource,
            "capacity.current_unavailable",
            generated_at_ms,
        ),
        rch: not_collected_snapshot(
            "current.rch",
            OperatingEnvelopeSourceKind::Rch,
            "rch.current_unavailable",
            generated_at_ms,
        ),
        beads: not_collected_snapshot(
            "current.beads",
            OperatingEnvelopeSourceKind::Beads,
            "beads.current_unavailable",
            generated_at_ms,
        ),
        agent_mail: not_collected_snapshot(
            "current.agent_mail",
            OperatingEnvelopeSourceKind::AgentMail,
            "agent_mail.current_unavailable",
            generated_at_ms,
        ),
        git: not_collected_snapshot(
            "current.git",
            OperatingEnvelopeSourceKind::Git,
            "git.current_unavailable",
            generated_at_ms,
        ),
        robot_inventory: None,
    }
}

fn not_collected_snapshot(
    source_id: &'static str,
    source_kind: OperatingEnvelopeSourceKind,
    reason_code: &'static str,
    generated_at_ms: u64,
) -> OperatingEnvelopeSourceSnapshot {
    let mut snapshot =
        OperatingEnvelopeSourceSnapshot::new(source_id, source_kind).not_collected(reason_code);
    snapshot.command_or_api = "ft swarm envelope current".to_string();
    snapshot.collected_at_ms = Some(generated_at_ms);
    snapshot
}

fn scenario_domains(
    generated_at_ms: u64,
    scenario: OperatingEnvelopeScenario,
) -> OperatingEnvelopeInputDomains {
    let mut capacity = OperatingEnvelopeCapacitySourceInput::new(fixture_provenance(
        "fixture.capacity_resource",
        "ft robot swarm-capacity status --level 2",
        generated_at_ms,
    ));
    capacity.pressure_tier = OperatingEnvelopeCapacityPressureTier::Green;
    capacity.target_class_proof_state = OperatingEnvelopeProofState::NotRequired;

    let mut rch = OperatingEnvelopeRchSourceInput::new(fixture_provenance(
        "fixture.rch",
        "rch status --json",
        generated_at_ms,
    ));
    rch.healthy_worker_count = 2;
    rch.remote_cargo_reached = Some(true);
    rch.remote_cargo_verdict_pass = true;

    let mut beads = OperatingEnvelopeBeadsSourceInput::new(fixture_provenance(
        "fixture.beads",
        "bv --robot-triage",
        generated_at_ms,
    ));
    beads.ready_count = 4;

    let mut agent_mail = OperatingEnvelopeAgentMailSourceInput::new(fixture_provenance(
        "fixture.agent_mail",
        "mcp.agent_mail.fetch_inbox",
        generated_at_ms,
    ));

    let mut git = OperatingEnvelopeGitSourceInput::new(
        fixture_provenance(
            "fixture.git",
            "git status --short --branch",
            generated_at_ms,
        ),
        "main",
    );

    match scenario {
        OperatingEnvelopeScenario::Current | OperatingEnvelopeScenario::Healthy => {}
        OperatingEnvelopeScenario::Degraded => {
            agent_mail.unavailable_reason = Some("agent_mail.unavailable_after_retry".to_string());
        }
        OperatingEnvelopeScenario::Blocked => {
            rch.healthy_worker_count = 0;
            rch.remote_cargo_reached = Some(false);
            rch.topology_preflight_failed = true;
        }
        OperatingEnvelopeScenario::Emergency => {
            capacity.pressure_tier = OperatingEnvelopeCapacityPressureTier::Black;
            capacity.kill_switch_active = true;
            git.deleted_path_count = 1;
        }
    }

    OperatingEnvelopeSourceAdapterInputs::new(capacity, rch, beads, agent_mail, git)
        .into_input_domains()
}

fn fixture_provenance(
    source_id: &'static str,
    command_or_api: &'static str,
    generated_at_ms: u64,
) -> OperatingEnvelopeSourceProvenance {
    OperatingEnvelopeSourceProvenance::live_command(source_id, command_or_api).with_freshness(
        Some(generated_at_ms),
        Some(0),
        OperatingEnvelopeFreshnessState::Fresh,
    )
}

fn operating_envelope_summary(plan: &OperatingEnvelopePlan) -> serde_json::Value {
    let next_safe_window = plan
        .admission_windows
        .iter()
        .find(|window| {
            !matches!(
                window.window_class,
                OperatingEnvelopeWindowClass::Wait | OperatingEnvelopeWindowClass::Blocked
            ) && (window.max_parallel_agents > 0 || window.max_parallel_proofs > 0)
        })
        .map(operating_envelope_window_summary);

    serde_json::json!({
        "envelope_tier": plan.decision.envelope_tier,
        "outcome": plan.decision.outcome,
        "confidence": plan.decision.confidence,
        "admission_count": {
            "max_parallel_agents": plan.decision.max_parallel_agents,
            "max_parallel_proofs": plan.decision.max_parallel_proofs,
            "window_count": plan.admission_windows.len(),
        },
        "paused_action_classes": operating_envelope_paused_action_classes(plan),
        "blocked_reason_codes": plan.decision.reason_codes,
        "next_safe_window": next_safe_window,
        "required_approvals": operating_envelope_required_approvals(),
        "proof_requirements": operating_envelope_proof_requirements(plan),
        "forbidden_action_classes": operating_envelope_forbidden_action_classes(plan),
        "raw_pane_content_stored": plan.raw_pane_content_stored,
        "live_mutation_allowed": false,
        "side_effects_executed": false,
    })
}

fn operating_envelope_window_summary(
    window: &OperatingEnvelopeAdmissionWindow,
) -> serde_json::Value {
    serde_json::json!({
        "window_id": window.window_id,
        "window_class": window.window_class,
        "starts_at_ms": window.starts_at_ms,
        "expires_at_ms": window.expires_at_ms,
        "max_parallel_agents": window.max_parallel_agents,
        "max_parallel_proofs": window.max_parallel_proofs,
        "permitted_action_classes": window.permitted_action_classes,
        "reason_codes": window.reason_codes,
    })
}

fn operating_envelope_source_states(plan: &OperatingEnvelopePlan) -> Vec<serde_json::Value> {
    std::iter::once(plan.input_domains.capacity_resource.clone())
        .chain(std::iter::once(plan.input_domains.rch.clone()))
        .chain(std::iter::once(plan.input_domains.beads.clone()))
        .chain(std::iter::once(plan.input_domains.agent_mail.clone()))
        .chain(std::iter::once(plan.input_domains.git.clone()))
        .chain(plan.input_domains.robot_inventory.clone())
        .map(|source| {
            serde_json::json!({
                "source_id": source.source_id,
                "source_kind": source.source_kind,
                "state": source.state,
                "freshness_state": source.freshness_state,
                "evidence_level": source.evidence_level,
                "redacted": source.redacted,
                "raw_pane_content_stored": source.raw_pane_content_stored,
                "reason_codes": source.reason_codes,
                "artifact_paths": source.artifact_paths,
            })
        })
        .collect()
}

fn operating_envelope_paused_action_classes(plan: &OperatingEnvelopePlan) -> Vec<String> {
    if plan.decision.outcome == OperatingEnvelopeOutcome::Admit {
        return Vec::new();
    }
    operating_envelope_forbidden_action_classes(plan)
}

fn operating_envelope_forbidden_action_classes(plan: &OperatingEnvelopePlan) -> Vec<String> {
    let mut values = BTreeSet::new();
    for window in &plan.admission_windows {
        for action_class in &window.forbidden_action_classes {
            values.insert(action_class_name(*action_class));
        }
    }
    values.into_iter().collect()
}

fn operating_envelope_proof_requirements(plan: &OperatingEnvelopePlan) -> Vec<String> {
    let mut requirements = BTreeSet::from([
        "remote_rch_required_for_cargo_proof".to_string(),
        "local_cargo_output_not_counted_as_proof".to_string(),
    ]);
    if plan.decision.max_parallel_proofs == 0 {
        requirements.insert("no_remote_proof_lane_admitted_currently".to_string());
    }
    if matches!(
        plan.decision.target_hardware_state,
        OperatingEnvelopeProofState::Unavailable | OperatingEnvelopeProofState::SkippedNotProven
    ) {
        requirements.insert("target_class_claim_requires_non_skipped_artifact".to_string());
    }
    requirements.into_iter().collect()
}

fn operating_envelope_required_approvals() -> Vec<String> {
    vec![
        "beads_claim_requires_separate_operator_action".to_string(),
        "pane_mutation_requires_separate_approval".to_string(),
        "service_repair_or_restart_is_not_approved".to_string(),
        "rch_cancellation_is_not_approved".to_string(),
    ]
}

fn action_class_name(action_class: OperatingEnvelopeActionClass) -> String {
    serde_json::to_value(action_class)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{action_class:?}"))
}

fn operating_envelope_explain(
    plan: &OperatingEnvelopePlan,
    reason_code: Option<&str>,
) -> serde_json::Value {
    let entries = operating_envelope_explain_entries(plan, reason_code);
    let matched = reason_code.is_none() || !entries.is_empty();
    serde_json::json!({
        "matched": matched,
        "query_reason_code": reason_code,
        "entries": entries,
        "operator_action": if matched {
            "inspect the matched reason-code entries; this explain call executed no mutation"
        } else {
            "refresh ft robot swarm envelope --surface status and choose a reason code from data.summary.blocked_reason_codes"
        },
    })
}

fn operating_envelope_explain_entries(
    plan: &OperatingEnvelopePlan,
    reason_code: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut entries = Vec::new();
    if reason_code_matches(&plan.decision.reason_codes, reason_code) {
        entries.push(serde_json::json!({
            "scope": "decision",
            "subject_id": plan.decision.decision_id,
            "outcome": plan.decision.outcome,
            "envelope_tier": plan.decision.envelope_tier,
            "reason_codes": matching_reason_codes(&plan.decision.reason_codes, reason_code),
        }));
    }
    for window in &plan.admission_windows {
        if reason_code_matches(&window.reason_codes, reason_code) {
            entries.push(serde_json::json!({
                "scope": "admission_window",
                "subject_id": window.window_id,
                "window_class": window.window_class,
                "max_parallel_agents": window.max_parallel_agents,
                "max_parallel_proofs": window.max_parallel_proofs,
                "reason_codes": matching_reason_codes(&window.reason_codes, reason_code),
            }));
        }
    }
    for source in operating_envelope_sources(plan) {
        if reason_code_matches(&source.reason_codes, reason_code) {
            entries.push(serde_json::json!({
                "scope": "source",
                "subject_id": source.source_id,
                "source_kind": source.source_kind,
                "state": source.state,
                "freshness_state": source.freshness_state,
                "reason_codes": matching_reason_codes(&source.reason_codes, reason_code),
            }));
        }
        for evidence in &source.evidence {
            if reason_code_matches(&evidence.reason_codes, reason_code) {
                entries.push(serde_json::json!({
                    "scope": "evidence",
                    "subject_id": evidence.evidence_id,
                    "source_id": source.source_id,
                    "category": evidence.category,
                    "state": evidence.state,
                    "reason_codes": matching_reason_codes(&evidence.reason_codes, reason_code),
                }));
            }
        }
    }
    entries
}

fn operating_envelope_sources(
    plan: &OperatingEnvelopePlan,
) -> Vec<&OperatingEnvelopeSourceSnapshot> {
    let mut sources = vec![
        &plan.input_domains.capacity_resource,
        &plan.input_domains.rch,
        &plan.input_domains.beads,
        &plan.input_domains.agent_mail,
        &plan.input_domains.git,
    ];
    if let Some(robot_inventory) = &plan.input_domains.robot_inventory {
        sources.push(robot_inventory);
    }
    sources
}

fn reason_code_matches(reason_codes: &[String], reason_code: Option<&str>) -> bool {
    reason_code.is_none_or(|needle| reason_codes.iter().any(|reason| reason == needle))
}

fn matching_reason_codes(reason_codes: &[String], reason_code: Option<&str>) -> Vec<String> {
    reason_code.map_or_else(
        || reason_codes.to_vec(),
        |needle| {
            reason_codes
                .iter()
                .filter(|reason| reason.as_str() == needle)
                .cloned()
                .collect()
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvelopeFacts {
    privacy_violation: bool,
    capacity_black: bool,
    capacity_red: bool,
    rch_no_worker: bool,
    rch_topology_failure: bool,
    rch_active_project_exclusion: bool,
    rch_recovered: bool,
    agent_mail_unavailable: bool,
    agent_mail_degraded: bool,
    dirty_non_overlap: bool,
    dirty_overlap: bool,
    deletion_risk: bool,
    stale_owner: bool,
    active_owner: bool,
    no_ready_work: bool,
    target_class_unproven: bool,
    stale_telemetry: bool,
    insufficient_proof: bool,
    kill_switch_active: bool,
    /// A REQUIRED measurement source (capacity_resource or rch) is absent
    /// (NotCollected / Unavailable). Unlike `capacity.*`/`rch.*` reason-coded
    /// failures, an absent source may carry no failure reason code, so without
    /// this fact the decision would fall through to Admit — a fail-OPEN against
    /// the documented `missing_signal_behavior = "lower_envelope"` contract.
    required_telemetry_missing: bool,
}

impl EnvelopeFacts {
    fn from_input(input: &OperatingEnvelopePlannerInput) -> Self {
        let sources = input.sources();
        Self {
            privacy_violation: sources
                .iter()
                .any(|source| !source.redacted || source.raw_pane_content_stored),
            capacity_black: any_reason(&sources, &["capacity.black", "pressure.black"]),
            capacity_red: any_reason(&sources, &["capacity.red", "pressure.red"]),
            rch_no_worker: any_reason(
                &sources,
                &[
                    "rch.no_workers_passed_health",
                    "rch.no_admissible_workers",
                    "rch.no_admissible_workers.critical_pressure",
                    "rch.critical_pressure",
                    "rch.worker_selection.no_available_worker",
                    "rch.worker_selection.no_admissible_workers",
                    "no_admissible_workers=critical_pressure=1",
                ],
            ),
            rch_topology_failure: any_reason(
                &sources,
                &[
                    "rch.topology_preflight_failed",
                    "rch.topology_preflight.fail",
                ],
            ),
            rch_active_project_exclusion: any_reason(
                &sources,
                &[
                    "rch.active_project_exclusion",
                    "rch.active_project_exclusion.active_project_present",
                ],
            ),
            rch_recovered: any_reason(
                &sources,
                &[
                    "rch.remote_cargo_reached_true",
                    "rch.cargo_verdict.pass",
                    "rch.remote_proof_available",
                ],
            ),
            agent_mail_unavailable: input.input_domains.agent_mail.state
                == OperatingEnvelopeSourceState::Unavailable
                || any_reason(
                    &sources,
                    &[
                        "agent_mail.unavailable_after_retry",
                        "agent_mail.database_error",
                    ],
                ),
            agent_mail_degraded: input.input_domains.agent_mail.state
                == OperatingEnvelopeSourceState::Degraded,
            dirty_non_overlap: any_reason(
                &sources,
                &[
                    "git.dirty_non_overlap",
                    "git.dirty_nonoverlap",
                    "dirty_tree.non_overlap",
                ],
            ),
            dirty_overlap: input.input_domains.git.state == OperatingEnvelopeSourceState::Blocked
                || any_reason(
                    &sources,
                    &[
                        "dirty_overlap.present",
                        "git.dirty_overlap_risk",
                        "dirty_overlap.risk",
                    ],
                ),
            deletion_risk: any_reason(
                &sources,
                &[
                    "deletion_risk.present",
                    "deletion_risk.frankenterm_core",
                    "git.deleted_paths_present",
                ],
            ),
            stale_owner: any_reason(
                &sources,
                &["beads.stale_candidate", "beads.stale_reopen_candidate"],
            ),
            active_owner: any_reason(
                &sources,
                &["beads.in_progress_active", "assignee_overlap.active"],
            ),
            no_ready_work: any_reason(&sources, &["beads.ready_empty", "beads.no_ready_work"]),
            target_class_unproven: input.target_class.proof_state
                == OperatingEnvelopeProofState::SkippedNotProven
                || input.target_class.proof_state == OperatingEnvelopeProofState::Unavailable
                || any_reason(
                    &sources,
                    &[
                        "target_hardware.skipped_not_proven",
                        "target_class.hardware_not_available",
                        "attestation.skipped_not_proven",
                        "capacity.target_class_unproven",
                    ],
                ),
            stale_telemetry: sources
                .iter()
                .any(|source| source.freshness_state == OperatingEnvelopeFreshnessState::Stale)
                || any_reason(&sources, &["telemetry.stale", "capacity.stale"]),
            insufficient_proof: any_reason(
                &sources,
                &[
                    "proof.insufficient",
                    "rch.remote_cargo_reached_false",
                    "local_cargo.forbidden",
                ],
            ),
            kill_switch_active: any_reason(
                &sources,
                &[
                    "capacity.kill_switch_active",
                    "policy.kill_switch_active",
                    "kill_switch.active",
                ],
            ),
            // Fail-closed on ABSENT required telemetry. capacity_resource and rch
            // are required inputs; if either is NotCollected/Unavailable we must
            // not admit, even when the source reported no failure reason code.
            // (agent_mail / git / target_class / stale_telemetry already guard on
            // their source state above; this closes the same gap for capacity+rch.)
            required_telemetry_missing: matches!(
                input.input_domains.capacity_resource.state,
                OperatingEnvelopeSourceState::NotCollected
                    | OperatingEnvelopeSourceState::Unavailable
            ) || matches!(
                input.input_domains.rch.state,
                OperatingEnvelopeSourceState::NotCollected
                    | OperatingEnvelopeSourceState::Unavailable
            ),
        }
    }

    fn rch_unavailable_or_insufficient(&self) -> bool {
        self.rch_no_worker
            || self.rch_topology_failure
            || self.rch_active_project_exclusion
            || self.insufficient_proof
    }

    fn rch_contradiction(&self) -> bool {
        self.rch_recovered && self.rch_unavailable_or_insufficient()
    }
}

fn decision_for(
    input: &OperatingEnvelopePlannerInput,
    facts: &EnvelopeFacts,
) -> OperatingEnvelopeDecision {
    let mut reason_codes = Vec::new();
    let (outcome, tier, confidence, dirty_tree_state, max_agents, max_proofs) =
        if facts.privacy_violation {
            push_unique(&mut reason_codes, "source.raw_content_forbidden");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.deletion_risk {
            push_unique(&mut reason_codes, "deletion_risk.present");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.kill_switch_active {
            push_unique(&mut reason_codes, "capacity.kill_switch_active");
            push_unique(&mut reason_codes, "policy.kill_switch_active");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.capacity_black {
            push_unique(&mut reason_codes, "capacity.black");
            (
                OperatingEnvelopeOutcome::Shed,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.rch_contradiction() {
            push_unique(&mut reason_codes, "fail_closed.block_contradiction");
            push_unique(&mut reason_codes, "rch.evidence_contradictory");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.rch_topology_failure {
            push_unique(&mut reason_codes, "rch.topology_preflight_failed");
            (
                OperatingEnvelopeOutcome::Block,
                OperatingEnvelopeTier::Black,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.capacity_red {
            push_unique(&mut reason_codes, "capacity.red");
            (
                OperatingEnvelopeOutcome::Shed,
                OperatingEnvelopeTier::Red,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.dirty_overlap || facts.active_owner {
            if facts.dirty_overlap {
                push_unique(&mut reason_codes, "dirty_overlap.present");
            }
            if facts.active_owner {
                push_unique(&mut reason_codes, "assignee_overlap.active");
            }
            (
                OperatingEnvelopeOutcome::Wait,
                OperatingEnvelopeTier::Orange,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.rch_no_worker || facts.rch_active_project_exclusion {
            if facts.rch_no_worker {
                push_unique(&mut reason_codes, "rch.no_workers_passed_health");
            }
            if facts.rch_active_project_exclusion {
                push_unique(&mut reason_codes, "rch.active_project_exclusion");
            }
            (
                OperatingEnvelopeOutcome::Defer,
                OperatingEnvelopeTier::Red,
                OperatingEnvelopeConfidence::Unavailable,
                dirty_tree_state(facts),
                0,
                0,
            )
        } else if facts.target_class_unproven {
            push_unique(&mut reason_codes, "target_hardware.skipped_not_proven");
            push_unique(&mut reason_codes, "capacity.target_class_unproven");
            (
                OperatingEnvelopeOutcome::Defer,
                OperatingEnvelopeTier::Orange,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.agent_mail_unavailable || facts.agent_mail_degraded {
            if facts.agent_mail_unavailable {
                push_unique(&mut reason_codes, "agent_mail.unavailable_after_retry");
            } else {
                push_unique(&mut reason_codes, "agent_mail.degraded");
            }
            push_unique(&mut reason_codes, "fallback.beads_only");
            (
                OperatingEnvelopeOutcome::Degrade,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.interactive_agents.min(4),
                0,
            )
        } else if facts.no_ready_work {
            push_unique(&mut reason_codes, "beads.ready_empty");
            (
                OperatingEnvelopeOutcome::Wait,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.stale_owner || facts.stale_telemetry {
            if facts.stale_owner {
                push_unique(&mut reason_codes, "beads.stale_candidate");
            }
            if facts.stale_telemetry {
                push_unique(&mut reason_codes, "telemetry.stale");
            }
            (
                OperatingEnvelopeOutcome::Degrade,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Stale,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.insufficient_proof && !facts.rch_recovered {
            push_unique(&mut reason_codes, "proof.insufficient");
            (
                OperatingEnvelopeOutcome::Degrade,
                OperatingEnvelopeTier::Yellow,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else if facts.required_telemetry_missing {
            // FND/GA-FND-015: a required telemetry source (capacity_resource or
            // rch) is absent and none of the reason-coded failure facts fired.
            // Fail CLOSED — lower the envelope rather than admit on missing
            // measurement (the documented `missing_signal_behavior` contract).
            push_unique(&mut reason_codes, "fail_closed.lower_missing");
            push_unique(&mut reason_codes, "telemetry.required_source_missing");
            (
                OperatingEnvelopeOutcome::Defer,
                OperatingEnvelopeTier::Orange,
                OperatingEnvelopeConfidence::Mixed,
                dirty_tree_state(facts),
                input.budgets.docs_static_checks,
                0,
            )
        } else {
            push_unique(&mut reason_codes, "envelope.all_required_sources_available");
            (
                OperatingEnvelopeOutcome::Admit,
                OperatingEnvelopeTier::Green,
                OperatingEnvelopeConfidence::Measured,
                dirty_tree_state(facts),
                input.budgets.interactive_agents,
                input.budgets.long_rch_proof_lanes,
            )
        };

    push_unique(&mut reason_codes, "policy.no_local_cargo_proof");
    push_unique(&mut reason_codes, "source.redacted_summary_only");

    OperatingEnvelopeDecision {
        decision_id: format!("decision-{}", slug(&input.envelope_id)),
        outcome,
        envelope_tier: tier,
        confidence,
        target_hardware_state: input.target_class.proof_state,
        rch_proof_state: rch_proof_state(facts),
        agent_mail_state: input.input_domains.agent_mail.state,
        dirty_tree_state,
        max_parallel_agents: max_agents,
        max_parallel_proofs: max_proofs,
        reason_codes,
    }
}

fn admission_windows_for(
    input: &OperatingEnvelopePlannerInput,
    facts: &EnvelopeFacts,
    decision: &OperatingEnvelopeDecision,
) -> Vec<OperatingEnvelopeAdmissionWindow> {
    match decision.outcome {
        OperatingEnvelopeOutcome::Admit => vec![
            window(
                "admit_now",
                OperatingEnvelopeWindowClass::Open,
                input.generated_at_ms,
                Some(input.generated_at_ms.saturating_add(300_000)),
                decision.max_parallel_agents,
                decision.max_parallel_proofs,
                vec![
                    OperatingEnvelopeActionClass::ReadStatus,
                    OperatingEnvelopeActionClass::ClaimBead,
                    OperatingEnvelopeActionClass::AddBeadsComment,
                    OperatingEnvelopeActionClass::EditFiles,
                    OperatingEnvelopeActionClass::RunStaticCheck,
                    OperatingEnvelopeActionClass::RunRchProof,
                ],
                vec![
                    "envelope.admit",
                    "rch.remote_cargo_reached_true",
                    "proof.remote_only",
                ],
                input,
            ),
            proof_window("proof_only", input, input.budgets.long_rch_proof_lanes),
            static_window("docs_only", input, input.budgets.docs_static_checks),
        ],
        OperatingEnvelopeOutcome::Degrade => {
            let mut windows = vec![static_window(
                "docs_only",
                input,
                decision
                    .max_parallel_agents
                    .max(input.budgets.docs_static_checks),
            )];
            if facts.agent_mail_unavailable || facts.agent_mail_degraded {
                windows.push(wait_window(
                    "admit_after_agent_mail_recovers",
                    input,
                    &["agent_mail.recovery_required", "fallback.beads_only"],
                ));
            }
            if facts.stale_owner {
                windows.push(read_only_window(
                    "stale_reopen_status_check",
                    input,
                    &["beads.stale_candidate", "coordination.status_check_first"],
                ));
            }
            windows
        }
        OperatingEnvelopeOutcome::Defer => vec![
            static_window("docs_only", input, input.budgets.docs_static_checks),
            wait_window(
                "admit_after_rch_recovers",
                input,
                &["rch.recovery_required", "proof.remote_unavailable"],
            ),
        ],
        OperatingEnvelopeOutcome::Wait => vec![wait_window(
            "pause_admission",
            input,
            &wait_reason_codes(facts),
        )],
        OperatingEnvelopeOutcome::Shed => vec![window(
            "emergency_stop_recommended",
            OperatingEnvelopeWindowClass::Blocked,
            input.generated_at_ms,
            None,
            0,
            0,
            vec![OperatingEnvelopeActionClass::ReadStatus],
            vec!["capacity.pressure_shed", "envelope.shed"],
            input,
        )],
        OperatingEnvelopeOutcome::Block => vec![window(
            "emergency_stop_recommended",
            OperatingEnvelopeWindowClass::Blocked,
            input.generated_at_ms,
            None,
            0,
            0,
            vec![
                OperatingEnvelopeActionClass::ReadStatus,
                OperatingEnvelopeActionClass::AddBeadsComment,
            ],
            decision
                .reason_codes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            input,
        )],
    }
}

fn static_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    max_agents: u32,
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::StaticOnly,
        input.generated_at_ms,
        Some(input.generated_at_ms.saturating_add(300_000)),
        max_agents,
        0,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::AddBeadsComment,
            OperatingEnvelopeActionClass::RunStaticCheck,
        ],
        vec!["proof.static_only", "policy.no_local_cargo_proof"],
        input,
    )
}

fn proof_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    max_proofs: u32,
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::RchProofAllowed,
        input.generated_at_ms,
        Some(input.generated_at_ms.saturating_add(300_000)),
        input.budgets.interactive_agents.min(2),
        max_proofs,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::RunRchProof,
        ],
        vec!["proof.remote_only", "rch.remote_cargo_reached_true"],
        input,
    )
}

fn read_only_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    reason_codes: &[&str],
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::ReadOnly,
        input.generated_at_ms,
        Some(input.generated_at_ms.saturating_add(300_000)),
        input.budgets.docs_static_checks,
        0,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::AddBeadsComment,
        ],
        reason_codes.to_vec(),
        input,
    )
}

fn wait_window(
    window_id: &'static str,
    input: &OperatingEnvelopePlannerInput,
    reason_codes: &[&str],
) -> OperatingEnvelopeAdmissionWindow {
    window(
        window_id,
        OperatingEnvelopeWindowClass::Wait,
        input.generated_at_ms,
        None,
        0,
        0,
        vec![
            OperatingEnvelopeActionClass::ReadStatus,
            OperatingEnvelopeActionClass::Wait,
        ],
        reason_codes.to_vec(),
        input,
    )
}

#[allow(clippy::too_many_arguments)]
fn window(
    window_id: impl Into<String>,
    window_class: OperatingEnvelopeWindowClass,
    starts_at_ms: u64,
    expires_at_ms: Option<u64>,
    max_parallel_agents: u32,
    max_parallel_proofs: u32,
    permitted_action_classes: Vec<OperatingEnvelopeActionClass>,
    reason_codes: Vec<&str>,
    input: &OperatingEnvelopePlannerInput,
) -> OperatingEnvelopeAdmissionWindow {
    OperatingEnvelopeAdmissionWindow {
        window_id: window_id.into(),
        window_class,
        starts_at_ms: Some(starts_at_ms),
        expires_at_ms,
        max_parallel_agents,
        max_parallel_proofs,
        permitted_action_classes,
        forbidden_action_classes: default_forbidden_action_classes(),
        source_ids: source_ids(input),
        reason_codes: reason_codes.into_iter().map(str::to_string).collect(),
    }
}

fn default_forbidden_action_classes() -> Vec<OperatingEnvelopeActionClass> {
    vec![
        OperatingEnvelopeActionClass::AgentMailRepair,
        OperatingEnvelopeActionClass::BuildCancellation,
        OperatingEnvelopeActionClass::DestructiveGit,
        OperatingEnvelopeActionClass::DestructiveFilesystem,
        OperatingEnvelopeActionClass::LocalCargoProof,
        OperatingEnvelopeActionClass::RawPaneContentCapture,
        OperatingEnvelopeActionClass::RawPaneContent,
        OperatingEnvelopeActionClass::RchDaemonRestart,
        OperatingEnvelopeActionClass::ServiceRestart,
        OperatingEnvelopeActionClass::ServiceMutation,
        OperatingEnvelopeActionClass::PaneMutation,
        OperatingEnvelopeActionClass::WorkerDrain,
    ]
}

fn stable_rank_windows(windows: &mut [OperatingEnvelopeAdmissionWindow]) {
    windows.sort_by_key(|window| {
        let class_rank = match window.window_class {
            OperatingEnvelopeWindowClass::Open => 0,
            OperatingEnvelopeWindowClass::RchProofAllowed => 1,
            OperatingEnvelopeWindowClass::ClaimAllowed => 2,
            OperatingEnvelopeWindowClass::StaticOnly => 3,
            OperatingEnvelopeWindowClass::ReadOnly => 4,
            OperatingEnvelopeWindowClass::Defer => 5,
            OperatingEnvelopeWindowClass::Wait => 6,
            OperatingEnvelopeWindowClass::Blocked => 7,
        };
        (class_rank, window.window_id.clone())
    });
}

fn source_ids(input: &OperatingEnvelopePlannerInput) -> Vec<String> {
    input
        .sources()
        .into_iter()
        .map(|source| source.source_id.clone())
        .collect()
}

fn any_reason(sources: &[&OperatingEnvelopeSourceSnapshot], needles: &[&str]) -> bool {
    sources.iter().any(|source| {
        source
            .reason_codes
            .iter()
            .chain(
                source
                    .evidence
                    .iter()
                    .flat_map(|evidence| evidence.reason_codes.iter()),
            )
            .any(|reason| needles.contains(&reason.as_str()))
    })
}

fn adapter_snapshot(
    provenance: &OperatingEnvelopeSourceProvenance,
    source_kind: OperatingEnvelopeSourceKind,
) -> OperatingEnvelopeSourceSnapshot {
    let mut snapshot = OperatingEnvelopeSourceSnapshot::new(&provenance.source_id, source_kind)
        .evidence_level(provenance.evidence_level);
    snapshot
        .command_or_api
        .clone_from(&provenance.command_or_api);
    snapshot.collected_at_ms = provenance.collected_at_ms;
    snapshot.freshness_ms = provenance.freshness_ms;
    snapshot.freshness_state = provenance.freshness_state;
    snapshot.evidence.clear();
    for artifact_path in &provenance.artifact_paths {
        snapshot = snapshot.with_artifact_path(artifact_path.clone());
    }
    snapshot
}

fn evidence(
    category: OperatingEnvelopeEvidenceCategory,
    state: OperatingEnvelopeEvidenceState,
    subject: impl Into<String>,
    reason_code: impl Into<String>,
) -> OperatingEnvelopeEvidenceItem {
    OperatingEnvelopeEvidenceItem::new(category, state, subject).with_reason_code(reason_code)
}

fn normalized_optional_reason(reason: Option<&str>) -> Option<String> {
    reason
        .map(|reason| bounded_string(reason, "source.unavailable"))
        .filter(|reason| !reason.is_empty())
}

fn dirty_tree_state(facts: &EnvelopeFacts) -> OperatingEnvelopeDirtyTreeState {
    if facts.dirty_overlap {
        OperatingEnvelopeDirtyTreeState::DirtyOverlap
    } else if facts.deletion_risk {
        OperatingEnvelopeDirtyTreeState::Unknown
    } else if facts.dirty_non_overlap {
        OperatingEnvelopeDirtyTreeState::DirtyNonOverlap
    } else {
        OperatingEnvelopeDirtyTreeState::Clean
    }
}

fn wait_reason_codes(facts: &EnvelopeFacts) -> Vec<&'static str> {
    let mut reason_codes = Vec::new();
    if facts.dirty_overlap {
        reason_codes.push("dirty_overlap.present");
        reason_codes.push("coordination.wait_for_owner");
    }
    if facts.active_owner {
        reason_codes.push("assignee_overlap.active");
        reason_codes.push("coordination.wait_for_owner");
    }
    if facts.no_ready_work {
        reason_codes.push("beads.ready_empty");
        reason_codes.push("coordination.wait_for_ready_work");
    }
    if reason_codes.is_empty() {
        reason_codes.push("coordination.wait");
    }
    reason_codes
}

fn rch_proof_state(facts: &EnvelopeFacts) -> OperatingEnvelopeProofState {
    if facts.rch_unavailable_or_insufficient() {
        OperatingEnvelopeProofState::Unavailable
    } else if facts.rch_recovered {
        OperatingEnvelopeProofState::Measured
    } else {
        OperatingEnvelopeProofState::NotRequired
    }
}

fn push_unique<T>(values: &mut Vec<T>, value: impl Into<T>)
where
    T: Eq,
{
    let value = value.into();
    if !values.contains(&value) {
        values.push(value);
    }
}

fn bounded_string(value: impl Into<String>, fallback: &str) -> String {
    let value = value.into();
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.chars().take(256).collect()
    }
}

fn slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW_MS: u64 = 1_778_912_100_000;

    fn source(
        source_id: &str,
        source_kind: OperatingEnvelopeSourceKind,
        reason_code: &str,
    ) -> OperatingEnvelopeSourceSnapshot {
        OperatingEnvelopeSourceSnapshot::new(source_id, source_kind)
            .evidence_level(OperatingEnvelopeEvidenceLevel::Fixture)
            .with_reason_code(reason_code)
    }

    fn base_domains() -> OperatingEnvelopeInputDomains {
        OperatingEnvelopeInputDomains {
            capacity_resource: source(
                "capacity-green",
                OperatingEnvelopeSourceKind::CapacityResource,
                "capacity.green",
            ),
            rch: source(
                "rch-healthy",
                OperatingEnvelopeSourceKind::Rch,
                "rch.remote_cargo_reached_true",
            ),
            beads: source(
                "beads-ready",
                OperatingEnvelopeSourceKind::Beads,
                "beads.ready_available",
            ),
            agent_mail: source(
                "agent-mail-healthy",
                OperatingEnvelopeSourceKind::AgentMail,
                "agent_mail.available",
            ),
            git: source(
                "git-clean",
                OperatingEnvelopeSourceKind::Git,
                "git.clean_for_scope",
            ),
            robot_inventory: None,
        }
    }

    fn input_with(domains: OperatingEnvelopeInputDomains) -> OperatingEnvelopePlannerInput {
        OperatingEnvelopePlannerInput::new(NOW_MS, "test-envelope", "test-objective", domains)
            .target_class(
                OperatingEnvelopeTargetClass::target_64_core_256g()
                    .proof_state(OperatingEnvelopeProofState::Measured),
            )
            .budgets(OperatingEnvelopeBudgets::target_class())
    }

    fn provenance(source_id: &str, command_or_api: &str) -> OperatingEnvelopeSourceProvenance {
        OperatingEnvelopeSourceProvenance::retained_artifact(source_id, command_or_api)
            .with_freshness(
                Some(NOW_MS),
                Some(1_000),
                OperatingEnvelopeFreshnessState::Fresh,
            )
    }

    fn adapter_domains() -> OperatingEnvelopeInputDomains {
        let mut capacity = OperatingEnvelopeCapacitySourceInput::new(provenance(
            "capacity-resource-retained",
            "docs/attestations/proofs/resource-cockpit-target-class.json",
        ));
        capacity.pressure_tier = OperatingEnvelopeCapacityPressureTier::Green;
        capacity.target_class_proof_state = OperatingEnvelopeProofState::Measured;

        let mut rch = OperatingEnvelopeRchSourceInput::new(provenance(
            "rch-status-retained",
            "rch status --json",
        ));
        rch.healthy_worker_count = 3;
        rch.remote_cargo_reached = Some(true);
        rch.remote_cargo_verdict_pass = true;

        let mut beads = OperatingEnvelopeBeadsSourceInput::new(provenance(
            "beads-bv-retained",
            "bv --robot-triage",
        ));
        beads.ready_count = 4;
        beads.blocked_count = 2;
        beads.dependency_impact_count = 6;

        let agent_mail = OperatingEnvelopeAgentMailSourceInput::new(provenance(
            "agent-mail-inbox-retained",
            "fetch_inbox",
        ));

        let git = OperatingEnvelopeGitSourceInput::new(
            provenance("git-status-retained", "git status --short --branch"),
            "main",
        );

        OperatingEnvelopeSourceAdapterInputs::new(capacity, rch, beads, agent_mail, git)
            .into_input_domains()
    }

    fn plan_with(domains: OperatingEnvelopeInputDomains) -> OperatingEnvelopePlan {
        plan_operating_envelope(input_with(domains))
    }

    fn window_ids(plan: &OperatingEnvelopePlan) -> Vec<&str> {
        plan.admission_windows
            .iter()
            .map(|window| window.window_id.as_str())
            .collect()
    }

    #[test]
    fn clean_ready_queue_admits_ranked_windows() {
        let plan = plan_with(base_domains());

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Green);
        assert_eq!(
            window_ids(&plan),
            vec!["admit_now", "proof_only", "docs_only"]
        );
        assert_eq!(plan.decision.max_parallel_agents, 32);
        assert_eq!(plan.decision.max_parallel_proofs, 8);
        assert!(
            plan.reason_codes
                .contains(&"policy.no_local_cargo_proof".to_string())
        );
    }

    #[test]
    fn no_ready_work_keeps_wait_advice_only() {
        let mut domains = base_domains();
        domains.beads = source(
            "beads-empty",
            OperatingEnvelopeSourceKind::Beads,
            "beads.ready_empty",
        );

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Wait);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Yellow);
        assert_eq!(window_ids(&plan), vec!["pause_admission"]);
        assert!(
            plan.admission_windows[0]
                .reason_codes
                .contains(&"beads.ready_empty".to_string())
        );
        assert!(
            !plan.admission_windows[0]
                .reason_codes
                .contains(&"dirty_overlap.present".to_string())
        );
    }

    #[test]
    fn active_non_stale_owner_waits_without_claiming() {
        let mut domains = base_domains();
        domains.beads = source(
            "beads-owner",
            OperatingEnvelopeSourceKind::Beads,
            "beads.in_progress_active",
        )
        .with_reason_code("assignee_overlap.active");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Wait);
        assert_eq!(
            plan.decision.dirty_tree_state,
            OperatingEnvelopeDirtyTreeState::Clean
        );
        assert_eq!(plan.decision.max_parallel_agents, 0);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"assignee_overlap.active".to_string())
        );
        assert!(
            !plan
                .decision
                .reason_codes
                .contains(&"dirty_overlap.present".to_string())
        );
    }

    #[test]
    fn dirty_non_overlap_is_preserved_without_blocking_claims() {
        let mut domains = base_domains();
        domains.git = source(
            "git-dirty-non-overlap",
            OperatingEnvelopeSourceKind::Git,
            "git.dirty_non_overlap",
        );

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert_eq!(
            plan.decision.dirty_tree_state,
            OperatingEnvelopeDirtyTreeState::DirtyNonOverlap
        );
    }

    #[test]
    fn stale_reopen_candidate_gets_read_only_status_check() {
        let mut domains = base_domains();
        domains.beads = source(
            "beads-stale",
            OperatingEnvelopeSourceKind::Beads,
            "beads.stale_candidate",
        );

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Degrade);
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "stale_reopen_status_check"]
        );
    }

    #[test]
    fn dirty_overlap_waits_and_forbids_edits() {
        let mut domains = base_domains();
        domains.git = source(
            "git-dirty-overlap",
            OperatingEnvelopeSourceKind::Git,
            "dirty_overlap.present",
        )
        .blocked("dirty_overlap.present");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Wait);
        assert_eq!(
            plan.decision.dirty_tree_state,
            OperatingEnvelopeDirtyTreeState::DirtyOverlap
        );
        assert!(
            plan.admission_windows[0]
                .forbidden_action_classes
                .contains(&OperatingEnvelopeActionClass::DestructiveFilesystem)
        );
    }

    #[test]
    fn rch_degraded_no_worker_defers_remote_proof() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-no-worker",
            OperatingEnvelopeSourceKind::Rch,
            "rch.no_workers_passed_health",
        )
        .with_reason_code("rch.remote_cargo_reached_false")
        .blocked("rch.no_workers_passed_health");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Defer);
        assert_eq!(
            plan.decision.rch_proof_state,
            OperatingEnvelopeProofState::Unavailable
        );
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "admit_after_rch_recovers"]
        );
    }

    #[test]
    fn contradictory_rch_evidence_blocks_and_keeps_proof_unavailable() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-contradictory",
            OperatingEnvelopeSourceKind::Rch,
            "rch.remote_cargo_reached_true",
        )
        .with_reason_code("rch.no_workers_passed_health")
        .blocked("rch.no_workers_passed_health");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Block);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Black);
        assert_eq!(
            plan.decision.rch_proof_state,
            OperatingEnvelopeProofState::Unavailable
        );
        assert!(
            plan.decision
                .reason_codes
                .contains(&"fail_closed.block_contradiction".to_string())
        );
        assert!(
            plan.decision
                .reason_codes
                .contains(&"rch.evidence_contradictory".to_string())
        );
        assert_eq!(window_ids(&plan), vec!["emergency_stop_recommended"]);
    }

    #[test]
    fn rch_critical_pressure_defers_remote_proof() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-critical-pressure",
            OperatingEnvelopeSourceKind::Rch,
            "rch.no_admissible_workers.critical_pressure",
        )
        .with_reason_code("rch.remote_cargo_reached_false")
        .blocked("rch.no_admissible_workers.critical_pressure");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Defer);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Red);
        assert_eq!(
            plan.decision.rch_proof_state,
            OperatingEnvelopeProofState::Unavailable
        );
        assert_eq!(plan.decision.max_parallel_proofs, 0);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"rch.no_workers_passed_health".to_string())
        );
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "admit_after_rch_recovers"]
        );
    }

    #[test]
    fn rch_recovered_allows_proof_window() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-recovered",
            OperatingEnvelopeSourceKind::Rch,
            "rch.remote_cargo_reached_true",
        )
        .with_reason_code("rch.cargo_verdict.pass");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert!(window_ids(&plan).contains(&"proof_only"));
    }

    #[test]
    fn target_class_insufficient_proof_defers_claims() {
        let input = OperatingEnvelopePlannerInput::new(
            NOW_MS,
            "target-class",
            "target-objective",
            base_domains(),
        )
        .target_class(OperatingEnvelopeTargetClass::target_64_core_256g())
        .budgets(OperatingEnvelopeBudgets::target_class());

        let plan = plan_operating_envelope(input);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Defer);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"target_hardware.skipped_not_proven".to_string())
        );
    }

    #[test]
    fn source_adapter_bundle_admits_only_with_known_green_evidence() {
        let domains = adapter_domains();

        for source in [
            &domains.capacity_resource,
            &domains.rch,
            &domains.beads,
            &domains.agent_mail,
            &domains.git,
        ] {
            assert!(source.redacted);
            assert!(!source.raw_pane_content_stored);
            assert_eq!(
                source.freshness_state,
                OperatingEnvelopeFreshnessState::Fresh
            );
        }

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Green);
        assert!(
            plan.admission_windows[0]
                .forbidden_action_classes
                .contains(&OperatingEnvelopeActionClass::AgentMailRepair)
        );
        assert!(
            plan.admission_windows[0]
                .forbidden_action_classes
                .contains(&OperatingEnvelopeActionClass::RawPaneContent)
        );
    }

    #[test]
    fn beads_adapter_stale_candidate_degrades_to_status_check() {
        let mut domains = adapter_domains();
        let mut beads = OperatingEnvelopeBeadsSourceInput::new(provenance(
            "beads-stale-retained",
            "scripts/swarm-tick.sh --agent-mail-fallback frankenterm",
        ));
        beads.ready_count = 2;
        beads.stale_candidate_count = 1;
        domains.beads = beads.to_source_snapshot();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Degrade);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"beads.stale_candidate".to_string())
        );
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "stale_reopen_status_check"]
        );
    }

    #[test]
    fn rch_adapter_no_workers_and_no_cargo_reach_defers_proof() {
        let mut domains = adapter_domains();
        let mut rch = OperatingEnvelopeRchSourceInput::new(provenance(
            "rch-no-workers-retained",
            "rch diagnose --json --dry-run",
        ));
        rch.no_workers_passed_health = true;
        rch.remote_cargo_reached = Some(false);
        domains.rch = rch.to_source_snapshot();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Defer);
        assert_eq!(
            plan.decision.rch_proof_state,
            OperatingEnvelopeProofState::Unavailable
        );
        assert!(
            plan.decision
                .reason_codes
                .contains(&"rch.no_workers_passed_health".to_string())
        );
    }

    #[test]
    fn agent_mail_adapter_database_error_uses_beads_only_fallback() {
        let mut domains = adapter_domains();
        let mut agent_mail = OperatingEnvelopeAgentMailSourceInput::new(provenance(
            "agent-mail-db-error-retained",
            "fetch_inbox",
        ));
        agent_mail.database_error = true;
        agent_mail.forbidden_service_remediation_seen = true;
        domains.agent_mail = agent_mail.to_source_snapshot();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Degrade);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"fallback.beads_only".to_string())
        );
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "admit_after_agent_mail_recovers"]
        );
        assert!(
            plan.admission_windows[0]
                .forbidden_action_classes
                .contains(&OperatingEnvelopeActionClass::ServiceRestart)
        );
    }

    #[test]
    fn git_adapter_core_deletion_risk_blocks() {
        let mut domains = adapter_domains();
        let mut git = OperatingEnvelopeGitSourceInput::new(
            provenance("git-core-deletion-retained", "git status --porcelain=v1"),
            "main",
        );
        git.deleted_frankenterm_core_path_count = 1;
        domains.git = git.to_source_snapshot();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Block);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Black);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"deletion_risk.present".to_string())
        );
    }

    #[test]
    fn capacity_adapter_kill_switch_blocks_admission() {
        let mut domains = adapter_domains();
        let mut capacity = OperatingEnvelopeCapacitySourceInput::new(provenance(
            "capacity-kill-switch-retained",
            "policy diagnostics",
        ));
        capacity.pressure_tier = OperatingEnvelopeCapacityPressureTier::Green;
        capacity.target_class_proof_state = OperatingEnvelopeProofState::Measured;
        capacity.kill_switch_active = true;
        domains.capacity_resource = capacity.to_source_snapshot();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Block);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Black);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"capacity.kill_switch_active".to_string())
        );
    }

    #[test]
    fn source_adapters_fail_closed_on_unknown_required_evidence() {
        let mut domains = adapter_domains();
        let mut capacity =
            OperatingEnvelopeCapacitySourceInput::new(provenance("capacity-unknown", "fixture"));
        capacity.target_class_proof_state = OperatingEnvelopeProofState::Measured;
        domains.capacity_resource = capacity.to_source_snapshot();
        domains.rch = OperatingEnvelopeRchSourceInput::new(provenance("rch-unknown", "fixture"))
            .to_source_snapshot();

        let plan = plan_with(domains);

        assert_ne!(plan.decision.outcome, OperatingEnvelopeOutcome::Admit);
        assert!(
            plan.decision
                .reason_codes
                .contains(&"proof.insufficient".to_string())
                || plan
                    .decision
                    .reason_codes
                    .contains(&"telemetry.stale".to_string())
        );
    }

    #[test]
    fn missing_capacity_telemetry_fails_closed_not_admit() {
        // gauntlet GA-FND-015: a REQUIRED source (capacity_resource) that is
        // NotCollected must NOT admit — the envelope fails closed on absent
        // telemetry even when the source carries no failure reason code. Before the
        // fix, this fell through to Admit (Green) — a fail-OPEN.
        let mut domains = base_domains();
        domains.capacity_resource = source(
            "capacity-missing",
            OperatingEnvelopeSourceKind::CapacityResource,
            "capacity.green",
        )
        .not_collected("capacity.collector_unreachable");
        let plan = plan_with(domains);
        assert_ne!(
            plan.decision.outcome,
            OperatingEnvelopeOutcome::Admit,
            "missing capacity telemetry must NOT admit (fail-closed); outcome={:?} reasons={:?}",
            plan.decision.outcome,
            plan.decision.reason_codes
        );
        assert!(
            plan.decision
                .reason_codes
                .contains(&"telemetry.required_source_missing".to_string()),
            "expected telemetry.required_source_missing; got {:?}",
            plan.decision.reason_codes
        );
    }

    #[test]
    fn missing_rch_telemetry_fails_closed_not_admit() {
        let mut domains = base_domains();
        domains.rch = source(
            "rch-missing",
            OperatingEnvelopeSourceKind::Rch,
            "rch.remote_cargo_reached_true",
        )
        .not_collected("rch.collector_unreachable");
        let plan = plan_with(domains);
        assert_ne!(
            plan.decision.outcome,
            OperatingEnvelopeOutcome::Admit,
            "missing rch telemetry must NOT admit (fail-closed); outcome={:?} reasons={:?}",
            plan.decision.outcome,
            plan.decision.reason_codes
        );
    }

    #[test]
    fn red_and_black_pressure_shed() {
        let mut red = base_domains();
        red.capacity_resource = source(
            "capacity-red",
            OperatingEnvelopeSourceKind::CapacityResource,
            "capacity.red",
        );
        let red_plan = plan_with(red);
        assert_eq!(red_plan.decision.outcome, OperatingEnvelopeOutcome::Shed);
        assert_eq!(red_plan.decision.envelope_tier, OperatingEnvelopeTier::Red);

        let mut black = base_domains();
        black.capacity_resource = source(
            "capacity-black",
            OperatingEnvelopeSourceKind::CapacityResource,
            "capacity.black",
        );
        let black_plan = plan_with(black);
        assert_eq!(black_plan.decision.outcome, OperatingEnvelopeOutcome::Shed);
        assert_eq!(
            black_plan.decision.envelope_tier,
            OperatingEnvelopeTier::Black
        );
    }

    #[test]
    fn agent_mail_unavailable_uses_beads_only_static_window() {
        let mut domains = base_domains();
        domains.agent_mail = source(
            "agent-mail-red",
            OperatingEnvelopeSourceKind::AgentMail,
            "agent_mail.unavailable_after_retry",
        )
        .unavailable("agent_mail.unavailable_after_retry");

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Degrade);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Yellow);
        assert_eq!(
            window_ids(&plan),
            vec!["docs_only", "admit_after_agent_mail_recovers"]
        );
        assert_eq!(plan.decision.max_parallel_proofs, 0);
    }

    #[test]
    fn privacy_violation_blocks_even_when_other_sources_are_green() {
        let mut domains = base_domains();
        domains.git = domains.git.clone().raw_pane_content_stored();

        let plan = plan_with(domains);

        assert_eq!(plan.decision.outcome, OperatingEnvelopeOutcome::Block);
        assert_eq!(plan.decision.envelope_tier, OperatingEnvelopeTier::Black);
        assert_eq!(plan.decision.max_parallel_agents, 0);
    }
}
