//! Side-effect-free mission objective planning.
//!
//! This module turns already-collected, redacted coordination facts into a
//! deterministic assignment plan. It intentionally does not shell out, claim
//! Beads, mutate panes, restart services, run Cargo, or read pane text.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize, Serializer};

pub const MISSION_OBJECTIVE_PLAN_CONTRACT_ID: &str = "ft.mission_objective_plan.v1";
pub const MISSION_OBJECTIVE_PLAN_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 7_200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveStrictness {
    Advisory,
    Normal,
    Strict,
}

impl Default for MissionObjectiveStrictness {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectivePlanStatus {
    Actionable,
    PlanningOnly,
    Blocked,
    WaitingOwner,
    WaitingExternal,
    DirtyOverlap,
    NoReadyWork,
    RchSubstrateBlocked,
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveRiskLevel {
    Low,
    Medium,
    High,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveSourceKind {
    Beads,
    AgentMail,
    Rch,
    Git,
    Robot,
    BlockerRadar,
    ResourceCockpit,
    Manual,
    Fixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveSourceState {
    Available,
    Degraded,
    Unavailable,
    Stale,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveFreshnessState {
    Fresh,
    WithinBudget,
    Stale,
    Unknown,
    NotCollected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveRedactionPosture {
    Redacted,
    RedactedSummaryOnly,
    Unredacted,
    RawPaneContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveEvidenceCategory {
    BeadsReadyQueue,
    BeadsBlockedQueue,
    BeadsInProgress,
    ActiveAssigneeOverlap,
    AgentMailAvailability,
    RchWorkerSelection,
    RchActiveProjectExclusion,
    RchTopologyPreflight,
    RchCargoVerdict,
    GitDirtyTree,
    DirtyPathOverlap,
    CapacityPressure,
    RobotInventory,
    RedactionPosture,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionObjectiveEvidenceItem {
    pub category: MissionObjectiveEvidenceCategory,
    pub summary: String,
    pub reason_codes: Vec<String>,
}

impl MissionObjectiveEvidenceItem {
    #[must_use]
    pub fn new(category: MissionObjectiveEvidenceCategory, summary: impl Into<String>) -> Self {
        Self {
            category,
            summary: bounded_string(summary, "evidence unavailable"),
            reason_codes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionObjectiveSourceSnapshot {
    pub source_id: String,
    pub kind: MissionObjectiveSourceKind,
    pub state: MissionObjectiveSourceState,
    pub freshness_state: MissionObjectiveFreshnessState,
    pub redaction_posture: MissionObjectiveRedactionPosture,
    pub reason_codes: Vec<String>,
    pub evidence: Vec<MissionObjectiveEvidenceItem>,
}

impl MissionObjectiveSourceSnapshot {
    #[must_use]
    pub fn new(source_id: impl Into<String>, kind: MissionObjectiveSourceKind) -> Self {
        Self {
            source_id: bounded_string(source_id, "source.unknown"),
            kind,
            state: MissionObjectiveSourceState::Available,
            freshness_state: MissionObjectiveFreshnessState::Fresh,
            redaction_posture: MissionObjectiveRedactionPosture::Redacted,
            reason_codes: Vec::new(),
            evidence: Vec::new(),
        }
    }

    #[must_use]
    pub fn degraded(mut self, reason_code: impl Into<String>) -> Self {
        self.state = MissionObjectiveSourceState::Degraded;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn unavailable(mut self, reason_code: impl Into<String>) -> Self {
        self.state = MissionObjectiveSourceState::Unavailable;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn stale(mut self, reason_code: impl Into<String>) -> Self {
        self.freshness_state = MissionObjectiveFreshnessState::Stale;
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn redaction_posture(mut self, posture: MissionObjectiveRedactionPosture) -> Self {
        self.redaction_posture = posture;
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: MissionObjectiveEvidenceItem) -> Self {
        self.evidence.push(evidence);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveCandidateReadiness {
    ReadyBead,
    NoReadyFallback,
    ActiveSameDomain,
    StaleReopenCandidate,
    BlockedDependency,
    PlanningOnly,
    TestingSkillLane,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveProofAvailability {
    NotRequired,
    Available,
    Blocked,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveCapacityPosture {
    Admit,
    Defer,
    Pause,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveActionKind {
    ChooseReadyBead,
    WaitForOwner,
    StatusCheckBeforeReopen,
    AddBeadsComment,
    RunBvRobotTriage,
    RunTestingSkill,
    PlanningOnly,
    FileFollowupBead,
    WaitExternal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveSideEffectClass {
    None,
    CoordinationMutation,
    RemoteProof,
    PaneMutation,
    ServiceMutation,
    Destructive,
    LocalCargoProof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveProofLane {
    None,
    StaticSchema,
    RchCargo,
    Blocked,
    NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveApprovalRequirement {
    HumanApproval,
    OwnerHandoff,
    RchRecovered,
    CleanWorktree,
    AgentMailRecovered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectiveForbiddenAction {
    ServiceMutation,
    Destructive,
    LocalCargoProof,
    RawPaneContent,
    PaneMutation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionObjectiveDirtyPath {
    pub path: String,
    pub status: String,
    pub category: String,
}

impl MissionObjectiveDirtyPath {
    #[must_use]
    pub fn new(path: impl Into<String>, status: impl Into<String>) -> Self {
        Self {
            path: bounded_string(path, "unknown"),
            status: bounded_string(status, "dirty"),
            category: "dirty_tree".to_string(),
        }
    }

    #[must_use]
    pub fn category(mut self, category: impl Into<String>) -> Self {
        self.category = bounded_string(category, "dirty_tree");
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionObjectiveCandidateWork {
    pub candidate_id: String,
    pub title: String,
    pub readiness: MissionObjectiveCandidateReadiness,
    pub priority: u8,
    pub target_bead_id: Option<String>,
    pub owned_paths: Vec<String>,
    pub active_assignee: Option<String>,
    pub active_age_seconds: Option<u64>,
    pub stale_after_seconds: u64,
    pub dependency_ready: bool,
    pub proof_availability: MissionObjectiveProofAvailability,
    pub capacity_posture: MissionObjectiveCapacityPosture,
    pub reason_codes: Vec<String>,
}

impl MissionObjectiveCandidateWork {
    #[must_use]
    pub fn new(
        candidate_id: impl Into<String>,
        readiness: MissionObjectiveCandidateReadiness,
    ) -> Self {
        let candidate_id = bounded_string(candidate_id, "candidate.unknown");
        Self {
            title: candidate_id.clone(),
            candidate_id,
            readiness,
            priority: 2,
            target_bead_id: None,
            owned_paths: Vec::new(),
            active_assignee: None,
            active_age_seconds: None,
            stale_after_seconds: DEFAULT_STALE_AFTER_SECONDS,
            dependency_ready: true,
            proof_availability: MissionObjectiveProofAvailability::NotRequired,
            capacity_posture: MissionObjectiveCapacityPosture::Admit,
            reason_codes: Vec::new(),
        }
    }

    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = bounded_string(title, "untitled candidate");
        self
    }

    #[must_use]
    pub fn priority(mut self, priority: u8) -> Self {
        self.priority = priority.min(4);
        self
    }

    #[must_use]
    pub fn target_bead_id(mut self, bead_id: impl Into<String>) -> Self {
        self.target_bead_id = Some(bounded_string(bead_id, "unknown"));
        self
    }

    #[must_use]
    pub fn with_owned_path(mut self, path: impl Into<String>) -> Self {
        push_unique(&mut self.owned_paths, path);
        self
    }

    #[must_use]
    pub fn active_owner(mut self, assignee: impl Into<String>, age_seconds: u64) -> Self {
        self.active_assignee = Some(bounded_string(assignee, "unknown"));
        self.active_age_seconds = Some(age_seconds);
        self
    }

    #[must_use]
    pub fn stale_after_seconds(mut self, stale_after_seconds: u64) -> Self {
        self.stale_after_seconds = stale_after_seconds.max(60);
        self
    }

    #[must_use]
    pub fn dependency_ready(mut self, dependency_ready: bool) -> Self {
        self.dependency_ready = dependency_ready;
        self
    }

    #[must_use]
    pub fn proof_availability(
        mut self,
        proof_availability: MissionObjectiveProofAvailability,
    ) -> Self {
        self.proof_availability = proof_availability;
        self
    }

    #[must_use]
    pub fn capacity_posture(mut self, capacity_posture: MissionObjectiveCapacityPosture) -> Self {
        self.capacity_posture = capacity_posture;
        self
    }

    #[must_use]
    pub fn with_reason_code(mut self, reason_code: impl Into<String>) -> Self {
        push_unique(&mut self.reason_codes, reason_code);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionObjectivePlannerInput {
    pub generated_at_ms: u64,
    pub source: String,
    pub objective: String,
    pub strictness: MissionObjectiveStrictness,
    pub source_snapshots: Vec<MissionObjectiveSourceSnapshot>,
    pub dirty_paths: Vec<MissionObjectiveDirtyPath>,
    pub candidates: Vec<MissionObjectiveCandidateWork>,
}

impl MissionObjectivePlannerInput {
    #[must_use]
    pub fn new(
        generated_at_ms: u64,
        source: impl Into<String>,
        objective: impl Into<String>,
    ) -> Self {
        Self {
            generated_at_ms,
            source: bounded_string(source, "mission_objective_plan.core"),
            objective: bounded_string(objective, "unspecified objective"),
            strictness: MissionObjectiveStrictness::Normal,
            source_snapshots: Vec::new(),
            dirty_paths: Vec::new(),
            candidates: Vec::new(),
        }
    }

    #[must_use]
    pub fn strictness(mut self, strictness: MissionObjectiveStrictness) -> Self {
        self.strictness = strictness;
        self
    }

    #[must_use]
    pub fn with_source_snapshot(mut self, snapshot: MissionObjectiveSourceSnapshot) -> Self {
        self.source_snapshots.push(snapshot);
        self
    }

    #[must_use]
    pub fn with_dirty_path(mut self, dirty_path: MissionObjectiveDirtyPath) -> Self {
        self.dirty_paths.push(dirty_path);
        self
    }

    #[must_use]
    pub fn with_candidate(mut self, candidate: MissionObjectiveCandidateWork) -> Self {
        self.candidates.push(candidate);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionObjectivePlanStep {
    pub rank: u32,
    pub candidate_id: String,
    pub action_kind: MissionObjectiveActionKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_bead_id: Option<String>,
    pub score: i32,
    pub status: MissionObjectivePlanStatus,
    pub risk_level: MissionObjectiveRiskLevel,
    pub side_effect_class: MissionObjectiveSideEffectClass,
    pub forbidden: bool,
    pub required_approvals: Vec<MissionObjectiveApprovalRequirement>,
    pub proof_lane: MissionObjectiveProofLane,
    pub fallback_action: MissionObjectiveActionKind,
    pub affected_paths: Vec<String>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissionObjectivePlan {
    pub schema_version: u16,
    pub contract_id: String,
    pub generated_at_ms: u64,
    pub source: String,
    pub objective: String,
    pub strictness: MissionObjectiveStrictness,
    pub dry_run: bool,
    pub side_effects_executed: bool,
    pub raw_pane_content_stored: bool,
    pub plan_status: MissionObjectivePlanStatus,
    pub risk_level: MissionObjectiveRiskLevel,
    pub forbidden_actions: Vec<MissionObjectiveForbiddenAction>,
    pub source_snapshots: Vec<MissionObjectiveSourceSnapshot>,
    pub plan_steps: Vec<MissionObjectivePlanStep>,
    pub fallback_steps: Vec<MissionObjectivePlanStep>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionObjectivePlanExplainMode {
    Step,
    Reason,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionObjectivePlanExplainData {
    pub mode: MissionObjectivePlanExplainMode,
    pub query: String,
    pub matched: bool,
    pub matches: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<MissionObjectivePlanStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionObjectivePlanSurfaceData {
    pub contract_id: String,
    pub schema_version: u16,
    pub dry_run: bool,
    pub side_effects_executed: bool,
    pub raw_pane_content_stored: bool,
    pub plan_status: MissionObjectivePlanStatus,
    pub risk_level: MissionObjectiveRiskLevel,
    pub reason_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explain: Option<MissionObjectivePlanExplainData>,
    pub plan: MissionObjectivePlan,
}

impl Serialize for MissionObjectivePlan {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        MissionObjectivePlanArtifact::from_plan(self).serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectivePlanArtifact {
    schema_version: u16,
    contract_id: String,
    generated_at_ms: u64,
    objective: MissionObjectiveObjectiveArtifact,
    plan_status: MissionObjectivePlanStatus,
    source_snapshots: Vec<MissionObjectiveSourceSnapshotArtifact>,
    capacity_admission: MissionObjectiveCapacityAdmissionArtifact,
    steps: Vec<MissionObjectivePlanStepArtifact>,
    proof_requirements: Vec<MissionObjectiveProofRequirementArtifact>,
    forbidden_actions: Vec<MissionObjectiveForbiddenActionArtifact>,
    redaction_policy: MissionObjectiveRedactionPolicyArtifact,
    raw_pane_content_stored: bool,
    artifact_paths: Vec<String>,
}

impl MissionObjectivePlanArtifact {
    fn from_plan(plan: &MissionObjectivePlan) -> Self {
        let ordered_steps = ordered_steps(plan);
        let proof_requirements = proof_requirements_for_steps(&ordered_steps);
        let proof_strictness = proof_strictness_for_steps(&ordered_steps, plan.strictness);

        Self {
            schema_version: plan.schema_version,
            contract_id: plan.contract_id.clone(),
            generated_at_ms: plan.generated_at_ms,
            objective: MissionObjectiveObjectiveArtifact {
                objective_id: objective_id(plan),
                requested_by: plan.source.clone(),
                summary: plan.objective.clone(),
                workspace_root: "unknown".to_string(),
                target_bead_ids: target_bead_ids(&ordered_steps),
                allowed_action_classes: allowed_action_classes(&ordered_steps),
                forbidden_action_classes: forbidden_action_classes(&plan.forbidden_actions),
                freshness_budget_ms: 0,
                proof_strictness,
            },
            plan_status: plan.plan_status,
            source_snapshots: source_snapshot_artifacts(plan),
            capacity_admission: capacity_admission_artifact(plan, &ordered_steps),
            steps: ordered_steps
                .iter()
                .map(MissionObjectivePlanStepArtifact::from_step)
                .collect(),
            proof_requirements,
            forbidden_actions: plan
                .forbidden_actions
                .iter()
                .copied()
                .map(MissionObjectiveForbiddenActionArtifact::from_forbidden_action)
                .collect(),
            redaction_policy: MissionObjectiveRedactionPolicyArtifact::default(),
            raw_pane_content_stored: false,
            artifact_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveObjectiveArtifact {
    objective_id: String,
    requested_by: String,
    summary: String,
    workspace_root: String,
    target_bead_ids: Vec<String>,
    allowed_action_classes: Vec<String>,
    forbidden_action_classes: Vec<String>,
    freshness_budget_ms: u64,
    proof_strictness: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveSourceSnapshotArtifact {
    source_id: String,
    source_kind: MissionObjectiveSourceKind,
    state: MissionObjectiveSourceState,
    collected_at_ms: Option<u64>,
    freshness_ms: Option<u64>,
    freshness_state: MissionObjectiveFreshnessState,
    command_or_api: String,
    redacted: bool,
    redaction_posture: MissionObjectiveRedactionPostureArtifact,
    raw_pane_content_stored: bool,
    reason_codes: Vec<String>,
    evidence: Vec<MissionObjectiveSourceEvidenceArtifact>,
    artifact_paths: Vec<String>,
}

impl MissionObjectiveSourceSnapshotArtifact {
    fn from_snapshot(snapshot: &MissionObjectiveSourceSnapshot) -> Self {
        let mut evidence = snapshot
            .evidence
            .iter()
            .enumerate()
            .map(|(index, item)| {
                MissionObjectiveSourceEvidenceArtifact::from_evidence(snapshot, item, index)
            })
            .collect::<Vec<_>>();

        if evidence.is_empty() {
            evidence.push(MissionObjectiveSourceEvidenceArtifact {
                evidence_id: format!("evidence-{}-summary", snapshot.source_id),
                category: "manual_note".to_string(),
                state: source_evidence_state(snapshot),
                subject: snapshot.source_id.clone(),
                reason_codes: snapshot.reason_codes.clone(),
                artifact_paths: Vec::new(),
            });
        }

        Self {
            source_id: snapshot.source_id.clone(),
            source_kind: snapshot.kind,
            state: snapshot.state,
            collected_at_ms: None,
            freshness_ms: None,
            freshness_state: snapshot.freshness_state,
            command_or_api: "mission_objective_plan.core".to_string(),
            redacted: true,
            redaction_posture: MissionObjectiveRedactionPostureArtifact::from_posture(
                snapshot.redaction_posture,
            ),
            raw_pane_content_stored: false,
            reason_codes: snapshot.reason_codes.clone(),
            evidence,
            artifact_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveRedactionPostureArtifact {
    pane_content: String,
    secret_material: String,
    notes: Vec<String>,
}

impl MissionObjectiveRedactionPostureArtifact {
    fn from_posture(posture: MissionObjectiveRedactionPosture) -> Self {
        match posture {
            MissionObjectiveRedactionPosture::Redacted => Self {
                pane_content: "not_collected".to_string(),
                secret_material: "not_collected".to_string(),
                notes: vec!["Planner source snapshot is a redacted summary.".to_string()],
            },
            MissionObjectiveRedactionPosture::RedactedSummaryOnly => Self {
                pane_content: "redacted".to_string(),
                secret_material: "redacted".to_string(),
                notes: vec!["Planner source snapshot omits raw transcript text.".to_string()],
            },
            MissionObjectiveRedactionPosture::Unredacted
            | MissionObjectiveRedactionPosture::RawPaneContent => Self {
                pane_content: "raw_forbidden".to_string(),
                secret_material: "raw_forbidden".to_string(),
                notes: vec!["Raw pane content and secret material are forbidden.".to_string()],
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveSourceEvidenceArtifact {
    evidence_id: String,
    category: String,
    state: String,
    subject: String,
    reason_codes: Vec<String>,
    artifact_paths: Vec<String>,
}

impl MissionObjectiveSourceEvidenceArtifact {
    fn from_evidence(
        snapshot: &MissionObjectiveSourceSnapshot,
        evidence: &MissionObjectiveEvidenceItem,
        index: usize,
    ) -> Self {
        Self {
            evidence_id: format!("evidence-{}-{}", snapshot.source_id, index + 1),
            category: evidence_category_name(evidence.category).to_string(),
            state: source_evidence_state(snapshot),
            subject: evidence.summary.clone(),
            reason_codes: evidence.reason_codes.clone(),
            artifact_paths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveCapacityAdmissionArtifact {
    action: String,
    pressure_tier: String,
    proof_state: String,
    reason_codes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectivePlanStepArtifact {
    step_id: String,
    rank: u32,
    action_kind: String,
    status: MissionObjectivePlanStatus,
    target: String,
    side_effect_class: String,
    requires_approval: bool,
    forbidden: bool,
    reason_codes: Vec<String>,
    proof_requirement_ids: Vec<String>,
}

impl MissionObjectivePlanStepArtifact {
    fn from_step(step: &MissionObjectivePlanStep) -> Self {
        Self {
            step_id: format!("step-{}-{}", step.rank, step.candidate_id),
            rank: step.rank,
            action_kind: step_action_kind_name(step.action_kind).to_string(),
            status: step.status,
            target: step
                .target_bead_id
                .clone()
                .unwrap_or_else(|| step.candidate_id.clone()),
            side_effect_class: step_side_effect_class_name(step.side_effect_class).to_string(),
            requires_approval: !step.required_approvals.is_empty(),
            forbidden: step.forbidden,
            reason_codes: step.reason_codes.clone(),
            proof_requirement_ids: proof_requirement_ids(step.proof_lane),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveProofRequirementArtifact {
    proof_id: String,
    proof_kind: String,
    required: bool,
    status: String,
    command: String,
    artifact_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveForbiddenActionArtifact {
    action_class: String,
    reason: String,
}

impl MissionObjectiveForbiddenActionArtifact {
    fn from_forbidden_action(action: MissionObjectiveForbiddenAction) -> Self {
        let (action_class, reason) = match action {
            MissionObjectiveForbiddenAction::ServiceMutation => (
                "service_mutation",
                "Objective planning must not restart or repair Agent Mail or RCH.",
            ),
            MissionObjectiveForbiddenAction::Destructive => (
                "destructive_filesystem",
                "Objective planning must not delete or overwrite files.",
            ),
            MissionObjectiveForbiddenAction::LocalCargoProof => (
                "local_cargo_proof",
                "Local Cargo fallback is forbidden; Rust proof must run through RCH.",
            ),
            MissionObjectiveForbiddenAction::RawPaneContent => (
                "raw_pane_content",
                "Planner status reads must not collect or store raw pane content.",
            ),
            MissionObjectiveForbiddenAction::PaneMutation => (
                "pane_mutation",
                "Objective planning is read-only and cannot inject pane input.",
            ),
        };
        Self {
            action_class: action_class.to_string(),
            reason: reason.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct MissionObjectiveRedactionPolicyArtifact {
    policy_id: String,
    raw_pane_content_allowed: bool,
    secret_material_allowed: bool,
    notes: Vec<String>,
}

impl Default for MissionObjectiveRedactionPolicyArtifact {
    fn default() -> Self {
        Self {
            policy_id: "ft.redaction.objective_plan.v1".to_string(),
            raw_pane_content_allowed: false,
            secret_material_allowed: false,
            notes: vec![
                "Mission objective plans store source summaries, ids, and reason codes only."
                    .to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourcePosture {
    degraded: bool,
    unavailable: bool,
    redaction_violation: bool,
    agent_mail_unavailable: bool,
    rch_unavailable: bool,
    reason_codes: Vec<String>,
}

/// Build a deterministic, dry-run mission objective plan.
#[must_use]
pub fn plan_mission_objective(input: &MissionObjectivePlannerInput) -> MissionObjectivePlan {
    let source_posture = source_posture(input);
    let forbidden_actions = default_forbidden_actions();

    let mut plan_steps = if source_posture.redaction_violation {
        vec![unavailable_redaction_step(&source_posture)]
    } else if input.candidates.is_empty() {
        Vec::new()
    } else {
        input
            .candidates
            .iter()
            .map(|candidate| plan_candidate_step(candidate, input, &source_posture))
            .collect::<Vec<_>>()
    };

    plan_steps.sort_by(compare_plan_steps);
    for (index, step) in plan_steps.iter_mut().enumerate() {
        step.rank = usize_to_u32(index + 1);
    }

    let fallback_steps = if input.candidates.is_empty() {
        no_ready_fallback_steps(input.generated_at_ms)
    } else {
        Vec::new()
    };

    let plan_status = plan_status(
        &plan_steps,
        &fallback_steps,
        &source_posture,
        input.strictness,
    );
    let risk_level = plan_risk_level(&plan_steps, &fallback_steps, &source_posture);
    let reason_codes = plan_reason_codes(
        &plan_steps,
        &fallback_steps,
        &source_posture,
        plan_status,
        risk_level,
    );

    MissionObjectivePlan {
        schema_version: MISSION_OBJECTIVE_PLAN_SCHEMA_VERSION,
        contract_id: MISSION_OBJECTIVE_PLAN_CONTRACT_ID.to_string(),
        generated_at_ms: input.generated_at_ms,
        source: input.source.clone(),
        objective: input.objective.clone(),
        strictness: input.strictness,
        dry_run: true,
        side_effects_executed: false,
        raw_pane_content_stored: false,
        plan_status,
        risk_level,
        forbidden_actions,
        source_snapshots: input.source_snapshots.clone(),
        plan_steps,
        fallback_steps,
        reason_codes,
    }
}

#[must_use]
pub fn build_mission_objective_plan_surface_data(
    plan: MissionObjectivePlan,
    explain_step: Option<&str>,
    explain_reason: Option<&str>,
) -> MissionObjectivePlanSurfaceData {
    let explain = explain_step
        .map(|query| explain_plan_step(&plan, query))
        .or_else(|| explain_reason.map(|query| explain_plan_reason(&plan, query)));

    MissionObjectivePlanSurfaceData {
        contract_id: plan.contract_id.clone(),
        schema_version: plan.schema_version,
        dry_run: plan.dry_run,
        side_effects_executed: plan.side_effects_executed,
        raw_pane_content_stored: plan.raw_pane_content_stored,
        plan_status: plan.plan_status,
        risk_level: plan.risk_level,
        reason_codes: plan.reason_codes.clone(),
        explain,
        plan,
    }
}

fn explain_plan_step(plan: &MissionObjectivePlan, query: &str) -> MissionObjectivePlanExplainData {
    let query = query.trim();
    let step = ordered_steps(plan).into_iter().find(|step| {
        step.candidate_id == query
            || step
                .target_bead_id
                .as_deref()
                .is_some_and(|target| target == query)
            || step.rank.to_string() == query
    });
    let matches = step
        .as_ref()
        .map(|step| {
            vec![
                format!("step.rank:{}", step.rank),
                format!("step.candidate_id:{}", step.candidate_id),
            ]
        })
        .unwrap_or_default();

    MissionObjectivePlanExplainData {
        mode: MissionObjectivePlanExplainMode::Step,
        query: query.to_string(),
        matched: step.is_some(),
        matches,
        step,
    }
}

fn explain_plan_reason(
    plan: &MissionObjectivePlan,
    query: &str,
) -> MissionObjectivePlanExplainData {
    let query = query.trim();
    let mut matches = Vec::new();
    if plan.reason_codes.iter().any(|reason| reason == query) {
        matches.push("plan.reason_codes".to_string());
    }
    for snapshot in &plan.source_snapshots {
        if snapshot.reason_codes.iter().any(|reason| reason == query) {
            matches.push(format!("source_snapshots.{}", snapshot.source_id));
        }
        for (index, evidence) in snapshot.evidence.iter().enumerate() {
            if evidence.reason_codes.iter().any(|reason| reason == query) {
                matches.push(format!(
                    "source_snapshots.{}.evidence.{}",
                    snapshot.source_id, index
                ));
            }
        }
    }
    for step in ordered_steps(plan) {
        if step.reason_codes.iter().any(|reason| reason == query) {
            matches.push(format!("steps.{}.reason_codes", step.candidate_id));
        }
    }

    MissionObjectivePlanExplainData {
        mode: MissionObjectivePlanExplainMode::Reason,
        query: query.to_string(),
        matched: !matches.is_empty(),
        matches,
        step: None,
    }
}

fn source_posture(input: &MissionObjectivePlannerInput) -> SourcePosture {
    let mut posture = SourcePosture {
        degraded: false,
        unavailable: false,
        redaction_violation: false,
        agent_mail_unavailable: false,
        rch_unavailable: false,
        reason_codes: Vec::new(),
    };

    for snapshot in &input.source_snapshots {
        match snapshot.state {
            MissionObjectiveSourceState::Available => {}
            MissionObjectiveSourceState::Degraded | MissionObjectiveSourceState::Stale => {
                posture.degraded = true;
            }
            MissionObjectiveSourceState::Unavailable | MissionObjectiveSourceState::Blocked => {
                posture.unavailable = true;
            }
        }
        if freshness_degrades_plan(snapshot.freshness_state) {
            posture.degraded = true;
            push_unique(
                &mut posture.reason_codes,
                format!(
                    "source.{}.{}",
                    snapshot.source_id,
                    freshness_reason(snapshot)
                ),
            );
        }
        if matches!(
            snapshot.redaction_posture,
            MissionObjectiveRedactionPosture::Unredacted
                | MissionObjectiveRedactionPosture::RawPaneContent
        ) {
            posture.redaction_violation = true;
            push_unique(
                &mut posture.reason_codes,
                "redaction.raw_pane_content_forbidden",
            );
        }
        if snapshot.kind == MissionObjectiveSourceKind::AgentMail
            && snapshot.state != MissionObjectiveSourceState::Available
        {
            posture.agent_mail_unavailable = true;
            push_unique(&mut posture.reason_codes, "agent_mail.fallback_required");
        }
        if snapshot.kind == MissionObjectiveSourceKind::Rch
            && snapshot.state != MissionObjectiveSourceState::Available
        {
            posture.rch_unavailable = true;
            push_unique(&mut posture.reason_codes, "rch.proof_substrate_unavailable");
        }
        for reason_code in &snapshot.reason_codes {
            push_unique(&mut posture.reason_codes, reason_code.clone());
            if snapshot.kind == MissionObjectiveSourceKind::Rch
                && reason_code_contains_blocker(reason_code)
            {
                posture.rch_unavailable = true;
            }
        }
    }

    posture
}

fn freshness_reason(snapshot: &MissionObjectiveSourceSnapshot) -> &'static str {
    match snapshot.freshness_state {
        MissionObjectiveFreshnessState::Fresh => "fresh",
        MissionObjectiveFreshnessState::WithinBudget => "within_budget",
        MissionObjectiveFreshnessState::Stale => "stale",
        MissionObjectiveFreshnessState::Unknown => "freshness_unknown",
        MissionObjectiveFreshnessState::NotCollected => "not_collected",
    }
}

fn freshness_degrades_plan(freshness_state: MissionObjectiveFreshnessState) -> bool {
    matches!(
        freshness_state,
        MissionObjectiveFreshnessState::Stale
            | MissionObjectiveFreshnessState::Unknown
            | MissionObjectiveFreshnessState::NotCollected
    )
}

fn reason_code_contains_blocker(reason_code: &str) -> bool {
    let code = reason_code.to_ascii_lowercase();
    code.contains("blocked") || code.contains("unavailable") || code.contains("degraded")
}

fn plan_candidate_step(
    candidate: &MissionObjectiveCandidateWork,
    input: &MissionObjectivePlannerInput,
    source_posture: &SourcePosture,
) -> MissionObjectivePlanStep {
    let dirty_overlaps = dirty_overlaps(candidate, &input.dirty_paths);
    let stale_owner = active_owner_is_stale(candidate);
    let proof_blocked = candidate.proof_availability == MissionObjectiveProofAvailability::Blocked
        || candidate.proof_availability == MissionObjectiveProofAvailability::Unavailable
        || (candidate.proof_availability == MissionObjectiveProofAvailability::Available
            && source_posture.rch_unavailable);

    let mut reason_codes = candidate.reason_codes.clone();
    push_unique(&mut reason_codes, readiness_reason(candidate.readiness));
    if source_posture.agent_mail_unavailable {
        push_unique(&mut reason_codes, "agent_mail.beads_fallback_available");
    }
    if !dirty_overlaps.is_empty() {
        push_unique(&mut reason_codes, "dirty_overlap.owned_surface_blocked");
    }
    if !candidate.dependency_ready {
        push_unique(&mut reason_codes, "beads.dependency_not_ready");
    }
    if stale_owner {
        push_unique(&mut reason_codes, "beads.status_check_before_reopen");
    }
    if proof_blocked {
        push_unique(&mut reason_codes, "rch.proof_substrate_blocked");
    }

    let status = candidate_status(
        candidate,
        !dirty_overlaps.is_empty(),
        stale_owner,
        proof_blocked,
    );
    let action_kind = action_kind_for_status(candidate, status, stale_owner);
    let side_effect_class = side_effect_class(action_kind, candidate.proof_availability);
    let forbidden = side_effect_forbidden(side_effect_class);
    let proof_lane = proof_lane(candidate.proof_availability, proof_blocked);
    let required_approvals = required_approvals(
        candidate,
        status,
        &dirty_overlaps,
        source_posture,
        proof_blocked,
    );
    let risk_level = step_risk_level(status, side_effect_class, &required_approvals);
    let score = score_candidate(candidate, status, risk_level, proof_blocked);

    MissionObjectivePlanStep {
        rank: 0,
        candidate_id: candidate.candidate_id.clone(),
        action_kind,
        title: candidate.title.clone(),
        target_bead_id: candidate.target_bead_id.clone(),
        score,
        status,
        risk_level,
        side_effect_class,
        forbidden,
        required_approvals,
        proof_lane,
        fallback_action: fallback_action(status),
        affected_paths: dirty_overlaps,
        reason_codes,
    }
}

fn candidate_status(
    candidate: &MissionObjectiveCandidateWork,
    has_dirty_overlap: bool,
    stale_owner: bool,
    proof_blocked: bool,
) -> MissionObjectivePlanStatus {
    if has_dirty_overlap {
        return MissionObjectivePlanStatus::DirtyOverlap;
    }
    if candidate.readiness == MissionObjectiveCandidateReadiness::ActiveSameDomain && !stale_owner {
        return MissionObjectivePlanStatus::WaitingOwner;
    }
    if candidate.readiness == MissionObjectiveCandidateReadiness::StaleReopenCandidate
        || stale_owner
    {
        return MissionObjectivePlanStatus::WaitingOwner;
    }
    if !candidate.dependency_ready
        || candidate.readiness == MissionObjectiveCandidateReadiness::BlockedDependency
    {
        return MissionObjectivePlanStatus::Blocked;
    }
    if proof_blocked {
        return MissionObjectivePlanStatus::RchSubstrateBlocked;
    }
    match candidate.capacity_posture {
        MissionObjectiveCapacityPosture::Pause | MissionObjectiveCapacityPosture::Defer => {
            return MissionObjectivePlanStatus::WaitingExternal;
        }
        MissionObjectiveCapacityPosture::Unknown => return MissionObjectivePlanStatus::Degraded,
        MissionObjectiveCapacityPosture::Admit => {}
    }
    match candidate.readiness {
        MissionObjectiveCandidateReadiness::ReadyBead => MissionObjectivePlanStatus::Actionable,
        MissionObjectiveCandidateReadiness::NoReadyFallback => {
            MissionObjectivePlanStatus::NoReadyWork
        }
        MissionObjectiveCandidateReadiness::PlanningOnly
        | MissionObjectiveCandidateReadiness::TestingSkillLane => {
            MissionObjectivePlanStatus::PlanningOnly
        }
        MissionObjectiveCandidateReadiness::ActiveSameDomain
        | MissionObjectiveCandidateReadiness::StaleReopenCandidate
        | MissionObjectiveCandidateReadiness::BlockedDependency => {
            MissionObjectivePlanStatus::Blocked
        }
    }
}

fn action_kind_for_status(
    candidate: &MissionObjectiveCandidateWork,
    status: MissionObjectivePlanStatus,
    stale_owner: bool,
) -> MissionObjectiveActionKind {
    if status == MissionObjectivePlanStatus::DirtyOverlap {
        return MissionObjectiveActionKind::WaitForOwner;
    }
    if stale_owner
        || candidate.readiness == MissionObjectiveCandidateReadiness::StaleReopenCandidate
    {
        return MissionObjectiveActionKind::StatusCheckBeforeReopen;
    }
    match status {
        MissionObjectivePlanStatus::Actionable => MissionObjectiveActionKind::ChooseReadyBead,
        MissionObjectivePlanStatus::WaitingOwner | MissionObjectivePlanStatus::DirtyOverlap => {
            MissionObjectiveActionKind::WaitForOwner
        }
        MissionObjectivePlanStatus::RchSubstrateBlocked | MissionObjectivePlanStatus::Blocked => {
            MissionObjectiveActionKind::FileFollowupBead
        }
        MissionObjectivePlanStatus::NoReadyWork => MissionObjectiveActionKind::RunBvRobotTriage,
        MissionObjectivePlanStatus::PlanningOnly | MissionObjectivePlanStatus::Degraded => {
            if candidate.readiness == MissionObjectiveCandidateReadiness::TestingSkillLane {
                MissionObjectiveActionKind::RunTestingSkill
            } else {
                MissionObjectiveActionKind::PlanningOnly
            }
        }
        MissionObjectivePlanStatus::WaitingExternal => MissionObjectiveActionKind::WaitExternal,
        MissionObjectivePlanStatus::Unavailable => MissionObjectiveActionKind::PlanningOnly,
    }
}

fn side_effect_class(
    action_kind: MissionObjectiveActionKind,
    proof_availability: MissionObjectiveProofAvailability,
) -> MissionObjectiveSideEffectClass {
    match action_kind {
        MissionObjectiveActionKind::AddBeadsComment
        | MissionObjectiveActionKind::StatusCheckBeforeReopen => {
            MissionObjectiveSideEffectClass::CoordinationMutation
        }
        MissionObjectiveActionKind::ChooseReadyBead
            if proof_availability == MissionObjectiveProofAvailability::Available =>
        {
            MissionObjectiveSideEffectClass::RemoteProof
        }
        _ => MissionObjectiveSideEffectClass::None,
    }
}

fn side_effect_forbidden(side_effect_class: MissionObjectiveSideEffectClass) -> bool {
    matches!(
        side_effect_class,
        MissionObjectiveSideEffectClass::PaneMutation
            | MissionObjectiveSideEffectClass::ServiceMutation
            | MissionObjectiveSideEffectClass::Destructive
            | MissionObjectiveSideEffectClass::LocalCargoProof
    )
}

fn proof_lane(
    proof_availability: MissionObjectiveProofAvailability,
    proof_blocked: bool,
) -> MissionObjectiveProofLane {
    if proof_blocked {
        return MissionObjectiveProofLane::Blocked;
    }
    match proof_availability {
        MissionObjectiveProofAvailability::NotRequired => MissionObjectiveProofLane::NotRequired,
        MissionObjectiveProofAvailability::Available => MissionObjectiveProofLane::RchCargo,
        MissionObjectiveProofAvailability::Blocked
        | MissionObjectiveProofAvailability::Unavailable => MissionObjectiveProofLane::Blocked,
    }
}

fn required_approvals(
    candidate: &MissionObjectiveCandidateWork,
    status: MissionObjectivePlanStatus,
    dirty_overlaps: &[String],
    source_posture: &SourcePosture,
    proof_blocked: bool,
) -> Vec<MissionObjectiveApprovalRequirement> {
    let mut approvals = Vec::new();
    if !dirty_overlaps.is_empty() {
        push_unique(
            &mut approvals,
            MissionObjectiveApprovalRequirement::CleanWorktree,
        );
    }
    if matches!(
        status,
        MissionObjectivePlanStatus::WaitingOwner | MissionObjectivePlanStatus::DirtyOverlap
    ) || candidate.active_assignee.is_some()
    {
        push_unique(
            &mut approvals,
            MissionObjectiveApprovalRequirement::OwnerHandoff,
        );
    }
    if proof_blocked {
        push_unique(
            &mut approvals,
            MissionObjectiveApprovalRequirement::RchRecovered,
        );
    }
    if source_posture.agent_mail_unavailable {
        push_unique(
            &mut approvals,
            MissionObjectiveApprovalRequirement::AgentMailRecovered,
        );
    }
    approvals
}

fn step_risk_level(
    status: MissionObjectivePlanStatus,
    side_effect_class: MissionObjectiveSideEffectClass,
    required_approvals: &[MissionObjectiveApprovalRequirement],
) -> MissionObjectiveRiskLevel {
    if matches!(
        status,
        MissionObjectivePlanStatus::Blocked
            | MissionObjectivePlanStatus::DirtyOverlap
            | MissionObjectivePlanStatus::RchSubstrateBlocked
            | MissionObjectivePlanStatus::Unavailable
    ) || side_effect_forbidden(side_effect_class)
    {
        return MissionObjectiveRiskLevel::Blocked;
    }
    if matches!(
        status,
        MissionObjectivePlanStatus::WaitingOwner | MissionObjectivePlanStatus::WaitingExternal
    ) {
        return MissionObjectiveRiskLevel::High;
    }
    if !required_approvals.is_empty() || status == MissionObjectivePlanStatus::Degraded {
        return MissionObjectiveRiskLevel::Medium;
    }
    MissionObjectiveRiskLevel::Low
}

fn score_candidate(
    candidate: &MissionObjectiveCandidateWork,
    status: MissionObjectivePlanStatus,
    risk_level: MissionObjectiveRiskLevel,
    proof_blocked: bool,
) -> i32 {
    let mut score = match candidate.readiness {
        MissionObjectiveCandidateReadiness::ReadyBead => 1_000,
        MissionObjectiveCandidateReadiness::TestingSkillLane => 520,
        MissionObjectiveCandidateReadiness::NoReadyFallback => 500,
        MissionObjectiveCandidateReadiness::PlanningOnly => 460,
        MissionObjectiveCandidateReadiness::StaleReopenCandidate => 430,
        MissionObjectiveCandidateReadiness::ActiveSameDomain => 300,
        MissionObjectiveCandidateReadiness::BlockedDependency => 100,
    };
    score -= i32::from(candidate.priority.min(4)) * 20;
    score -= match risk_level {
        MissionObjectiveRiskLevel::Low => 0,
        MissionObjectiveRiskLevel::Medium => 80,
        MissionObjectiveRiskLevel::High => 250,
        MissionObjectiveRiskLevel::Blocked => 600,
    };
    if proof_blocked {
        score -= 150;
    }
    if status == MissionObjectivePlanStatus::Actionable
        && candidate.proof_availability == MissionObjectiveProofAvailability::NotRequired
    {
        score += 20;
    }
    score
}

fn compare_plan_steps(
    left: &MissionObjectivePlanStep,
    right: &MissionObjectivePlanStep,
) -> Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.candidate_id.cmp(&right.candidate_id))
        .then_with(|| left.title.cmp(&right.title))
}

fn dirty_overlaps(
    candidate: &MissionObjectiveCandidateWork,
    dirty_paths: &[MissionObjectiveDirtyPath],
) -> Vec<String> {
    let mut overlaps = Vec::new();
    for dirty_path in dirty_paths {
        if candidate
            .owned_paths
            .iter()
            .any(|owned_path| paths_overlap(owned_path, &dirty_path.path))
        {
            push_unique(&mut overlaps, dirty_path.path.clone());
        }
    }
    overlaps
}

fn paths_overlap(left: &str, right: &str) -> bool {
    let left = left.trim();
    let right = right.trim();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    if left == right {
        return true;
    }
    let left_prefix = format!("{left}/");
    let right_prefix = format!("{right}/");
    if right.starts_with(&left_prefix) || left.starts_with(&right_prefix) {
        return true;
    }
    if left.ends_with('/') {
        return right.starts_with(left);
    }
    if right.ends_with('/') {
        return left.starts_with(right);
    }
    if let Some(prefix) = left.strip_suffix("/*") {
        return right.starts_with(prefix)
            && right
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'));
    }
    if let Some(prefix) = right.strip_suffix("/*") {
        return left.starts_with(prefix)
            && left
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.starts_with('/'));
    }
    false
}

fn active_owner_is_stale(candidate: &MissionObjectiveCandidateWork) -> bool {
    candidate
        .active_age_seconds
        .is_some_and(|age| age >= candidate.stale_after_seconds)
}

fn readiness_reason(readiness: MissionObjectiveCandidateReadiness) -> &'static str {
    match readiness {
        MissionObjectiveCandidateReadiness::ReadyBead => "beads.ready_available",
        MissionObjectiveCandidateReadiness::NoReadyFallback => "beads.no_ready_work",
        MissionObjectiveCandidateReadiness::ActiveSameDomain => "beads.owner_active",
        MissionObjectiveCandidateReadiness::StaleReopenCandidate => "beads.stale_reopen_candidate",
        MissionObjectiveCandidateReadiness::BlockedDependency => "beads.dependency_blocked",
        MissionObjectiveCandidateReadiness::PlanningOnly => "planner.planning_only",
        MissionObjectiveCandidateReadiness::TestingSkillLane => "planner.testing_skill_lane",
    }
}

fn fallback_action(status: MissionObjectivePlanStatus) -> MissionObjectiveActionKind {
    match status {
        MissionObjectivePlanStatus::Actionable | MissionObjectivePlanStatus::Degraded => {
            MissionObjectiveActionKind::RunBvRobotTriage
        }
        MissionObjectivePlanStatus::NoReadyWork => MissionObjectiveActionKind::RunTestingSkill,
        MissionObjectivePlanStatus::WaitingOwner | MissionObjectivePlanStatus::DirtyOverlap => {
            MissionObjectiveActionKind::WaitForOwner
        }
        MissionObjectivePlanStatus::RchSubstrateBlocked
        | MissionObjectivePlanStatus::WaitingExternal => MissionObjectiveActionKind::WaitExternal,
        MissionObjectivePlanStatus::Blocked
        | MissionObjectivePlanStatus::PlanningOnly
        | MissionObjectivePlanStatus::Unavailable => MissionObjectiveActionKind::PlanningOnly,
    }
}

fn unavailable_redaction_step(source_posture: &SourcePosture) -> MissionObjectivePlanStep {
    let mut reason_codes = source_posture.reason_codes.clone();
    push_unique(&mut reason_codes, "redaction.fail_closed");
    MissionObjectivePlanStep {
        rank: 1,
        candidate_id: "redaction.fail_closed".to_string(),
        action_kind: MissionObjectiveActionKind::PlanningOnly,
        title: "Reject unredacted objective-plan source snapshot".to_string(),
        target_bead_id: None,
        score: -1_000,
        status: MissionObjectivePlanStatus::Unavailable,
        risk_level: MissionObjectiveRiskLevel::Blocked,
        side_effect_class: MissionObjectiveSideEffectClass::None,
        forbidden: false,
        required_approvals: vec![MissionObjectiveApprovalRequirement::HumanApproval],
        proof_lane: MissionObjectiveProofLane::None,
        fallback_action: MissionObjectiveActionKind::PlanningOnly,
        affected_paths: Vec::new(),
        reason_codes,
    }
}

fn no_ready_fallback_steps(generated_at_ms: u64) -> Vec<MissionObjectivePlanStep> {
    let suffix = generated_at_ms.to_string();
    let mut steps = vec![
        fallback_step(
            format!("fallback.bv_robot_triage.{suffix}"),
            MissionObjectiveActionKind::RunBvRobotTriage,
            "Refresh Beads ready queue with robot triage",
            300,
            "beads.robot_triage_fallback",
        ),
        fallback_step(
            format!("fallback.testing_skill.{suffix}"),
            MissionObjectiveActionKind::RunTestingSkill,
            "Run a planning-only testing skill lane",
            260,
            "planner.testing_skill_fallback",
        ),
        fallback_step(
            format!("fallback.planning_only.{suffix}"),
            MissionObjectiveActionKind::PlanningOnly,
            "File or refine planning-only work",
            220,
            "planner.planning_only_fallback",
        ),
    ];
    for (index, step) in steps.iter_mut().enumerate() {
        step.rank = usize_to_u32(index + 1);
    }
    steps
}

fn fallback_step(
    candidate_id: String,
    action_kind: MissionObjectiveActionKind,
    title: &str,
    score: i32,
    reason_code: &str,
) -> MissionObjectivePlanStep {
    MissionObjectivePlanStep {
        rank: 0,
        candidate_id,
        action_kind,
        title: title.to_string(),
        target_bead_id: None,
        score,
        status: MissionObjectivePlanStatus::NoReadyWork,
        risk_level: MissionObjectiveRiskLevel::Medium,
        side_effect_class: MissionObjectiveSideEffectClass::None,
        forbidden: false,
        required_approvals: Vec::new(),
        proof_lane: MissionObjectiveProofLane::NotRequired,
        fallback_action: MissionObjectiveActionKind::PlanningOnly,
        affected_paths: Vec::new(),
        reason_codes: vec![reason_code.to_string()],
    }
}

fn plan_status(
    plan_steps: &[MissionObjectivePlanStep],
    fallback_steps: &[MissionObjectivePlanStep],
    source_posture: &SourcePosture,
    strictness: MissionObjectiveStrictness,
) -> MissionObjectivePlanStatus {
    if source_posture.redaction_violation {
        return MissionObjectivePlanStatus::Unavailable;
    }
    if strictness == MissionObjectiveStrictness::Strict
        && (source_posture.unavailable || source_posture.degraded)
    {
        return MissionObjectivePlanStatus::Unavailable;
    }
    if let Some(first) = plan_steps.first() {
        if first.status == MissionObjectivePlanStatus::Actionable
            && (source_posture.unavailable || source_posture.degraded)
        {
            return MissionObjectivePlanStatus::Degraded;
        }
        return first.status;
    }
    if !fallback_steps.is_empty() {
        return MissionObjectivePlanStatus::NoReadyWork;
    }
    MissionObjectivePlanStatus::Unavailable
}

fn plan_risk_level(
    plan_steps: &[MissionObjectivePlanStep],
    fallback_steps: &[MissionObjectivePlanStep],
    source_posture: &SourcePosture,
) -> MissionObjectiveRiskLevel {
    if source_posture.redaction_violation {
        return MissionObjectiveRiskLevel::Blocked;
    }
    plan_steps
        .iter()
        .chain(fallback_steps)
        .map(|step| step.risk_level)
        .max()
        .unwrap_or(MissionObjectiveRiskLevel::Blocked)
}

fn plan_reason_codes(
    plan_steps: &[MissionObjectivePlanStep],
    fallback_steps: &[MissionObjectivePlanStep],
    source_posture: &SourcePosture,
    plan_status: MissionObjectivePlanStatus,
    risk_level: MissionObjectiveRiskLevel,
) -> Vec<String> {
    let mut reason_codes = Vec::new();
    push_unique(&mut reason_codes, "planner.dry_run_only");
    push_unique(
        &mut reason_codes,
        format!("planner.status.{}", plan_status_reason_suffix(plan_status)),
    );
    push_unique(
        &mut reason_codes,
        format!("planner.risk.{}", risk_level_reason_suffix(risk_level)),
    );
    for reason_code in &source_posture.reason_codes {
        push_unique(&mut reason_codes, reason_code.clone());
    }
    for step in plan_steps.iter().chain(fallback_steps) {
        for reason_code in &step.reason_codes {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }
    reason_codes
}

fn plan_status_reason_suffix(status: MissionObjectivePlanStatus) -> &'static str {
    match status {
        MissionObjectivePlanStatus::Actionable => "actionable",
        MissionObjectivePlanStatus::PlanningOnly => "planning_only",
        MissionObjectivePlanStatus::Blocked => "blocked",
        MissionObjectivePlanStatus::WaitingOwner => "waiting_owner",
        MissionObjectivePlanStatus::WaitingExternal => "waiting_external",
        MissionObjectivePlanStatus::DirtyOverlap => "dirty_overlap",
        MissionObjectivePlanStatus::NoReadyWork => "no_ready_work",
        MissionObjectivePlanStatus::RchSubstrateBlocked => "rch_substrate_blocked",
        MissionObjectivePlanStatus::Degraded => "degraded",
        MissionObjectivePlanStatus::Unavailable => "unavailable",
    }
}

fn risk_level_reason_suffix(risk_level: MissionObjectiveRiskLevel) -> &'static str {
    match risk_level {
        MissionObjectiveRiskLevel::Low => "low",
        MissionObjectiveRiskLevel::Medium => "medium",
        MissionObjectiveRiskLevel::High => "high",
        MissionObjectiveRiskLevel::Blocked => "blocked",
    }
}

fn default_forbidden_actions() -> Vec<MissionObjectiveForbiddenAction> {
    vec![
        MissionObjectiveForbiddenAction::ServiceMutation,
        MissionObjectiveForbiddenAction::Destructive,
        MissionObjectiveForbiddenAction::LocalCargoProof,
        MissionObjectiveForbiddenAction::RawPaneContent,
        MissionObjectiveForbiddenAction::PaneMutation,
    ]
}

fn ordered_steps(plan: &MissionObjectivePlan) -> Vec<MissionObjectivePlanStep> {
    plan.plan_steps
        .iter()
        .chain(&plan.fallback_steps)
        .cloned()
        .collect()
}

fn objective_id(plan: &MissionObjectivePlan) -> String {
    format!("objective-{}", plan.generated_at_ms)
}

fn source_snapshot_artifacts(
    plan: &MissionObjectivePlan,
) -> Vec<MissionObjectiveSourceSnapshotArtifact> {
    let mut snapshots = plan
        .source_snapshots
        .iter()
        .map(MissionObjectiveSourceSnapshotArtifact::from_snapshot)
        .collect::<Vec<_>>();

    if snapshots.is_empty() {
        snapshots.push(MissionObjectiveSourceSnapshotArtifact::from_snapshot(
            &MissionObjectiveSourceSnapshot::new(
                "planner.generated",
                MissionObjectiveSourceKind::Manual,
            )
            .with_reason_code("source.generated_fallback"),
        ));
    }

    snapshots
}

fn target_bead_ids(steps: &[MissionObjectivePlanStep]) -> Vec<String> {
    let mut ids = Vec::new();
    for step in steps {
        if let Some(bead_id) = &step.target_bead_id {
            push_unique(&mut ids, bead_id.clone());
        }
    }
    ids
}

fn allowed_action_classes(steps: &[MissionObjectivePlanStep]) -> Vec<String> {
    let mut classes = Vec::new();
    push_unique(&mut classes, "read_status".to_string());
    for step in steps {
        push_unique(
            &mut classes,
            action_class_name(step.action_kind).to_string(),
        );
    }
    classes
}

fn forbidden_action_classes(actions: &[MissionObjectiveForbiddenAction]) -> Vec<String> {
    let mut classes = Vec::new();
    for action in actions {
        push_unique(&mut classes, forbidden_action_class(*action).to_string());
    }
    classes
}

fn forbidden_action_class(action: MissionObjectiveForbiddenAction) -> &'static str {
    match action {
        MissionObjectiveForbiddenAction::ServiceMutation => "service_mutation",
        MissionObjectiveForbiddenAction::Destructive => "destructive_filesystem",
        MissionObjectiveForbiddenAction::PaneMutation => "pane_mutation",
        MissionObjectiveForbiddenAction::LocalCargoProof => "local_cargo_proof",
        MissionObjectiveForbiddenAction::RawPaneContent => "raw_pane_content",
    }
}

fn capacity_admission_artifact(
    plan: &MissionObjectivePlan,
    steps: &[MissionObjectivePlanStep],
) -> MissionObjectiveCapacityAdmissionArtifact {
    let action = match plan.plan_status {
        MissionObjectivePlanStatus::Actionable
        | MissionObjectivePlanStatus::PlanningOnly
        | MissionObjectivePlanStatus::NoReadyWork
        | MissionObjectivePlanStatus::Degraded => "admit",
        MissionObjectivePlanStatus::WaitingOwner
        | MissionObjectivePlanStatus::WaitingExternal
        | MissionObjectivePlanStatus::DirtyOverlap
        | MissionObjectivePlanStatus::RchSubstrateBlocked => "defer",
        MissionObjectivePlanStatus::Blocked => "shed",
        MissionObjectivePlanStatus::Unavailable => "unavailable",
    };
    let proof_state = if steps
        .iter()
        .any(|step| step.proof_lane == MissionObjectiveProofLane::Blocked)
    {
        "unavailable"
    } else if steps
        .iter()
        .any(|step| step.proof_lane == MissionObjectiveProofLane::RchCargo)
    {
        "simulated"
    } else {
        "not_required"
    };

    MissionObjectiveCapacityAdmissionArtifact {
        action: action.to_string(),
        pressure_tier: "unknown".to_string(),
        proof_state: proof_state.to_string(),
        reason_codes: plan.reason_codes.clone(),
    }
}

fn proof_strictness_for_steps(
    steps: &[MissionObjectivePlanStep],
    strictness: MissionObjectiveStrictness,
) -> String {
    if steps
        .iter()
        .any(|step| step.proof_lane == MissionObjectiveProofLane::Blocked)
    {
        return "blocked_until_proof_available".to_string();
    }
    if strictness == MissionObjectiveStrictness::Strict
        || steps
            .iter()
            .any(|step| step.proof_lane == MissionObjectiveProofLane::RchCargo)
    {
        return "rch_required".to_string();
    }
    "static_only".to_string()
}

fn proof_requirements_for_steps(
    steps: &[MissionObjectivePlanStep],
) -> Vec<MissionObjectiveProofRequirementArtifact> {
    let mut requirements = Vec::new();
    let mut seen = Vec::new();

    for step in steps {
        for proof_id in proof_requirement_ids(step.proof_lane) {
            if !seen.contains(&proof_id) {
                seen.push(proof_id.clone());
                requirements.push(proof_requirement_for_lane(proof_id, step.proof_lane));
            }
        }
    }

    requirements
}

fn proof_requirement_for_lane(
    proof_id: String,
    proof_lane: MissionObjectiveProofLane,
) -> MissionObjectiveProofRequirementArtifact {
    match proof_lane {
        MissionObjectiveProofLane::RchCargo => MissionObjectiveProofRequirementArtifact {
            proof_id,
            proof_kind: "rch_cargo".to_string(),
            required: true,
            status: "pending".to_string(),
            command: "rch exec -- env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo test -p frankenterm-core --lib mission_objective_plan".to_string(),
            artifact_paths: Vec::new(),
        },
        MissionObjectiveProofLane::Blocked => MissionObjectiveProofRequirementArtifact {
            proof_id,
            proof_kind: "rch_cargo".to_string(),
            required: true,
            status: "blocked".to_string(),
            command: "rch exec -- env CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo test -p frankenterm-core --lib mission_objective_plan".to_string(),
            artifact_paths: Vec::new(),
        },
        MissionObjectiveProofLane::StaticSchema => MissionObjectiveProofRequirementArtifact {
            proof_id,
            proof_kind: "static_schema".to_string(),
            required: true,
            status: "pending".to_string(),
            command: "jq empty docs/json-schema/ft-mission-objective-plan.json".to_string(),
            artifact_paths: vec!["docs/json-schema/ft-mission-objective-plan.json".to_string()],
        },
        MissionObjectiveProofLane::None | MissionObjectiveProofLane::NotRequired => {
            MissionObjectiveProofRequirementArtifact {
                proof_id,
                proof_kind: "manual_review".to_string(),
                required: false,
                status: "not_required".to_string(),
                command: "not required".to_string(),
                artifact_paths: Vec::new(),
            }
        }
    }
}

fn proof_requirement_ids(proof_lane: MissionObjectiveProofLane) -> Vec<String> {
    match proof_lane {
        MissionObjectiveProofLane::RchCargo => vec!["proof-rch-cargo".to_string()],
        MissionObjectiveProofLane::Blocked => vec!["proof-rch-cargo".to_string()],
        MissionObjectiveProofLane::StaticSchema => vec!["proof-static-schema".to_string()],
        MissionObjectiveProofLane::None | MissionObjectiveProofLane::NotRequired => Vec::new(),
    }
}

fn step_action_kind_name(action_kind: MissionObjectiveActionKind) -> &'static str {
    match action_kind {
        MissionObjectiveActionKind::ChooseReadyBead => "choose_ready_bead",
        MissionObjectiveActionKind::WaitForOwner => "wait_for_owner",
        MissionObjectiveActionKind::StatusCheckBeforeReopen
        | MissionObjectiveActionKind::RunBvRobotTriage => "inspect_artifact",
        MissionObjectiveActionKind::AddBeadsComment => "add_beads_comment",
        MissionObjectiveActionKind::RunTestingSkill => "run_static_check",
        MissionObjectiveActionKind::PlanningOnly | MissionObjectiveActionKind::WaitExternal => {
            "no_op"
        }
        MissionObjectiveActionKind::FileFollowupBead => "create_bead",
    }
}

fn action_class_name(action_kind: MissionObjectiveActionKind) -> &'static str {
    match action_kind {
        MissionObjectiveActionKind::ChooseReadyBead => "claim_bead",
        MissionObjectiveActionKind::WaitForOwner | MissionObjectiveActionKind::WaitExternal => {
            "wait"
        }
        MissionObjectiveActionKind::StatusCheckBeforeReopen
        | MissionObjectiveActionKind::RunBvRobotTriage
        | MissionObjectiveActionKind::PlanningOnly => "read_status",
        MissionObjectiveActionKind::AddBeadsComment => "add_beads_comment",
        MissionObjectiveActionKind::RunTestingSkill => "run_static_check",
        MissionObjectiveActionKind::FileFollowupBead => "create_bead",
    }
}

fn step_side_effect_class_name(side_effect_class: MissionObjectiveSideEffectClass) -> &'static str {
    match side_effect_class {
        MissionObjectiveSideEffectClass::None => "none",
        MissionObjectiveSideEffectClass::CoordinationMutation => "beads_metadata",
        MissionObjectiveSideEffectClass::RemoteProof
        | MissionObjectiveSideEffectClass::LocalCargoProof => "proof_run",
        MissionObjectiveSideEffectClass::PaneMutation => "pane_mutation",
        MissionObjectiveSideEffectClass::ServiceMutation => "service_mutation",
        MissionObjectiveSideEffectClass::Destructive => "destructive",
    }
}

fn evidence_category_name(category: MissionObjectiveEvidenceCategory) -> &'static str {
    match category {
        MissionObjectiveEvidenceCategory::BeadsReadyQueue => "beads_ready_queue",
        MissionObjectiveEvidenceCategory::BeadsBlockedQueue => "beads_blocked_queue",
        MissionObjectiveEvidenceCategory::BeadsInProgress => "beads_in_progress",
        MissionObjectiveEvidenceCategory::ActiveAssigneeOverlap => "active_assignee_overlap",
        MissionObjectiveEvidenceCategory::AgentMailAvailability => "agent_mail_availability",
        MissionObjectiveEvidenceCategory::RchWorkerSelection => "rch_worker_selection",
        MissionObjectiveEvidenceCategory::RchActiveProjectExclusion => {
            "rch_active_project_exclusion"
        }
        MissionObjectiveEvidenceCategory::RchTopologyPreflight => "rch_topology_preflight",
        MissionObjectiveEvidenceCategory::RchCargoVerdict => "rch_cargo_verdict",
        MissionObjectiveEvidenceCategory::GitDirtyTree => "git_dirty_tree",
        MissionObjectiveEvidenceCategory::DirtyPathOverlap => "dirty_path_overlap",
        MissionObjectiveEvidenceCategory::CapacityPressure => "capacity_pressure",
        MissionObjectiveEvidenceCategory::RobotInventory => "robot_inventory",
        MissionObjectiveEvidenceCategory::RedactionPosture => "redaction_posture",
        MissionObjectiveEvidenceCategory::Manual => "manual_note",
    }
}

fn source_evidence_state(snapshot: &MissionObjectiveSourceSnapshot) -> String {
    match snapshot.state {
        MissionObjectiveSourceState::Available => "pass",
        MissionObjectiveSourceState::Degraded => "warn",
        MissionObjectiveSourceState::Unavailable => "unavailable",
        MissionObjectiveSourceState::Stale => "stale",
        MissionObjectiveSourceState::Blocked => "blocked",
    }
    .to_string()
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

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const NOW_MS: u64 = 1_770_000_001_000;

    fn green_sources() -> Vec<MissionObjectiveSourceSnapshot> {
        vec![
            MissionObjectiveSourceSnapshot::new("beads.ready", MissionObjectiveSourceKind::Beads)
                .with_evidence(
                    MissionObjectiveEvidenceItem::new(
                        MissionObjectiveEvidenceCategory::BeadsReadyQueue,
                        "one ready bead",
                    )
                    .with_reason_code("beads.ready_available"),
                ),
            MissionObjectiveSourceSnapshot::new("rch.status", MissionObjectiveSourceKind::Rch)
                .with_evidence(MissionObjectiveEvidenceItem::new(
                    MissionObjectiveEvidenceCategory::RchWorkerSelection,
                    "worker selected",
                )),
            MissionObjectiveSourceSnapshot::new("git.status", MissionObjectiveSourceKind::Git)
                .with_evidence(MissionObjectiveEvidenceItem::new(
                    MissionObjectiveEvidenceCategory::GitDirtyTree,
                    "clean for scope",
                )),
        ]
    }

    fn input_with_sources() -> MissionObjectivePlannerInput {
        let mut input =
            MissionObjectivePlannerInput::new(NOW_MS, "test", "ship the next safe slice");
        for source in green_sources() {
            input = input.with_source_snapshot(source);
        }
        input
    }

    #[test]
    fn ready_bead_ranks_first_and_is_actionable() {
        let plan = plan_mission_objective(
            &input_with_sources()
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "ft-ready",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .title("ready contract slice")
                    .priority(1)
                    .target_bead_id("ft-ready")
                    .with_owned_path("docs/robot-contracts/mission-objective-plan.md")
                    .proof_availability(MissionObjectiveProofAvailability::Available),
                )
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "fallback",
                        MissionObjectiveCandidateReadiness::PlanningOnly,
                    )
                    .priority(3),
                ),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::Actionable);
        assert_eq!(plan.plan_steps[0].candidate_id, "ft-ready");
        assert_eq!(
            plan.plan_steps[0].action_kind,
            MissionObjectiveActionKind::ChooseReadyBead
        );
        assert_eq!(
            plan.plan_steps[0].proof_lane,
            MissionObjectiveProofLane::RchCargo
        );
        assert!(plan.dry_run);
        assert!(!plan.side_effects_executed);
        assert!(!plan.raw_pane_content_stored);
    }

    #[test]
    fn no_ready_work_emits_triage_testing_and_planning_fallbacks() {
        let plan = plan_mission_objective(&input_with_sources());

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::NoReadyWork);
        assert!(plan.plan_steps.is_empty());
        assert_eq!(plan.fallback_steps.len(), 3);
        assert_eq!(
            plan.fallback_steps[0].action_kind,
            MissionObjectiveActionKind::RunBvRobotTriage
        );
        assert_eq!(
            plan.fallback_steps[1].action_kind,
            MissionObjectiveActionKind::RunTestingSkill
        );
        assert!(
            plan.reason_codes
                .iter()
                .any(|code| code == "planner.testing_skill_fallback")
        );
    }

    #[test]
    fn active_same_domain_waits_for_owner() {
        let plan = plan_mission_objective(
            &input_with_sources().with_candidate(
                MissionObjectiveCandidateWork::new(
                    "renderer-active",
                    MissionObjectiveCandidateReadiness::ActiveSameDomain,
                )
                .active_owner("Codex", 120)
                .stale_after_seconds(7_200)
                .with_owned_path("crates/frankenterm-gui/src/frontend.rs"),
            ),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::WaitingOwner);
        let step = &plan.plan_steps[0];
        assert_eq!(step.action_kind, MissionObjectiveActionKind::WaitForOwner);
        assert!(
            step.required_approvals
                .contains(&MissionObjectiveApprovalRequirement::OwnerHandoff)
        );
    }

    #[test]
    fn stale_reopen_candidate_requires_status_check_not_force_claim() {
        let plan = plan_mission_objective(
            &input_with_sources().with_candidate(
                MissionObjectiveCandidateWork::new(
                    "stale-bead",
                    MissionObjectiveCandidateReadiness::StaleReopenCandidate,
                )
                .target_bead_id("ft-stale")
                .active_owner("GreyRiver", 7_500)
                .stale_after_seconds(7_200),
            ),
        );

        let step = &plan.plan_steps[0];
        assert_eq!(
            step.action_kind,
            MissionObjectiveActionKind::StatusCheckBeforeReopen
        );
        assert_eq!(
            step.side_effect_class,
            MissionObjectiveSideEffectClass::CoordinationMutation
        );
        assert!(!step.forbidden);
        assert!(!plan.side_effects_executed);
        assert!(
            step.reason_codes
                .iter()
                .any(|code| code == "beads.status_check_before_reopen")
        );
    }

    #[test]
    fn rch_blocked_proof_blocks_local_cargo_fallback() {
        let plan = plan_mission_objective(
            &MissionObjectivePlannerInput::new(NOW_MS, "test", "prove Rust change")
                .with_source_snapshot(
                    MissionObjectiveSourceSnapshot::new(
                        "rch.status",
                        MissionObjectiveSourceKind::Rch,
                    )
                    .degraded("rch.no_workers_passed_health"),
                )
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "rust-change",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .target_bead_id("ft-rust")
                    .proof_availability(MissionObjectiveProofAvailability::Available),
                ),
        );

        assert_eq!(
            plan.plan_status,
            MissionObjectivePlanStatus::RchSubstrateBlocked
        );
        let step = &plan.plan_steps[0];
        assert_eq!(step.proof_lane, MissionObjectiveProofLane::Blocked);
        assert!(
            step.required_approvals
                .contains(&MissionObjectiveApprovalRequirement::RchRecovered)
        );
        assert!(
            plan.forbidden_actions
                .contains(&MissionObjectiveForbiddenAction::LocalCargoProof)
        );
    }

    #[test]
    fn dirty_overlap_waits_and_lists_paths() {
        let plan = plan_mission_objective(
            &input_with_sources()
                .with_dirty_path(
                    MissionObjectiveDirtyPath::new(
                        "fixtures/mission-planner/objective-plan/ready-bead.json",
                        "M",
                    )
                    .category("tracked_overlap_risk"),
                )
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "fixture-change",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .with_owned_path("fixtures/mission-planner/objective-plan/"),
                ),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::DirtyOverlap);
        let step = &plan.plan_steps[0];
        assert_eq!(step.action_kind, MissionObjectiveActionKind::WaitForOwner);
        assert_eq!(
            step.affected_paths,
            vec!["fixtures/mission-planner/objective-plan/ready-bead.json"]
        );
        assert!(
            step.required_approvals
                .contains(&MissionObjectiveApprovalRequirement::CleanWorktree)
        );
    }

    #[test]
    fn dirty_overlap_handles_directory_paths_without_trailing_slash() {
        let plan = plan_mission_objective(
            &input_with_sources()
                .with_dirty_path(MissionObjectiveDirtyPath::new(
                    "crates/frankenterm-core/src/mission_objective_plan.rs",
                    "M",
                ))
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "core-module-change",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .with_owned_path("crates/frankenterm-core"),
                ),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::DirtyOverlap);
        assert_eq!(
            plan.plan_steps[0].affected_paths,
            vec!["crates/frankenterm-core/src/mission_objective_plan.rs"]
        );
    }

    #[test]
    fn capacity_pause_waits_external_instead_of_claiming() {
        let plan = plan_mission_objective(
            &input_with_sources().with_candidate(
                MissionObjectiveCandidateWork::new(
                    "capacity-paused",
                    MissionObjectiveCandidateReadiness::ReadyBead,
                )
                .target_bead_id("ft-capacity")
                .capacity_posture(MissionObjectiveCapacityPosture::Pause),
            ),
        );

        assert_eq!(
            plan.plan_status,
            MissionObjectivePlanStatus::WaitingExternal
        );
        let step = &plan.plan_steps[0];
        assert_eq!(step.action_kind, MissionObjectiveActionKind::WaitExternal);
        assert_eq!(step.risk_level, MissionObjectiveRiskLevel::High);
    }

    #[test]
    fn agent_mail_unavailable_degrades_but_keeps_beads_fallback_actionable() {
        let plan = plan_mission_objective(
            &MissionObjectivePlannerInput::new(NOW_MS, "test", "continue with Beads")
                .with_source_snapshot(
                    MissionObjectiveSourceSnapshot::new(
                        "agent_mail.fallback",
                        MissionObjectiveSourceKind::AgentMail,
                    )
                    .unavailable("agent_mail.database_error"),
                )
                .with_source_snapshot(
                    MissionObjectiveSourceSnapshot::new(
                        "beads.ready",
                        MissionObjectiveSourceKind::Beads,
                    )
                    .with_reason_code("beads.ready_available"),
                )
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "ready-under-fallback",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .target_bead_id("ft-ready"),
                ),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::Degraded);
        let step = &plan.plan_steps[0];
        assert_eq!(
            step.action_kind,
            MissionObjectiveActionKind::ChooseReadyBead
        );
        assert!(
            step.required_approvals
                .contains(&MissionObjectiveApprovalRequirement::AgentMailRecovered)
        );
        assert!(
            step.reason_codes
                .iter()
                .any(|code| code == "agent_mail.beads_fallback_available")
        );
    }

    #[test]
    fn unredacted_source_fails_closed() {
        let plan = plan_mission_objective(
            &MissionObjectivePlannerInput::new(NOW_MS, "test", "unsafe objective")
                .with_source_snapshot(
                    MissionObjectiveSourceSnapshot::new(
                        "robot.text",
                        MissionObjectiveSourceKind::Robot,
                    )
                    .redaction_posture(MissionObjectiveRedactionPosture::RawPaneContent),
                )
                .with_candidate(MissionObjectiveCandidateWork::new(
                    "ready",
                    MissionObjectiveCandidateReadiness::ReadyBead,
                )),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::Unavailable);
        assert_eq!(plan.risk_level, MissionObjectiveRiskLevel::Blocked);
        assert_eq!(plan.plan_steps[0].candidate_id, "redaction.fail_closed");
        assert!(!plan.raw_pane_content_stored);
    }

    #[test]
    fn ranking_is_stable_for_equal_scores() {
        let plan = plan_mission_objective(
            &input_with_sources()
                .with_candidate(MissionObjectiveCandidateWork::new(
                    "candidate-b",
                    MissionObjectiveCandidateReadiness::PlanningOnly,
                ))
                .with_candidate(MissionObjectiveCandidateWork::new(
                    "candidate-a",
                    MissionObjectiveCandidateReadiness::PlanningOnly,
                )),
        );

        assert_eq!(plan.plan_steps[0].candidate_id, "candidate-a");
        assert_eq!(plan.plan_steps[1].candidate_id, "candidate-b");
        assert_eq!(plan.plan_steps[0].rank, 1);
        assert_eq!(plan.plan_steps[1].rank, 2);
    }

    #[test]
    fn plan_reason_codes_use_contract_snake_case_names() {
        let plan = plan_mission_objective(
            &MissionObjectivePlannerInput::new(NOW_MS, "test", "prove Rust change")
                .with_source_snapshot(
                    MissionObjectiveSourceSnapshot::new(
                        "rch.status",
                        MissionObjectiveSourceKind::Rch,
                    )
                    .degraded("rch.no_workers_passed_health"),
                )
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "rust-change",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .proof_availability(MissionObjectiveProofAvailability::Available),
                ),
        );

        assert!(
            plan.reason_codes
                .iter()
                .any(|code| code == "planner.status.rch_substrate_blocked")
        );
        assert!(
            !plan
                .reason_codes
                .iter()
                .any(|code| code == "planner.status.rchsubstrateblocked")
        );
        assert!(
            plan.reason_codes
                .iter()
                .any(|code| code == "planner.risk.blocked")
        );
    }

    #[test]
    fn source_kind_state_and_freshness_match_contract_strings() {
        assert_eq!(
            serde_json::to_value(MissionObjectiveSourceKind::ResourceCockpit).unwrap(),
            serde_json::json!("resource_cockpit")
        );
        assert_eq!(
            serde_json::to_value(MissionObjectiveSourceState::Blocked).unwrap(),
            serde_json::json!("blocked")
        );
        assert_eq!(
            serde_json::to_value(MissionObjectiveSourceState::Stale).unwrap(),
            serde_json::json!("stale")
        );
        assert_eq!(
            serde_json::to_value(MissionObjectiveFreshnessState::WithinBudget).unwrap(),
            serde_json::json!("within_budget")
        );
        assert_eq!(
            serde_json::to_value(MissionObjectiveFreshnessState::NotCollected).unwrap(),
            serde_json::json!("not_collected")
        );
    }

    #[test]
    fn within_budget_freshness_does_not_degrade_plan() {
        let plan = plan_mission_objective(
            &MissionObjectivePlannerInput::new(NOW_MS, "test", "ship fresh enough work")
                .with_source_snapshot(MissionObjectiveSourceSnapshot {
                    source_id: "beads.ready".to_string(),
                    kind: MissionObjectiveSourceKind::Beads,
                    state: MissionObjectiveSourceState::Available,
                    freshness_state: MissionObjectiveFreshnessState::WithinBudget,
                    redaction_posture: MissionObjectiveRedactionPosture::Redacted,
                    reason_codes: Vec::new(),
                    evidence: Vec::new(),
                })
                .with_candidate(
                    MissionObjectiveCandidateWork::new(
                        "fresh-enough",
                        MissionObjectiveCandidateReadiness::ReadyBead,
                    )
                    .target_bead_id("ft-fresh-enough"),
                ),
        );

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::Actionable);
        assert!(
            !plan
                .reason_codes
                .iter()
                .any(|code| code == "source.beads.ready.within_budget")
        );
    }

    #[test]
    fn serialized_plan_uses_objective_plan_contract_shape() {
        let plan = plan_mission_objective(
            &input_with_sources().with_candidate(
                MissionObjectiveCandidateWork::new(
                    "ft-ready",
                    MissionObjectiveCandidateReadiness::ReadyBead,
                )
                .target_bead_id("ft-ready")
                .proof_availability(MissionObjectiveProofAvailability::Available),
            ),
        );
        let value = serde_json::to_value(&plan).unwrap();

        assert_eq!(
            value["contract_id"],
            serde_json::json!(MISSION_OBJECTIVE_PLAN_CONTRACT_ID)
        );
        assert!(value.get("steps").is_some());
        assert!(value.get("proof_requirements").is_some());
        assert!(value.get("capacity_admission").is_some());
        assert!(value.get("redaction_policy").is_some());
        assert!(value.get("plan_steps").is_none());
        assert!(value.get("fallback_steps").is_none());
        assert!(value.get("source").is_none());
        assert_eq!(value["raw_pane_content_stored"], serde_json::json!(false));
        assert_eq!(
            value["source_snapshots"][0]["source_kind"],
            serde_json::json!("beads")
        );
        assert!(value["source_snapshots"][0].get("kind").is_none());
        assert!(value["source_snapshots"][0]["redaction_posture"].is_object());
        assert_eq!(
            value["steps"][0]["action_kind"],
            serde_json::json!("choose_ready_bead")
        );
        assert_eq!(
            value["steps"][0]["side_effect_class"],
            serde_json::json!("proof_run")
        );
        assert_eq!(
            value["proof_requirements"][0]["proof_id"],
            serde_json::json!("proof-rch-cargo")
        );
        assert!(value["forbidden_actions"][0].is_object());
        let objective_forbidden_classes = value["objective"]["forbidden_action_classes"]
            .as_array()
            .unwrap();
        assert!(objective_forbidden_classes.iter().any(|class| matches!(
            class.as_str(),
            Some("local_cargo_proof" | "raw_pane_content")
        )));
        assert!(
            value["forbidden_actions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|action| matches!(
                    action["action_class"].as_str(),
                    Some(
                        "pane_mutation"
                            | "service_mutation"
                            | "destructive_filesystem"
                            | "local_cargo_proof"
                            | "raw_pane_content"
                    )
                ))
        );
    }

    #[test]
    fn surface_data_preserves_contract_and_step_explain() {
        let plan = plan_mission_objective(
            &input_with_sources().with_candidate(
                MissionObjectiveCandidateWork::new(
                    "ft-ready",
                    MissionObjectiveCandidateReadiness::ReadyBead,
                )
                .target_bead_id("ft-ready"),
            ),
        );

        let surface = build_mission_objective_plan_surface_data(plan, Some("ft-ready"), None);

        assert_eq!(surface.contract_id, MISSION_OBJECTIVE_PLAN_CONTRACT_ID);
        assert!(surface.dry_run);
        assert!(!surface.side_effects_executed);
        assert!(!surface.raw_pane_content_stored);
        let explain = surface.explain.expect("step explanation");
        assert_eq!(explain.mode, MissionObjectivePlanExplainMode::Step);
        assert!(explain.matched);
        assert_eq!(
            explain
                .step
                .as_ref()
                .and_then(|step| step.target_bead_id.as_deref()),
            Some("ft-ready")
        );
    }

    #[test]
    fn surface_data_explains_reason_locations() {
        let plan = plan_mission_objective(
            &input_with_sources().with_candidate(
                MissionObjectiveCandidateWork::new(
                    "ft-ready",
                    MissionObjectiveCandidateReadiness::ReadyBead,
                )
                .target_bead_id("ft-ready"),
            ),
        );

        let surface =
            build_mission_objective_plan_surface_data(plan, None, Some("beads.ready_available"));

        let explain = surface.explain.expect("reason explanation");
        assert_eq!(explain.mode, MissionObjectivePlanExplainMode::Reason);
        assert!(explain.matched);
        assert!(
            explain
                .matches
                .iter()
                .any(|location| location == "plan.reason_codes")
        );
        assert!(
            explain
                .matches
                .iter()
                .any(|location| location == "steps.ft-ready.reason_codes")
        );
    }

    #[test]
    fn forbidden_actions_are_sorted_and_unique_contract_guard() {
        let plan = plan_mission_objective(&input_with_sources());
        let unique = plan
            .forbidden_actions
            .iter()
            .collect::<BTreeSet<&MissionObjectiveForbiddenAction>>();

        assert_eq!(unique.len(), plan.forbidden_actions.len());
        assert!(
            plan.forbidden_actions
                .contains(&MissionObjectiveForbiddenAction::ServiceMutation)
        );
        assert!(
            plan.forbidden_actions
                .contains(&MissionObjectiveForbiddenAction::RawPaneContent)
        );
    }
}
