//! Deterministic replay adapter for redacted mission-twin snapshots.
//!
//! The replay core is deliberately narrow: it validates retained
//! `MissionTwinSnapshotEnvelope` inputs, translates their redacted source facts
//! into `MissionObjectivePlannerInput`, and delegates ranking to the existing
//! side-effect-free mission objective planner.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::mission_objective_plan::{
    DEFAULT_STALE_AFTER_SECONDS, MissionObjectiveActionKind, MissionObjectiveApprovalRequirement,
    MissionObjectiveCandidateReadiness, MissionObjectiveCandidateWork,
    MissionObjectiveCapacityPosture, MissionObjectiveDirtyPath, MissionObjectiveEvidenceCategory,
    MissionObjectiveEvidenceItem, MissionObjectiveFreshnessState, MissionObjectivePlan,
    MissionObjectivePlanStatus, MissionObjectivePlanStep, MissionObjectivePlanSurfaceData,
    MissionObjectivePlannerInput, MissionObjectiveProofAvailability, MissionObjectiveProofLane,
    MissionObjectiveRedactionPosture, MissionObjectiveSideEffectClass, MissionObjectiveSourceKind,
    MissionObjectiveSourceSnapshot, MissionObjectiveSourceState,
    build_mission_objective_plan_surface_data, plan_mission_objective,
};
use crate::mission_twin_snapshot::{
    AgentMailAvailabilityState, AgentMailMissionTwinSnapshot, BeadsMissionTwinSnapshot,
    DirtyPathSummary, EvidenceLevel, FreshnessState, GitMissionTwinSnapshot,
    MissionTwinSnapshotEnvelope, MissionTwinSnapshotError, OperatingEnvelopeMissionTwinSnapshot,
    OperatingEnvelopeVerdict, OwnerState, RchAdmissionState, RchMissionTwinSnapshot,
    ReservationsMissionTwinSnapshot, SourceEvidence, SourceStatus, StaleOwnerCandidate,
    is_safe_repo_relative_path,
};

pub const MISSION_TWIN_REPLAY_SOURCE_BEAD: &str = "ft-u7r37.2";
pub const MISSION_TWIN_REPLAY_SOURCE: &str = "mission_twin.replay.ft-u7r37.2";
pub const MISSION_TWIN_REPLAY_OBJECTIVE: &str =
    "Replay redacted mission-twin snapshots into a side-effect-free current-state plan.";
pub const MISSION_TWIN_COUNTERFACTUAL_CONTRACT_ID: &str =
    "ft.mission_twin_counterfactual_replay.v1";
pub const MISSION_TWIN_COUNTERFACTUAL_SCHEMA_VERSION: u16 = 1;
pub const MISSION_TWIN_COUNTERFACTUAL_SOURCE_BEAD: &str = "ft-u7r37.3";
pub const MAX_COUNTERFACTUAL_PROOF_LANES: u8 = 64;
pub const MISSION_TWIN_OWNERSHIP_SIMULATOR_CONTRACT_ID: &str =
    "ft.mission_twin_ownership_handoff.v1";
pub const MISSION_TWIN_OWNERSHIP_SIMULATOR_SCHEMA_VERSION: u16 = 1;
pub const MISSION_TWIN_OWNERSHIP_SIMULATOR_SOURCE_BEAD: &str = "ft-u7r37.4";
pub const MISSION_TWIN_SURFACE_CONTRACT_ID: &str = "ft.mission_twin.robot_mcp_cli_surface.v1";
pub const MISSION_TWIN_SURFACE_SCHEMA_VERSION: u16 = 1;
pub const MISSION_TWIN_SURFACE_SOURCE_BEAD: &str = "ft-u7r37.5";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionTwinReplayError {
    EmptySnapshotSet,
    InvalidSnapshot {
        snapshot_id: String,
        error: MissionTwinSnapshotError,
    },
    InvalidCounterfactual {
        scenario_id: String,
        reason: String,
    },
    InvalidSurfaceRequest {
        action: String,
        reason: String,
    },
}

impl std::fmt::Display for MissionTwinReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySnapshotSet => {
                write!(f, "mission twin replay requires at least one snapshot")
            }
            Self::InvalidSnapshot { snapshot_id, error } => {
                write!(f, "invalid mission twin snapshot {snapshot_id}: {error}")
            }
            Self::InvalidCounterfactual {
                scenario_id,
                reason,
            } => {
                write!(
                    f,
                    "invalid mission twin counterfactual {scenario_id}: {reason}"
                )
            }
            Self::InvalidSurfaceRequest { action, reason } => {
                write!(f, "invalid mission twin surface request {action}: {reason}")
            }
        }
    }
}

impl std::error::Error for MissionTwinReplayError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EmptySnapshotSet => None,
            Self::InvalidSnapshot { error, .. } => Some(error),
            Self::InvalidCounterfactual { .. } => None,
            Self::InvalidSurfaceRequest { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinCounterfactualToggle {
    RchRecovered,
    AgentMailRecovered,
    DirtyOverlapCleared,
    OwnerHandoffAccepted,
    TargetClassProofAvailable,
    ProofLanesBudgeted,
}

impl MissionTwinCounterfactualToggle {
    fn reason_code(self) -> &'static str {
        match self {
            Self::RchRecovered => "mission_twin.counterfactual.rch_recovered",
            Self::AgentMailRecovered => "mission_twin.counterfactual.agent_mail_recovered",
            Self::DirtyOverlapCleared => "mission_twin.counterfactual.dirty_overlap_cleared",
            Self::OwnerHandoffAccepted => "mission_twin.counterfactual.owner_handoff_accepted",
            Self::TargetClassProofAvailable => {
                "mission_twin.counterfactual.target_class_proof_available"
            }
            Self::ProofLanesBudgeted => "mission_twin.counterfactual.proof_lanes_budgeted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinProofLaneBudget {
    pub remote_cargo_lanes: u8,
    pub static_verifier_lanes: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinCounterfactualRequest {
    pub scenario_id: String,
    pub toggles: Vec<MissionTwinCounterfactualToggle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_lane_budget: Option<MissionTwinProofLaneBudget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinProofLaneClass {
    RemoteCargo,
    StaticVerifier,
    CoordinationOnly,
    WaitingOwner,
    WaitingRch,
    NotRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionTwinProofLaneDecision {
    pub candidate_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_bead_id: Option<String>,
    pub lane_class: MissionTwinProofLaneClass,
    pub proof_lane: MissionObjectiveProofLane,
    pub status: MissionObjectivePlanStatus,
    pub required_approvals: Vec<MissionObjectiveApprovalRequirement>,
    pub reason_codes: Vec<String>,
    pub live_execution_blocked_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionTwinProofLaneBrokerReport {
    pub source_bead: String,
    pub simulated: bool,
    pub decisions: Vec<MissionTwinProofLaneDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionTwinCounterfactualPlan {
    pub scenario_id: String,
    pub simulated: bool,
    pub toggles: Vec<MissionTwinCounterfactualToggle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_lane_budget: Option<MissionTwinProofLaneBudget>,
    pub plan_status: MissionObjectivePlanStatus,
    pub risk_level: crate::mission_objective_plan::MissionObjectiveRiskLevel,
    pub live_execution_blocked_by: Vec<String>,
    pub remaining_blockers: Vec<String>,
    pub unblocked_reason_codes: Vec<String>,
    pub proof_lane_broker: MissionTwinProofLaneBrokerReport,
    pub surface: MissionObjectivePlanSurfaceData,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionTwinCounterfactualReplayReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub source_bead: String,
    pub simulated: bool,
    pub side_effects_executed: bool,
    pub raw_pane_content_stored: bool,
    pub forbidden_actions: Vec<String>,
    pub reason_codes: Vec<String>,
    pub live_plan: MissionTwinCounterfactualPlan,
    pub counterfactual_plans: Vec<MissionTwinCounterfactualPlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinOwnershipSimulationRequest {
    pub candidate_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_bead_id: Option<String>,
    #[serde(default)]
    pub owned_paths: Vec<String>,
    #[serde(default = "default_mission_twin_ownership_stale_after_seconds")]
    pub stale_after_seconds: u64,
    #[serde(default)]
    pub fallback_only_coordination: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinOwnershipOverlapSource {
    DirtyPath,
    Reservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinOwnershipOverlapKind {
    Exact,
    ParentChild,
    GlobLike,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinOwnershipPathOverlap {
    pub source: MissionTwinOwnershipOverlapSource,
    pub overlap_kind: MissionTwinOwnershipOverlapKind,
    pub owned_path: String,
    pub matched_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub exclusive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinOwnershipOwnerSummary {
    pub bead_id: String,
    pub assignee: String,
    pub owner_state: OwnerState,
    pub age_seconds: u64,
    pub stale_after_seconds: u64,
    pub stale: bool,
    pub last_activity_source: String,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinOwnershipHandoffState {
    Active,
    StaleCheckNeeded,
    HandoffRequired,
    SafeToOpen,
    UnsafeOverlap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinOwnershipNextAction {
    Wait,
    Comment,
    AskOwner,
    ChoosePlanningOnlyWork,
    RunStaticOnlyVerifier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionTwinOwnershipSimulationReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub source_bead: String,
    pub simulated: bool,
    pub side_effects_executed: bool,
    pub raw_pane_content_stored: bool,
    pub forbidden_actions: Vec<String>,
    pub candidate_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_bead_id: Option<String>,
    pub owned_paths: Vec<String>,
    pub stale_after_seconds: u64,
    pub fallback_only_coordination: bool,
    pub active_agents: Vec<String>,
    pub owner_summaries: Vec<MissionTwinOwnershipOwnerSummary>,
    pub dirty_overlaps: Vec<MissionTwinOwnershipPathOverlap>,
    pub reservation_overlaps: Vec<MissionTwinOwnershipPathOverlap>,
    pub handoff_state: MissionTwinOwnershipHandoffState,
    pub next_actions: Vec<MissionTwinOwnershipNextAction>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionTwinSurfaceAction {
    CurrentPlan,
    Simulate,
    ExplainStep,
    ExplainReason,
}

impl MissionTwinSurfaceAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentPlan => "current_plan",
            Self::Simulate => "simulate",
            Self::ExplainStep => "explain_step",
            Self::ExplainReason => "explain_reason",
        }
    }

    const fn robot_command(self) -> &'static str {
        match self {
            Self::CurrentPlan => "robot mission-twin current-plan",
            Self::Simulate => "robot mission-twin simulate",
            Self::ExplainStep => "robot mission-twin explain-step",
            Self::ExplainReason => "robot mission-twin explain-reason",
        }
    }

    const fn cli_command(self) -> &'static str {
        match self {
            Self::CurrentPlan => "mission-twin current-plan",
            Self::Simulate => "mission-twin simulate",
            Self::ExplainStep => "mission-twin explain-step",
            Self::ExplainReason => "mission-twin explain-reason",
        }
    }

    const fn mcp_tool_name(self) -> &'static str {
        match self {
            Self::CurrentPlan => "ft.mission_twin.current_plan",
            Self::Simulate => "ft.mission_twin.simulate",
            Self::ExplainStep => "ft.mission_twin.explain_step",
            Self::ExplainReason => "ft.mission_twin.explain_reason",
        }
    }

    const fn mcp_resource_uri(self) -> &'static str {
        match self {
            Self::CurrentPlan => "ft://mission-twin/current-plan",
            Self::Simulate => "ft://mission-twin/simulate",
            Self::ExplainStep => "ft://mission-twin/explain-step",
            Self::ExplainReason => "ft://mission-twin/explain-reason",
        }
    }

    const fn description(self) -> &'static str {
        match self {
            Self::CurrentPlan => "Return the current retained mission-twin plan.",
            Self::Simulate => "Return safe counterfactual and ownership simulations.",
            Self::ExplainStep => "Explain one retained mission-twin plan step.",
            Self::ExplainReason => "Explain where one mission-twin reason code appears.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinSurfaceActionContract {
    pub action: MissionTwinSurfaceAction,
    pub robot_command: String,
    pub cli_command: String,
    pub mcp_tool_name: String,
    pub mcp_resource_uri: String,
    pub response_payload: String,
    pub read_only: bool,
    pub idempotent: bool,
    pub description: String,
}

impl MissionTwinSurfaceActionContract {
    #[must_use]
    pub fn for_action(action: MissionTwinSurfaceAction) -> Self {
        Self {
            action,
            robot_command: action.robot_command().to_string(),
            cli_command: action.cli_command().to_string(),
            mcp_tool_name: action.mcp_tool_name().to_string(),
            mcp_resource_uri: action.mcp_resource_uri().to_string(),
            response_payload: "MissionTwinSurfaceReport".to_string(),
            read_only: true,
            idempotent: true,
            description: action.description().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionTwinSurfaceRequest {
    pub action: MissionTwinSurfaceAction,
    #[serde(default)]
    pub snapshot_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain_step: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explain_reason: Option<String>,
    #[serde(default)]
    pub counterfactual_requests: Vec<MissionTwinCounterfactualRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership_request: Option<MissionTwinOwnershipSimulationRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissionTwinSurfaceReport {
    pub schema_version: u16,
    pub contract_id: String,
    pub source_bead: String,
    pub action: MissionTwinSurfaceAction,
    pub action_contract: MissionTwinSurfaceActionContract,
    pub simulated: bool,
    pub side_effects_executed: bool,
    pub raw_pane_content_stored: bool,
    pub forbidden_actions: Vec<String>,
    pub snapshot_paths: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub reason_codes: Vec<String>,
    pub plan_surface: MissionObjectivePlanSurfaceData,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counterfactual_report: Option<MissionTwinCounterfactualReplayReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ownership_report: Option<MissionTwinOwnershipSimulationReport>,
}

#[must_use]
pub fn mission_twin_replay_source_id(snapshot_id: &str, source_name: &str) -> String {
    format!("mission_twin.{snapshot_id}.{source_name}")
}

pub fn build_mission_twin_replay_planner_input(
    snapshots: &[MissionTwinSnapshotEnvelope],
) -> Result<MissionObjectivePlannerInput, MissionTwinReplayError> {
    let ordered = ordered_validated_snapshots(snapshots)?;
    let generated_at_ms = ordered
        .iter()
        .map(|snapshot| snapshot.generated_at_ms)
        .max()
        .ok_or(MissionTwinReplayError::EmptySnapshotSet)?;

    let mut input = MissionObjectivePlannerInput::new(
        generated_at_ms,
        MISSION_TWIN_REPLAY_SOURCE,
        MISSION_TWIN_REPLAY_OBJECTIVE,
    );

    for snapshot in ordered {
        append_source_snapshots(&mut input, snapshot);
        append_dirty_paths(&mut input, snapshot);
        append_candidates(&mut input, snapshot);
    }

    input
        .source_snapshots
        .sort_by(|left, right| left.source_id.cmp(&right.source_id));
    input.dirty_paths.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then(left.status.cmp(&right.status))
    });
    input
        .candidates
        .sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));

    Ok(input)
}

pub fn replay_mission_twin_snapshots(
    snapshots: &[MissionTwinSnapshotEnvelope],
) -> Result<MissionObjectivePlan, MissionTwinReplayError> {
    let input = build_mission_twin_replay_planner_input(snapshots)?;
    Ok(plan_mission_objective(&input))
}

pub fn build_mission_twin_replay_surface_data(
    snapshots: &[MissionTwinSnapshotEnvelope],
    explain_step: Option<&str>,
    explain_reason: Option<&str>,
) -> Result<MissionObjectivePlanSurfaceData, MissionTwinReplayError> {
    let plan = replay_mission_twin_snapshots(snapshots)?;
    Ok(build_mission_objective_plan_surface_data(
        plan,
        explain_step,
        explain_reason,
    ))
}

#[must_use]
pub fn mission_twin_surface_action_contracts() -> Vec<MissionTwinSurfaceActionContract> {
    [
        MissionTwinSurfaceAction::CurrentPlan,
        MissionTwinSurfaceAction::Simulate,
        MissionTwinSurfaceAction::ExplainStep,
        MissionTwinSurfaceAction::ExplainReason,
    ]
    .into_iter()
    .map(MissionTwinSurfaceActionContract::for_action)
    .collect()
}

pub fn build_mission_twin_surface_report(
    snapshots: &[MissionTwinSnapshotEnvelope],
    request: &MissionTwinSurfaceRequest,
) -> Result<MissionTwinSurfaceReport, MissionTwinReplayError> {
    validate_surface_request(request)?;
    let explain_step = match request.action {
        MissionTwinSurfaceAction::ExplainStep => request.explain_step.as_deref(),
        MissionTwinSurfaceAction::CurrentPlan
        | MissionTwinSurfaceAction::Simulate
        | MissionTwinSurfaceAction::ExplainReason => None,
    };
    let explain_reason = match request.action {
        MissionTwinSurfaceAction::ExplainReason => request.explain_reason.as_deref(),
        MissionTwinSurfaceAction::CurrentPlan
        | MissionTwinSurfaceAction::Simulate
        | MissionTwinSurfaceAction::ExplainStep => None,
    };
    let plan_surface =
        build_mission_twin_replay_surface_data(snapshots, explain_step, explain_reason)?;
    let counterfactual_report = if request.action == MissionTwinSurfaceAction::Simulate
        && !request.counterfactual_requests.is_empty()
    {
        Some(simulate_mission_twin_counterfactuals(
            snapshots,
            &request.counterfactual_requests,
        )?)
    } else {
        None
    };
    let ownership_report = if request.action == MissionTwinSurfaceAction::Simulate {
        request
            .ownership_request
            .as_ref()
            .map(|ownership_request| {
                simulate_mission_twin_ownership_handoff(snapshots, ownership_request)
            })
            .transpose()?
    } else {
        None
    };
    let reason_codes = mission_twin_surface_reason_codes(
        request.action,
        &plan_surface,
        counterfactual_report.as_ref(),
        ownership_report.as_ref(),
    );

    Ok(MissionTwinSurfaceReport {
        schema_version: MISSION_TWIN_SURFACE_SCHEMA_VERSION,
        contract_id: MISSION_TWIN_SURFACE_CONTRACT_ID.to_string(),
        source_bead: MISSION_TWIN_SURFACE_SOURCE_BEAD.to_string(),
        action: request.action,
        action_contract: MissionTwinSurfaceActionContract::for_action(request.action),
        simulated: request.action == MissionTwinSurfaceAction::Simulate,
        side_effects_executed: false,
        raw_pane_content_stored: false,
        forbidden_actions: mission_twin_forbidden_action_names(),
        snapshot_paths: safe_unique_repo_paths(&request.snapshot_paths),
        artifact_paths: mission_twin_surface_artifact_paths(snapshots, &request.snapshot_paths),
        reason_codes,
        plan_surface,
        counterfactual_report,
        ownership_report,
    })
}

pub fn simulate_mission_twin_counterfactuals(
    snapshots: &[MissionTwinSnapshotEnvelope],
    requests: &[MissionTwinCounterfactualRequest],
) -> Result<MissionTwinCounterfactualReplayReport, MissionTwinReplayError> {
    let live_surface = build_mission_twin_replay_surface_data(snapshots, None, None)?;
    let live_blockers = live_execution_blockers(&live_surface);
    let live_plan = counterfactual_plan(
        "live",
        false,
        Vec::new(),
        None,
        live_blockers.clone(),
        live_surface,
    );

    let mut counterfactual_plans = Vec::with_capacity(requests.len());
    for request in requests {
        validate_counterfactual_request(request)?;
        let mut simulated_snapshots = snapshots.to_vec();
        for snapshot in &mut simulated_snapshots {
            apply_counterfactual_request(snapshot, request);
        }
        let surface = build_mission_twin_replay_surface_data(&simulated_snapshots, None, None)?;
        counterfactual_plans.push(counterfactual_plan(
            &request.scenario_id,
            true,
            request.toggles.clone(),
            request.proof_lane_budget,
            live_blockers.clone(),
            surface,
        ));
    }

    let mut reason_codes = vec![
        "mission_twin.counterfactual.simulated".to_string(),
        "mission_twin.counterfactual.side_effect_free".to_string(),
    ];
    for request in requests {
        for toggle in &request.toggles {
            push_unique(&mut reason_codes, toggle.reason_code());
        }
    }
    reason_codes.sort();

    Ok(MissionTwinCounterfactualReplayReport {
        schema_version: MISSION_TWIN_COUNTERFACTUAL_SCHEMA_VERSION,
        contract_id: MISSION_TWIN_COUNTERFACTUAL_CONTRACT_ID.to_string(),
        source_bead: MISSION_TWIN_COUNTERFACTUAL_SOURCE_BEAD.to_string(),
        simulated: true,
        side_effects_executed: false,
        raw_pane_content_stored: false,
        forbidden_actions: mission_twin_forbidden_action_names(),
        reason_codes,
        live_plan,
        counterfactual_plans,
    })
}

#[must_use]
pub fn classify_mission_twin_proof_lanes(
    surface: &MissionObjectivePlanSurfaceData,
) -> MissionTwinProofLaneBrokerReport {
    let mut decisions = surface
        .plan
        .plan_steps
        .iter()
        .chain(&surface.plan.fallback_steps)
        .map(proof_lane_decision)
        .collect::<Vec<_>>();
    decisions.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));

    MissionTwinProofLaneBrokerReport {
        source_bead: MISSION_TWIN_COUNTERFACTUAL_SOURCE_BEAD.to_string(),
        simulated: true,
        decisions,
    }
}

pub fn simulate_mission_twin_ownership_handoff(
    snapshots: &[MissionTwinSnapshotEnvelope],
    request: &MissionTwinOwnershipSimulationRequest,
) -> Result<MissionTwinOwnershipSimulationReport, MissionTwinReplayError> {
    let ordered = ordered_validated_snapshots(snapshots)?;
    let owned_paths = normalized_unique_owned_paths(&request.owned_paths);
    let stale_after_seconds = request.stale_after_seconds.max(60);
    let fallback_only_coordination = request.fallback_only_coordination
        || ordered.iter().any(|snapshot| {
            snapshot.sources.agent_mail.availability_state != AgentMailAvailabilityState::Healthy
        });

    let mut active_agents = Vec::new();
    let mut owner_summaries = Vec::new();
    let mut dirty_overlaps = Vec::new();
    let mut reservation_overlaps = Vec::new();

    for snapshot in ordered {
        extend_active_agents(&mut active_agents, snapshot);
        extend_owner_summaries(
            &mut owner_summaries,
            snapshot,
            request.target_bead_id.as_deref(),
            stale_after_seconds,
        );
        extend_dirty_overlaps(&mut dirty_overlaps, snapshot, &owned_paths);
        extend_reservation_overlaps(&mut reservation_overlaps, snapshot, &owned_paths);
    }

    active_agents.sort();
    owner_summaries.sort_by(|left, right| {
        left.bead_id
            .cmp(&right.bead_id)
            .then(left.assignee.cmp(&right.assignee))
            .then(left.age_seconds.cmp(&right.age_seconds))
    });
    owner_summaries.dedup();
    dirty_overlaps.sort_by(compare_ownership_overlaps);
    dirty_overlaps.dedup();
    reservation_overlaps.sort_by(compare_ownership_overlaps);
    reservation_overlaps.dedup();

    let handoff_state = mission_twin_ownership_handoff_state(
        &owner_summaries,
        &dirty_overlaps,
        &reservation_overlaps,
        fallback_only_coordination,
        !active_agents.is_empty(),
    );
    let next_actions =
        mission_twin_ownership_next_actions(handoff_state, fallback_only_coordination);
    let reason_codes = mission_twin_ownership_reason_codes(
        handoff_state,
        &owned_paths,
        &owner_summaries,
        &dirty_overlaps,
        &reservation_overlaps,
        fallback_only_coordination,
    );

    Ok(MissionTwinOwnershipSimulationReport {
        schema_version: MISSION_TWIN_OWNERSHIP_SIMULATOR_SCHEMA_VERSION,
        contract_id: MISSION_TWIN_OWNERSHIP_SIMULATOR_CONTRACT_ID.to_string(),
        source_bead: MISSION_TWIN_OWNERSHIP_SIMULATOR_SOURCE_BEAD.to_string(),
        simulated: true,
        side_effects_executed: false,
        raw_pane_content_stored: false,
        forbidden_actions: mission_twin_forbidden_action_names(),
        candidate_id: if request.candidate_id.trim().is_empty() {
            "candidate.unknown".to_string()
        } else {
            request.candidate_id.clone()
        },
        target_bead_id: request.target_bead_id.clone(),
        owned_paths,
        stale_after_seconds,
        fallback_only_coordination,
        active_agents,
        owner_summaries,
        dirty_overlaps,
        reservation_overlaps,
        handoff_state,
        next_actions,
        reason_codes,
    })
}

#[must_use]
pub fn classify_mission_twin_owned_path_overlap(
    owned_path: &str,
    observed_path: &str,
) -> Option<MissionTwinOwnershipOverlapKind> {
    let owned_path = normalized_overlap_path(owned_path)?;
    let observed_path = normalized_overlap_path(observed_path)?;

    if owned_path == observed_path {
        return Some(MissionTwinOwnershipOverlapKind::Exact);
    }
    if glob_like_path_overlap(&owned_path, &observed_path) {
        return Some(MissionTwinOwnershipOverlapKind::GlobLike);
    }
    if parent_child_path_overlap(&owned_path, &observed_path) {
        return Some(MissionTwinOwnershipOverlapKind::ParentChild);
    }
    None
}

#[must_use]
pub const fn default_mission_twin_ownership_stale_after_seconds() -> u64 {
    DEFAULT_STALE_AFTER_SECONDS
}

fn normalized_unique_owned_paths(paths: &[String]) -> Vec<String> {
    let mut normalized_paths = Vec::new();
    for path in paths {
        if let Some(path) = normalized_overlap_path(path) {
            if is_safe_repo_relative_path(&path) {
                push_unique(&mut normalized_paths, path);
            }
        }
    }
    normalized_paths.sort();
    normalized_paths
}

fn extend_active_agents(active_agents: &mut Vec<String>, snapshot: &MissionTwinSnapshotEnvelope) {
    for agent in &snapshot.sources.agent_mail.active_agents {
        push_unique(active_agents, agent.agent_name.clone());
    }
}

fn extend_owner_summaries(
    owner_summaries: &mut Vec<MissionTwinOwnershipOwnerSummary>,
    snapshot: &MissionTwinSnapshotEnvelope,
    target_bead_id: Option<&str>,
    stale_after_seconds: u64,
) {
    for owner in &snapshot.sources.beads.owner_states {
        if !owner_matches_target(&owner.bead_id, target_bead_id) {
            continue;
        }
        let stale = owner.owner_state == OwnerState::StaleCandidate
            || owner.age_seconds >= stale_after_seconds;
        push_unique_owner_summary(
            owner_summaries,
            MissionTwinOwnershipOwnerSummary {
                bead_id: owner.bead_id.clone(),
                assignee: owner.assignee.clone(),
                owner_state: owner.owner_state,
                age_seconds: owner.age_seconds,
                stale_after_seconds,
                stale,
                last_activity_source: owner.last_activity_source.clone(),
                reason_codes: sorted_reason_codes(owner.reason_codes.clone()),
            },
        );
    }

    for owner in &snapshot.sources.beads.stale_owner_candidates {
        if !owner_matches_target(&owner.bead_id, target_bead_id) {
            continue;
        }
        push_unique_owner_summary(
            owner_summaries,
            MissionTwinOwnershipOwnerSummary {
                bead_id: owner.bead_id.clone(),
                assignee: owner.assignee.clone(),
                owner_state: OwnerState::StaleCandidate,
                age_seconds: owner.age_seconds,
                stale_after_seconds,
                stale: true,
                last_activity_source: owner.last_activity_source.clone(),
                reason_codes: sorted_reason_codes(owner.reason_codes.clone()),
            },
        );
    }
}

fn extend_dirty_overlaps(
    dirty_overlaps: &mut Vec<MissionTwinOwnershipPathOverlap>,
    snapshot: &MissionTwinSnapshotEnvelope,
    owned_paths: &[String],
) {
    for owned_path in owned_paths {
        for dirty_path in &snapshot.sources.git.dirty_paths {
            if let Some(overlap_kind) =
                classify_mission_twin_owned_path_overlap(owned_path, &dirty_path.path)
            {
                push_unique_overlap(
                    dirty_overlaps,
                    MissionTwinOwnershipPathOverlap {
                        source: MissionTwinOwnershipOverlapSource::DirtyPath,
                        overlap_kind,
                        owned_path: owned_path.clone(),
                        matched_path: dirty_path.path.clone(),
                        status: Some(dirty_path.status.clone()),
                        holder: None,
                        exclusive: false,
                    },
                );
            }
        }

        for overlap_path in &snapshot.sources.git.overlap_paths {
            if let Some(overlap_kind) =
                classify_mission_twin_owned_path_overlap(owned_path, overlap_path)
            {
                push_unique_overlap(
                    dirty_overlaps,
                    MissionTwinOwnershipPathOverlap {
                        source: MissionTwinOwnershipOverlapSource::DirtyPath,
                        overlap_kind,
                        owned_path: owned_path.clone(),
                        matched_path: overlap_path.clone(),
                        status: None,
                        holder: None,
                        exclusive: false,
                    },
                );
            }
        }
    }
}

fn extend_reservation_overlaps(
    reservation_overlaps: &mut Vec<MissionTwinOwnershipPathOverlap>,
    snapshot: &MissionTwinSnapshotEnvelope,
    owned_paths: &[String],
) {
    for owned_path in owned_paths {
        for reservation in &snapshot.sources.reservations.active_reservations {
            if let Some(overlap_kind) =
                classify_mission_twin_owned_path_overlap(owned_path, &reservation.path_pattern)
            {
                push_unique_overlap(
                    reservation_overlaps,
                    MissionTwinOwnershipPathOverlap {
                        source: MissionTwinOwnershipOverlapSource::Reservation,
                        overlap_kind,
                        owned_path: owned_path.clone(),
                        matched_path: reservation.path_pattern.clone(),
                        status: None,
                        holder: Some(reservation.holder.clone()),
                        exclusive: reservation.exclusive,
                    },
                );
            }
        }
    }
}

fn mission_twin_ownership_handoff_state(
    owner_summaries: &[MissionTwinOwnershipOwnerSummary],
    dirty_overlaps: &[MissionTwinOwnershipPathOverlap],
    reservation_overlaps: &[MissionTwinOwnershipPathOverlap],
    fallback_only_coordination: bool,
    active_agents_present: bool,
) -> MissionTwinOwnershipHandoffState {
    if !dirty_overlaps.is_empty() {
        return MissionTwinOwnershipHandoffState::UnsafeOverlap;
    }
    if owner_summaries.iter().any(|owner| owner.stale) {
        return MissionTwinOwnershipHandoffState::StaleCheckNeeded;
    }
    if !reservation_overlaps.is_empty() || !owner_summaries.is_empty() {
        return MissionTwinOwnershipHandoffState::HandoffRequired;
    }
    if fallback_only_coordination && active_agents_present {
        return MissionTwinOwnershipHandoffState::Active;
    }
    MissionTwinOwnershipHandoffState::SafeToOpen
}

fn mission_twin_ownership_next_actions(
    state: MissionTwinOwnershipHandoffState,
    fallback_only_coordination: bool,
) -> Vec<MissionTwinOwnershipNextAction> {
    match state {
        MissionTwinOwnershipHandoffState::UnsafeOverlap => vec![
            MissionTwinOwnershipNextAction::Wait,
            MissionTwinOwnershipNextAction::AskOwner,
            MissionTwinOwnershipNextAction::ChoosePlanningOnlyWork,
            MissionTwinOwnershipNextAction::RunStaticOnlyVerifier,
        ],
        MissionTwinOwnershipHandoffState::StaleCheckNeeded => vec![
            MissionTwinOwnershipNextAction::Comment,
            MissionTwinOwnershipNextAction::AskOwner,
            MissionTwinOwnershipNextAction::RunStaticOnlyVerifier,
        ],
        MissionTwinOwnershipHandoffState::HandoffRequired if fallback_only_coordination => vec![
            MissionTwinOwnershipNextAction::Comment,
            MissionTwinOwnershipNextAction::ChoosePlanningOnlyWork,
            MissionTwinOwnershipNextAction::RunStaticOnlyVerifier,
        ],
        MissionTwinOwnershipHandoffState::HandoffRequired => vec![
            MissionTwinOwnershipNextAction::AskOwner,
            MissionTwinOwnershipNextAction::Comment,
            MissionTwinOwnershipNextAction::ChoosePlanningOnlyWork,
        ],
        MissionTwinOwnershipHandoffState::Active => vec![
            MissionTwinOwnershipNextAction::Wait,
            MissionTwinOwnershipNextAction::ChoosePlanningOnlyWork,
            MissionTwinOwnershipNextAction::RunStaticOnlyVerifier,
        ],
        MissionTwinOwnershipHandoffState::SafeToOpen => {
            vec![MissionTwinOwnershipNextAction::RunStaticOnlyVerifier]
        }
    }
}

fn mission_twin_ownership_reason_codes(
    state: MissionTwinOwnershipHandoffState,
    owned_paths: &[String],
    owner_summaries: &[MissionTwinOwnershipOwnerSummary],
    dirty_overlaps: &[MissionTwinOwnershipPathOverlap],
    reservation_overlaps: &[MissionTwinOwnershipPathOverlap],
    fallback_only_coordination: bool,
) -> Vec<String> {
    let mut reason_codes = vec![
        "mission_twin.ownership.simulated".to_string(),
        "mission_twin.ownership.side_effect_free".to_string(),
        format!(
            "mission_twin.ownership.state.{}",
            ownership_state_name(state)
        ),
    ];

    if owned_paths.is_empty() {
        push_unique(
            &mut reason_codes,
            "mission_twin.ownership.empty_owned_paths",
        );
    }
    if !dirty_overlaps.is_empty() {
        push_unique(&mut reason_codes, "mission_twin.ownership.dirty_overlap");
    }
    if !reservation_overlaps.is_empty() {
        push_unique(
            &mut reason_codes,
            "mission_twin.ownership.reservation_overlap",
        );
    }
    if owner_summaries.iter().any(|owner| owner.stale) {
        push_unique(
            &mut reason_codes,
            "mission_twin.ownership.stale_check_needed",
        );
    } else if !owner_summaries.is_empty() {
        push_unique(
            &mut reason_codes,
            "mission_twin.ownership.active_owner_handoff",
        );
    }
    if fallback_only_coordination {
        push_unique(
            &mut reason_codes,
            "mission_twin.ownership.fallback_only_coordination",
        );
    }
    reason_codes.sort();
    reason_codes
}

fn ownership_state_name(state: MissionTwinOwnershipHandoffState) -> &'static str {
    match state {
        MissionTwinOwnershipHandoffState::Active => "active",
        MissionTwinOwnershipHandoffState::StaleCheckNeeded => "stale_check_needed",
        MissionTwinOwnershipHandoffState::HandoffRequired => "handoff_required",
        MissionTwinOwnershipHandoffState::SafeToOpen => "safe_to_open",
        MissionTwinOwnershipHandoffState::UnsafeOverlap => "unsafe_overlap",
    }
}

fn owner_matches_target(owner_bead_id: &str, target_bead_id: Option<&str>) -> bool {
    match target_bead_id {
        Some(target_bead_id) => owner_bead_id == target_bead_id,
        None => true,
    }
}

fn compare_ownership_overlaps(
    left: &MissionTwinOwnershipPathOverlap,
    right: &MissionTwinOwnershipPathOverlap,
) -> Ordering {
    ownership_overlap_source_name(left.source)
        .cmp(ownership_overlap_source_name(right.source))
        .then(left.owned_path.cmp(&right.owned_path))
        .then(left.matched_path.cmp(&right.matched_path))
        .then(left.holder.cmp(&right.holder))
        .then(left.exclusive.cmp(&right.exclusive))
}

fn ownership_overlap_source_name(source: MissionTwinOwnershipOverlapSource) -> &'static str {
    match source {
        MissionTwinOwnershipOverlapSource::DirtyPath => "dirty_path",
        MissionTwinOwnershipOverlapSource::Reservation => "reservation",
    }
}

fn push_unique_overlap(
    overlaps: &mut Vec<MissionTwinOwnershipPathOverlap>,
    overlap: MissionTwinOwnershipPathOverlap,
) {
    if !overlaps.iter().any(|existing| {
        existing.source == overlap.source
            && existing.owned_path == overlap.owned_path
            && existing.matched_path == overlap.matched_path
            && existing.holder == overlap.holder
            && existing.exclusive == overlap.exclusive
    }) {
        overlaps.push(overlap);
    }
}

fn push_unique_owner_summary(
    owner_summaries: &mut Vec<MissionTwinOwnershipOwnerSummary>,
    owner_summary: MissionTwinOwnershipOwnerSummary,
) {
    if !owner_summaries.contains(&owner_summary) {
        owner_summaries.push(owner_summary);
    }
}

fn normalized_overlap_path(path: &str) -> Option<String> {
    let path = path.trim().trim_start_matches("./").trim_end_matches('/');
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn glob_like_path_overlap(left: &str, right: &str) -> bool {
    glob_pattern_matches(left, right) || glob_pattern_matches(right, left)
}

fn glob_pattern_matches(pattern: &str, path: &str) -> bool {
    let Some(prefix) = pattern.strip_suffix("/*") else {
        return false;
    };
    let Some(prefix) = normalized_overlap_path(prefix) else {
        return false;
    };
    path.strip_prefix(&prefix)
        .is_some_and(|tail| tail.starts_with('/') && tail.len() > 1)
}

fn parent_child_path_overlap(left: &str, right: &str) -> bool {
    let left_prefix = format!("{left}/");
    let right_prefix = format!("{right}/");
    right.starts_with(&left_prefix) || left.starts_with(&right_prefix)
}

fn mission_twin_forbidden_action_names() -> Vec<String> {
    vec![
        "agent_mail_service_repair_restart".to_string(),
        "rch_service_repair_restart".to_string(),
        "worker_mutation".to_string(),
        "build_cancellation".to_string(),
        "file_deletion".to_string(),
        "destructive_git".to_string(),
        "local_cargo_proof".to_string(),
        "pane_mutation".to_string(),
        "raw_pane_content_storage".to_string(),
        "beads_mutation".to_string(),
    ]
}

fn validate_surface_request(
    request: &MissionTwinSurfaceRequest,
) -> Result<(), MissionTwinReplayError> {
    match request.action {
        MissionTwinSurfaceAction::CurrentPlan => Ok(()),
        MissionTwinSurfaceAction::Simulate => Ok(()),
        MissionTwinSurfaceAction::ExplainStep => require_non_empty_surface_query(
            request,
            request.explain_step.as_deref(),
            "explain_step",
        ),
        MissionTwinSurfaceAction::ExplainReason => require_non_empty_surface_query(
            request,
            request.explain_reason.as_deref(),
            "explain_reason",
        ),
    }
}

fn require_non_empty_surface_query(
    request: &MissionTwinSurfaceRequest,
    value: Option<&str>,
    field: &str,
) -> Result<(), MissionTwinReplayError> {
    if value.is_some_and(|query| !query.trim().is_empty()) {
        return Ok(());
    }
    Err(MissionTwinReplayError::InvalidSurfaceRequest {
        action: request.action.as_str().to_string(),
        reason: format!("{field} is required"),
    })
}

fn mission_twin_surface_artifact_paths(
    snapshots: &[MissionTwinSnapshotEnvelope],
    requested_snapshot_paths: &[String],
) -> Vec<String> {
    let mut paths = safe_unique_repo_paths(requested_snapshot_paths);
    for snapshot in snapshots {
        for path in &snapshot.artifact_paths {
            if is_safe_repo_relative_path(path) {
                push_unique(&mut paths, path.clone());
            }
        }
    }
    paths.sort();
    paths
}

fn safe_unique_repo_paths(paths: &[String]) -> Vec<String> {
    let mut safe_paths = Vec::new();
    for path in paths {
        let path = path.trim();
        if is_safe_repo_relative_path(path) {
            push_unique(&mut safe_paths, path.to_string());
        }
    }
    safe_paths.sort();
    safe_paths
}

fn mission_twin_surface_reason_codes(
    action: MissionTwinSurfaceAction,
    plan_surface: &MissionObjectivePlanSurfaceData,
    counterfactual_report: Option<&MissionTwinCounterfactualReplayReport>,
    ownership_report: Option<&MissionTwinOwnershipSimulationReport>,
) -> Vec<String> {
    let mut reason_codes = vec![
        "mission_twin.surface.read_only".to_string(),
        "mission_twin.surface.side_effect_free".to_string(),
        format!("mission_twin.surface.action.{}", action.as_str()),
    ];
    for reason_code in &plan_surface.reason_codes {
        push_unique(&mut reason_codes, reason_code.clone());
    }
    if let Some(explain) = &plan_surface.explain {
        push_unique(
            &mut reason_codes,
            format!(
                "mission_twin.surface.explain.{}",
                explain_mode_name(explain.mode)
            ),
        );
        if explain.matched {
            push_unique(&mut reason_codes, "mission_twin.surface.explain.matched");
        } else {
            push_unique(&mut reason_codes, "mission_twin.surface.explain.unmatched");
        }
    }
    if let Some(report) = counterfactual_report {
        for reason_code in &report.reason_codes {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }
    if let Some(report) = ownership_report {
        for reason_code in &report.reason_codes {
            push_unique(&mut reason_codes, reason_code.clone());
        }
    }
    reason_codes.sort();
    reason_codes
}

fn explain_mode_name(
    mode: crate::mission_objective_plan::MissionObjectivePlanExplainMode,
) -> &'static str {
    match mode {
        crate::mission_objective_plan::MissionObjectivePlanExplainMode::Step => "step",
        crate::mission_objective_plan::MissionObjectivePlanExplainMode::Reason => "reason",
    }
}

fn validate_counterfactual_request(
    request: &MissionTwinCounterfactualRequest,
) -> Result<(), MissionTwinReplayError> {
    let scenario_id = request.scenario_id.trim();
    if scenario_id.is_empty() {
        return invalid_counterfactual(request, "scenario_id is required");
    }
    if request.toggles.is_empty() {
        return invalid_counterfactual(request, "at least one allowlisted toggle is required");
    }

    let mut seen = Vec::new();
    for toggle in &request.toggles {
        if seen.contains(toggle) {
            return invalid_counterfactual(
                request,
                format!("duplicate toggle {}", toggle.reason_code()),
            );
        }
        seen.push(*toggle);
    }

    let has_budget_toggle = request
        .toggles
        .contains(&MissionTwinCounterfactualToggle::ProofLanesBudgeted);
    match (has_budget_toggle, request.proof_lane_budget) {
        (true, None) => {
            invalid_counterfactual(request, "proof_lanes_budgeted requires proof_lane_budget")
        }
        (false, Some(_)) => invalid_counterfactual(
            request,
            "proof_lane_budget is forbidden without proof_lanes_budgeted",
        ),
        (true, Some(budget)) => {
            if budget.remote_cargo_lanes == 0 && budget.static_verifier_lanes == 0 {
                return invalid_counterfactual(
                    request,
                    "proof_lane_budget must enable at least one lane",
                );
            }
            if budget.remote_cargo_lanes > MAX_COUNTERFACTUAL_PROOF_LANES
                || budget.static_verifier_lanes > MAX_COUNTERFACTUAL_PROOF_LANES
            {
                return invalid_counterfactual(
                    request,
                    format!(
                        "proof_lane_budget exceeds max lane count {}",
                        MAX_COUNTERFACTUAL_PROOF_LANES
                    ),
                );
            }
            Ok(())
        }
        (false, None) => Ok(()),
    }
}

fn invalid_counterfactual<T>(
    request: &MissionTwinCounterfactualRequest,
    reason: impl Into<String>,
) -> Result<T, MissionTwinReplayError> {
    Err(MissionTwinReplayError::InvalidCounterfactual {
        scenario_id: if request.scenario_id.trim().is_empty() {
            "<missing>".to_string()
        } else {
            request.scenario_id.clone()
        },
        reason: reason.into(),
    })
}

fn counterfactual_plan(
    scenario_id: &str,
    simulated: bool,
    toggles: Vec<MissionTwinCounterfactualToggle>,
    proof_lane_budget: Option<MissionTwinProofLaneBudget>,
    live_blockers: Vec<String>,
    surface: MissionObjectivePlanSurfaceData,
) -> MissionTwinCounterfactualPlan {
    let remaining_blockers = live_execution_blockers(&surface);
    let unblocked_reason_codes = toggles
        .iter()
        .map(|toggle| toggle.reason_code().to_string())
        .collect::<Vec<_>>();
    let proof_lane_broker = classify_mission_twin_proof_lanes(&surface);

    MissionTwinCounterfactualPlan {
        scenario_id: scenario_id.to_string(),
        simulated,
        toggles,
        proof_lane_budget,
        plan_status: surface.plan_status,
        risk_level: surface.risk_level,
        live_execution_blocked_by: live_blockers,
        remaining_blockers,
        unblocked_reason_codes,
        proof_lane_broker,
        surface,
    }
}

fn apply_counterfactual_request(
    snapshot: &mut MissionTwinSnapshotEnvelope,
    request: &MissionTwinCounterfactualRequest,
) {
    for toggle in &request.toggles {
        match toggle {
            MissionTwinCounterfactualToggle::RchRecovered => apply_rch_recovered(snapshot),
            MissionTwinCounterfactualToggle::AgentMailRecovered => {
                apply_agent_mail_recovered(snapshot);
            }
            MissionTwinCounterfactualToggle::DirtyOverlapCleared => {
                apply_dirty_overlap_cleared(snapshot);
            }
            MissionTwinCounterfactualToggle::OwnerHandoffAccepted => {
                apply_owner_handoff_accepted(snapshot);
            }
            MissionTwinCounterfactualToggle::TargetClassProofAvailable => {
                apply_target_class_proof_available(snapshot);
            }
            MissionTwinCounterfactualToggle::ProofLanesBudgeted => {
                if let Some(budget) = request.proof_lane_budget {
                    apply_proof_lane_budget(snapshot, budget);
                }
            }
        }
    }
}

fn apply_rch_recovered(snapshot: &mut MissionTwinSnapshotEnvelope) {
    let rch = &mut snapshot.sources.rch;
    rch.admission_state = RchAdmissionState::Ready;
    rch.total_workers = rch.total_workers.max(1);
    rch.healthy_workers = rch.healthy_workers.max(1).min(rch.total_workers);
    rch.critical_pressure_count = 0;
    rch.admission_reasons.clear();
    rch.blocked_proof_lanes.clear();
    rch.evidence.status = SourceStatus::Available;
    rch.evidence.freshness_state = FreshnessState::Fresh;
    retain_reason_codes_without_prefix(&mut rch.evidence.reason_codes, &["rch."]);
    push_unique(
        &mut rch.evidence.reason_codes,
        MissionTwinCounterfactualToggle::RchRecovered.reason_code(),
    );

    let operating_envelope = &mut snapshot.sources.operating_envelope;
    if operating_envelope
        .reason_codes
        .iter()
        .chain(&operating_envelope.evidence.reason_codes)
        .any(|reason| reason.starts_with("rch."))
    {
        operating_envelope.verdict = OperatingEnvelopeVerdict::Admit;
        operating_envelope.evidence.status = SourceStatus::Available;
        retain_reason_codes_without_prefix(&mut operating_envelope.reason_codes, &["rch."]);
        retain_reason_codes_without_prefix(
            &mut operating_envelope.evidence.reason_codes,
            &["rch."],
        );
        push_unique(
            &mut operating_envelope.reason_codes,
            "mission_twin.counterfactual.operating_envelope_rch_recovered",
        );
    }
}

fn apply_agent_mail_recovered(snapshot: &mut MissionTwinSnapshotEnvelope) {
    let agent_mail = &mut snapshot.sources.agent_mail;
    agent_mail.availability_state = AgentMailAvailabilityState::Healthy;
    agent_mail.fallback_reason_codes.clear();
    agent_mail.evidence.status = SourceStatus::Available;
    agent_mail.evidence.freshness_state = FreshnessState::Fresh;
    agent_mail.evidence.collected_at_ms = Some(snapshot.generated_at_ms);
    agent_mail.evidence.freshness_ms = Some(0);
    agent_mail.evidence.evidence_level = EvidenceLevel::Fixture;
    retain_reason_codes_without_prefix(&mut agent_mail.evidence.reason_codes, &["agent_mail."]);
    push_unique(
        &mut agent_mail.evidence.reason_codes,
        MissionTwinCounterfactualToggle::AgentMailRecovered.reason_code(),
    );
}

fn apply_dirty_overlap_cleared(snapshot: &mut MissionTwinSnapshotEnvelope) {
    let git = &mut snapshot.sources.git;
    git.dirty_paths.retain(|path| !path.overlaps_owned_path);
    git.overlap_paths.clear();
    retain_reason_codes_without_fragment(
        &mut git.evidence.reason_codes,
        &[
            "dirty_overlap",
            "mission_twin.dirty_overlap",
            "git.dirty_paths_present",
        ],
    );
    if git.dirty_paths.is_empty() && git.overlap_paths.is_empty() {
        git.evidence.status = SourceStatus::Available;
    }
    push_unique(
        &mut git.evidence.reason_codes,
        MissionTwinCounterfactualToggle::DirtyOverlapCleared.reason_code(),
    );

    let operating_envelope = &mut snapshot.sources.operating_envelope;
    retain_reason_codes_without_fragment(
        &mut operating_envelope.reason_codes,
        &["dirty_overlap", "mission_twin.dirty_overlap"],
    );
    retain_reason_codes_without_fragment(
        &mut operating_envelope.evidence.reason_codes,
        &["dirty_overlap", "mission_twin.dirty_overlap"],
    );
    if operating_envelope.reason_codes.is_empty()
        && operating_envelope.evidence.reason_codes.is_empty()
        && operating_envelope.verdict == OperatingEnvelopeVerdict::Admit
    {
        operating_envelope.evidence.status = SourceStatus::Available;
    }
}

fn apply_owner_handoff_accepted(snapshot: &mut MissionTwinSnapshotEnvelope) {
    snapshot.sources.beads.owner_states.clear();
    snapshot.sources.beads.stale_owner_candidates.clear();
    retain_reason_codes_without_prefix(
        &mut snapshot.sources.beads.evidence.reason_codes,
        &["beads.owner_", "mission_twin.active_owner"],
    );
    push_unique(
        &mut snapshot.sources.beads.evidence.reason_codes,
        MissionTwinCounterfactualToggle::OwnerHandoffAccepted.reason_code(),
    );

    snapshot
        .sources
        .reservations
        .active_reservations
        .retain(|reservation| !reservation.exclusive);
    push_unique(
        &mut snapshot.sources.reservations.evidence.reason_codes,
        "mission_twin.counterfactual.reservation_handoff_accepted",
    );
}

fn apply_target_class_proof_available(snapshot: &mut MissionTwinSnapshotEnvelope) {
    let operating_envelope = &mut snapshot.sources.operating_envelope;
    operating_envelope.verdict = OperatingEnvelopeVerdict::Admit;
    operating_envelope.evidence.status = SourceStatus::Available;
    retain_reason_codes_without_fragment(
        &mut operating_envelope.reason_codes,
        &[
            "proof",
            "target",
            "capacity.pause",
            "capacity.defer",
            "operating_envelope.shed",
            "operating_envelope.deny",
        ],
    );
    retain_reason_codes_without_fragment(
        &mut operating_envelope.evidence.reason_codes,
        &[
            "proof",
            "target",
            "capacity.pause",
            "capacity.defer",
            "operating_envelope.shed",
            "operating_envelope.deny",
        ],
    );
    push_unique(
        &mut operating_envelope.reason_codes,
        MissionTwinCounterfactualToggle::TargetClassProofAvailable.reason_code(),
    );
}

fn apply_proof_lane_budget(
    snapshot: &mut MissionTwinSnapshotEnvelope,
    budget: MissionTwinProofLaneBudget,
) {
    let rch = &mut snapshot.sources.rch;
    if budget.remote_cargo_lanes > 0 && rch.admission_state == RchAdmissionState::Ready {
        rch.blocked_proof_lanes.clear();
        push_unique(
            &mut rch.evidence.reason_codes,
            "mission_twin.counterfactual.remote_cargo_lane_available",
        );
    }
    if budget.static_verifier_lanes > 0 {
        push_unique(
            &mut rch.evidence.reason_codes,
            "mission_twin.counterfactual.static_verifier_lane_available",
        );
    }
}

fn proof_lane_decision(step: &MissionObjectivePlanStep) -> MissionTwinProofLaneDecision {
    let lane_class = proof_lane_class(step);
    MissionTwinProofLaneDecision {
        candidate_id: step.candidate_id.clone(),
        target_bead_id: step.target_bead_id.clone(),
        lane_class,
        proof_lane: step.proof_lane,
        status: step.status,
        required_approvals: step.required_approvals.clone(),
        reason_codes: step.reason_codes.clone(),
        live_execution_blocked_by: step_live_blockers(step),
    }
}

fn proof_lane_class(step: &MissionObjectivePlanStep) -> MissionTwinProofLaneClass {
    if step.proof_lane == MissionObjectiveProofLane::RchCargo {
        return MissionTwinProofLaneClass::RemoteCargo;
    }
    if step.proof_lane == MissionObjectiveProofLane::StaticSchema
        || step.action_kind == MissionObjectiveActionKind::RunTestingSkill
    {
        return MissionTwinProofLaneClass::StaticVerifier;
    }
    if step.proof_lane == MissionObjectiveProofLane::Blocked
        || step.status == MissionObjectivePlanStatus::RchSubstrateBlocked
        || step
            .required_approvals
            .contains(&MissionObjectiveApprovalRequirement::RchRecovered)
    {
        return MissionTwinProofLaneClass::WaitingRch;
    }
    if matches!(
        step.status,
        MissionObjectivePlanStatus::WaitingOwner | MissionObjectivePlanStatus::DirtyOverlap
    ) || step
        .required_approvals
        .contains(&MissionObjectiveApprovalRequirement::OwnerHandoff)
    {
        return MissionTwinProofLaneClass::WaitingOwner;
    }
    if matches!(
        step.action_kind,
        MissionObjectiveActionKind::AddBeadsComment
            | MissionObjectiveActionKind::RunBvRobotTriage
            | MissionObjectiveActionKind::StatusCheckBeforeReopen
    ) || step.side_effect_class == MissionObjectiveSideEffectClass::CoordinationMutation
    {
        return MissionTwinProofLaneClass::CoordinationOnly;
    }
    MissionTwinProofLaneClass::NotRequired
}

fn live_execution_blockers(surface: &MissionObjectivePlanSurfaceData) -> Vec<String> {
    let mut blockers = Vec::new();
    for reason_code in &surface.reason_codes {
        push_blocker_for_reason(&mut blockers, reason_code);
    }
    for step in surface
        .plan
        .plan_steps
        .iter()
        .chain(&surface.plan.fallback_steps)
    {
        for blocker in step_live_blockers(step) {
            push_unique(&mut blockers, blocker);
        }
    }
    if blockers.is_empty()
        && !matches!(
            surface.plan_status,
            MissionObjectivePlanStatus::Actionable | MissionObjectivePlanStatus::PlanningOnly
        )
    {
        push_unique(
            &mut blockers,
            "mission_twin.counterfactual.live_blocker_unknown",
        );
    }
    blockers.sort();
    blockers
}

fn step_live_blockers(step: &MissionObjectivePlanStep) -> Vec<String> {
    let mut blockers = Vec::new();
    if step.proof_lane == MissionObjectiveProofLane::Blocked
        || step
            .required_approvals
            .contains(&MissionObjectiveApprovalRequirement::RchRecovered)
    {
        push_unique(&mut blockers, "rch.recovery_required");
    }
    if step
        .required_approvals
        .contains(&MissionObjectiveApprovalRequirement::AgentMailRecovered)
    {
        push_unique(&mut blockers, "agent_mail.recovery_required");
    }
    if step
        .required_approvals
        .contains(&MissionObjectiveApprovalRequirement::CleanWorktree)
    {
        push_unique(&mut blockers, "dirty_overlap.clear_required");
    }
    if step
        .required_approvals
        .contains(&MissionObjectiveApprovalRequirement::OwnerHandoff)
    {
        push_unique(&mut blockers, "owner_handoff.required");
    }
    for reason_code in &step.reason_codes {
        push_blocker_for_reason(&mut blockers, reason_code);
    }
    blockers.sort();
    blockers
}

fn push_blocker_for_reason(blockers: &mut Vec<String>, reason_code: &str) {
    if reason_code.starts_with("rch.") || reason_code.contains("proof_substrate") {
        push_unique(blockers, "rch.recovery_required");
    }
    if reason_code.starts_with("agent_mail.") || reason_code == "swarm_tick_fallback" {
        push_unique(blockers, "agent_mail.recovery_required");
    }
    if reason_code.contains("dirty_overlap") || reason_code.starts_with("git.dirty") {
        push_unique(blockers, "dirty_overlap.clear_required");
    }
    if reason_code.contains("owner_handoff") || reason_code == "beads.owner_active" {
        push_unique(blockers, "owner_handoff.required");
    }
    if reason_code.contains("target_class")
        || reason_code.contains("target_hardware")
        || reason_code.contains("skipped_not_proven")
    {
        push_unique(blockers, "target_class.proof_required");
    }
}

fn ordered_validated_snapshots(
    snapshots: &[MissionTwinSnapshotEnvelope],
) -> Result<Vec<&MissionTwinSnapshotEnvelope>, MissionTwinReplayError> {
    if snapshots.is_empty() {
        return Err(MissionTwinReplayError::EmptySnapshotSet);
    }

    let mut ordered = snapshots.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| compare_snapshots(left, right));

    for snapshot in &ordered {
        snapshot
            .validate()
            .map_err(|error| MissionTwinReplayError::InvalidSnapshot {
                snapshot_id: display_snapshot_id(snapshot),
                error,
            })?;
    }

    Ok(ordered)
}

fn compare_snapshots(
    left: &MissionTwinSnapshotEnvelope,
    right: &MissionTwinSnapshotEnvelope,
) -> Ordering {
    left.snapshot_id
        .cmp(&right.snapshot_id)
        .then(left.generated_at_ms.cmp(&right.generated_at_ms))
}

fn display_snapshot_id(snapshot: &MissionTwinSnapshotEnvelope) -> String {
    if snapshot.snapshot_id.trim().is_empty() {
        "<missing>".to_string()
    } else {
        snapshot.snapshot_id.clone()
    }
}

fn append_source_snapshots(
    input: &mut MissionObjectivePlannerInput,
    snapshot: &MissionTwinSnapshotEnvelope,
) {
    input.source_snapshots.push(beads_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.beads,
    ));
    input.source_snapshots.push(rch_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.rch,
    ));
    input.source_snapshots.push(agent_mail_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.agent_mail,
    ));
    input.source_snapshots.push(git_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.git,
    ));
    input.source_snapshots.push(reservations_source_snapshot(
        &snapshot.snapshot_id,
        &snapshot.sources.reservations,
    ));
    input
        .source_snapshots
        .push(operating_envelope_source_snapshot(
            &snapshot.snapshot_id,
            &snapshot.sources.operating_envelope,
        ));
}

fn append_dirty_paths(
    input: &mut MissionObjectivePlannerInput,
    snapshot: &MissionTwinSnapshotEnvelope,
) {
    input.dirty_paths.extend(
        snapshot
            .sources
            .git
            .dirty_paths
            .iter()
            .map(dirty_path_for_planner),
    );
}

fn append_candidates(
    input: &mut MissionObjectivePlannerInput,
    snapshot: &MissionTwinSnapshotEnvelope,
) {
    input.candidates.extend(replay_candidates(snapshot));
}

fn replay_candidates(snapshot: &MissionTwinSnapshotEnvelope) -> Vec<MissionObjectiveCandidateWork> {
    if !snapshot.sources.beads.owner_states.is_empty() {
        return snapshot
            .sources
            .beads
            .owner_states
            .iter()
            .map(|owner| {
                let mut candidate = base_candidate(
                    snapshot,
                    &format!("owner.{}", owner.bead_id),
                    MissionObjectiveCandidateReadiness::ActiveSameDomain,
                    format!(
                        "Wait for active owner {} on {}",
                        owner.assignee, owner.bead_id
                    ),
                )
                .target_bead_id(&owner.bead_id)
                .active_owner(&owner.assignee, owner.age_seconds)
                .with_reason_code("mission_twin.owner_handoff_required");

                for reason_code in &owner.reason_codes {
                    candidate = candidate.with_reason_code(reason_code);
                }

                candidate
            })
            .collect();
    }

    if !snapshot.sources.beads.stale_owner_candidates.is_empty() {
        return snapshot
            .sources
            .beads
            .stale_owner_candidates
            .iter()
            .map(|owner| stale_owner_candidate(snapshot, owner))
            .collect();
    }

    if snapshot.sources.beads.ready_count == 0 {
        return Vec::new();
    }

    let mut candidate = base_candidate(
        snapshot,
        "ready-work",
        MissionObjectiveCandidateReadiness::ReadyBead,
        format!(
            "Replay {} Beads-ready candidate(s) from mission-twin snapshot {}",
            snapshot.sources.beads.ready_count, snapshot.snapshot_id
        ),
    )
    .with_reason_code("mission_twin.replay.ready_work_available");

    for path in ready_candidate_owned_paths(snapshot) {
        candidate = candidate.with_owned_path(path);
    }

    vec![candidate]
}

fn stale_owner_candidate(
    snapshot: &MissionTwinSnapshotEnvelope,
    owner: &StaleOwnerCandidate,
) -> MissionObjectiveCandidateWork {
    let mut candidate = base_candidate(
        snapshot,
        &format!("stale-owner.{}", owner.bead_id),
        MissionObjectiveCandidateReadiness::StaleReopenCandidate,
        format!(
            "Status-check stale owner {} on {}",
            owner.assignee, owner.bead_id
        ),
    )
    .target_bead_id(&owner.bead_id)
    .active_owner(&owner.assignee, owner.age_seconds)
    .stale_after_seconds(60)
    .with_reason_code("mission_twin.stale_owner_candidate");

    for reason_code in &owner.reason_codes {
        candidate = candidate.with_reason_code(reason_code);
    }

    candidate
}

fn base_candidate(
    snapshot: &MissionTwinSnapshotEnvelope,
    suffix: &str,
    readiness: MissionObjectiveCandidateReadiness,
    title: String,
) -> MissionObjectiveCandidateWork {
    let mut candidate = MissionObjectiveCandidateWork::new(
        replay_candidate_id(&snapshot.snapshot_id, suffix),
        readiness,
    )
    .title(title)
    .priority(candidate_priority(snapshot))
    .proof_availability(snapshot_proof_availability(snapshot))
    .capacity_posture(snapshot_capacity_posture(snapshot));

    for reason_code in all_snapshot_reason_codes(snapshot) {
        candidate = candidate.with_reason_code(reason_code);
    }

    candidate
}

fn replay_candidate_id(snapshot_id: &str, suffix: &str) -> String {
    format!("mission-twin.{snapshot_id}.{suffix}")
}

fn candidate_priority(snapshot: &MissionTwinSnapshotEnvelope) -> u8 {
    if snapshot.sources.rch.critical_pressure_count > 0
        || snapshot.sources.rch.admission_state != RchAdmissionState::Ready
    {
        1
    } else {
        2
    }
}

fn snapshot_proof_availability(
    snapshot: &MissionTwinSnapshotEnvelope,
) -> MissionObjectiveProofAvailability {
    if snapshot.sources.rch.admission_state != RchAdmissionState::Ready
        || snapshot.sources.rch.critical_pressure_count > 0
        || !snapshot.sources.rch.blocked_proof_lanes.is_empty()
    {
        MissionObjectiveProofAvailability::Blocked
    } else if target_class_proof_gap(snapshot) {
        MissionObjectiveProofAvailability::Available
    } else {
        MissionObjectiveProofAvailability::NotRequired
    }
}

fn target_class_proof_gap(snapshot: &MissionTwinSnapshotEnvelope) -> bool {
    if snapshot.sources.operating_envelope.verdict == OperatingEnvelopeVerdict::Admit {
        return false;
    }

    snapshot
        .sources
        .operating_envelope
        .reason_codes
        .iter()
        .chain(&snapshot.sources.operating_envelope.evidence.reason_codes)
        .any(|reason| {
            let reason = reason.to_ascii_lowercase();
            reason.contains("proof")
                || reason.contains("target_class")
                || reason.contains("target_hardware")
                || reason.contains("skipped_not_proven")
        })
}

fn snapshot_capacity_posture(
    snapshot: &MissionTwinSnapshotEnvelope,
) -> MissionObjectiveCapacityPosture {
    match snapshot.sources.operating_envelope.verdict {
        OperatingEnvelopeVerdict::Admit => MissionObjectiveCapacityPosture::Admit,
        OperatingEnvelopeVerdict::Deny => MissionObjectiveCapacityPosture::Defer,
        OperatingEnvelopeVerdict::Shed => MissionObjectiveCapacityPosture::Pause,
        OperatingEnvelopeVerdict::Unknown => MissionObjectiveCapacityPosture::Unknown,
    }
}

fn ready_candidate_owned_paths(snapshot: &MissionTwinSnapshotEnvelope) -> Vec<String> {
    let mut paths = Vec::new();
    extend_reasonless_paths(&mut paths, &snapshot.sources.git.overlap_paths);

    for dirty_path in &snapshot.sources.git.dirty_paths {
        if dirty_path.overlaps_owned_path {
            push_unique(&mut paths, dirty_path.path.clone());
        }
    }

    paths.sort();
    paths
}

fn dirty_path_for_planner(path: &DirtyPathSummary) -> MissionObjectiveDirtyPath {
    let category = if path.overlaps_owned_path {
        "owned_path_overlap"
    } else {
        "dirty_tree"
    };

    MissionObjectiveDirtyPath::new(&path.path, &path.status).category(category)
}

fn beads_source_snapshot(
    snapshot_id: &str,
    source: &BeadsMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "beads"),
        MissionObjectiveSourceKind::Beads,
        &source.evidence,
        MissionObjectiveEvidenceCategory::BeadsReadyQueue,
        format!(
            "ready={} blocked={} in_progress={}",
            source.ready_count, source.blocked_count, source.in_progress_count
        ),
        beads_reason_codes(source),
    )
}

fn rch_source_snapshot(
    snapshot_id: &str,
    source: &RchMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    let mut snapshot = source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "rch"),
        MissionObjectiveSourceKind::Rch,
        &source.evidence,
        MissionObjectiveEvidenceCategory::RchWorkerSelection,
        format!(
            "admission={} healthy_workers={}/{} critical_pressure={}",
            rch_admission_state_name(source.admission_state),
            source.healthy_workers,
            source.total_workers,
            source.critical_pressure_count
        ),
        rch_reason_codes(source),
    );

    if source.admission_state == RchAdmissionState::Unavailable {
        snapshot.state = MissionObjectiveSourceState::Unavailable;
    } else if source.admission_state != RchAdmissionState::Ready
        || source.critical_pressure_count > 0
        || !source.blocked_proof_lanes.is_empty()
    {
        snapshot.state = MissionObjectiveSourceState::Blocked;
    }

    snapshot
}

fn agent_mail_source_snapshot(
    snapshot_id: &str,
    source: &AgentMailMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    let mut snapshot = source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "agent_mail"),
        MissionObjectiveSourceKind::AgentMail,
        &source.evidence,
        MissionObjectiveEvidenceCategory::AgentMailAvailability,
        format!(
            "availability={} active_agents={}",
            agent_mail_availability_name(source.availability_state),
            source.active_agents.len()
        ),
        agent_mail_reason_codes(source),
    );

    snapshot.state = match source.availability_state {
        AgentMailAvailabilityState::Healthy => snapshot.state,
        AgentMailAvailabilityState::Red => MissionObjectiveSourceState::Unavailable,
        AgentMailAvailabilityState::Fallback | AgentMailAvailabilityState::Unknown => {
            MissionObjectiveSourceState::Degraded
        }
    };

    snapshot
}

fn git_source_snapshot(
    snapshot_id: &str,
    source: &GitMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "git"),
        MissionObjectiveSourceKind::Git,
        &source.evidence,
        MissionObjectiveEvidenceCategory::GitDirtyTree,
        format!(
            "branch={} dirty_paths={} overlap_paths={} deletion_paths_present={}",
            source.branch,
            source.dirty_paths.len(),
            source.overlap_paths.len(),
            source.deletion_paths_present
        ),
        git_reason_codes(source),
    )
}

fn reservations_source_snapshot(
    snapshot_id: &str,
    source: &ReservationsMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "reservations"),
        MissionObjectiveSourceKind::BlockerRadar,
        &source.evidence,
        MissionObjectiveEvidenceCategory::ActiveAssigneeOverlap,
        format!("active_reservations={}", source.active_reservations.len()),
        reservations_reason_codes(source),
    )
}

fn operating_envelope_source_snapshot(
    snapshot_id: &str,
    source: &OperatingEnvelopeMissionTwinSnapshot,
) -> MissionObjectiveSourceSnapshot {
    let mut snapshot = source_snapshot_from_evidence(
        mission_twin_replay_source_id(snapshot_id, "operating_envelope"),
        MissionObjectiveSourceKind::ResourceCockpit,
        &source.evidence,
        MissionObjectiveEvidenceCategory::CapacityPressure,
        format!(
            "operating_envelope_verdict={}",
            operating_envelope_verdict_name(source.verdict)
        ),
        operating_envelope_reason_codes(source),
    );

    snapshot.state = match source.verdict {
        OperatingEnvelopeVerdict::Admit => snapshot.state,
        OperatingEnvelopeVerdict::Deny | OperatingEnvelopeVerdict::Shed => {
            MissionObjectiveSourceState::Degraded
        }
        OperatingEnvelopeVerdict::Unknown => MissionObjectiveSourceState::Degraded,
    };

    snapshot
}

fn source_snapshot_from_evidence(
    source_id: String,
    kind: MissionObjectiveSourceKind,
    evidence: &SourceEvidence,
    category: MissionObjectiveEvidenceCategory,
    summary: String,
    reason_codes: Vec<String>,
) -> MissionObjectiveSourceSnapshot {
    let reason_codes = sorted_reason_codes(reason_codes);
    let mut evidence_item = MissionObjectiveEvidenceItem::new(category, summary);
    evidence_item.reason_codes.clone_from(&reason_codes);

    let mut snapshot = MissionObjectiveSourceSnapshot::new(source_id, kind);
    snapshot.state = source_state(evidence.status);
    snapshot.freshness_state = freshness_state(evidence.freshness_state);
    snapshot.redaction_posture = redaction_posture(evidence);
    snapshot.reason_codes = reason_codes;
    snapshot.evidence.push(evidence_item);
    snapshot
}

fn source_state(status: SourceStatus) -> MissionObjectiveSourceState {
    match status {
        SourceStatus::Available => MissionObjectiveSourceState::Available,
        SourceStatus::Degraded => MissionObjectiveSourceState::Degraded,
        SourceStatus::Unavailable => MissionObjectiveSourceState::Unavailable,
        SourceStatus::Blocked => MissionObjectiveSourceState::Blocked,
    }
}

fn freshness_state(state: FreshnessState) -> MissionObjectiveFreshnessState {
    match state {
        FreshnessState::Fresh => MissionObjectiveFreshnessState::Fresh,
        FreshnessState::Stale => MissionObjectiveFreshnessState::Stale,
        FreshnessState::Unknown => MissionObjectiveFreshnessState::Unknown,
        FreshnessState::NotCollected => MissionObjectiveFreshnessState::NotCollected,
    }
}

fn redaction_posture(evidence: &SourceEvidence) -> MissionObjectiveRedactionPosture {
    if evidence.raw_pane_content_stored {
        MissionObjectiveRedactionPosture::RawPaneContent
    } else if evidence.redacted {
        MissionObjectiveRedactionPosture::RedactedSummaryOnly
    } else {
        MissionObjectiveRedactionPosture::Unredacted
    }
}

fn all_snapshot_reason_codes(snapshot: &MissionTwinSnapshotEnvelope) -> Vec<String> {
    let mut reason_codes = Vec::new();
    extend_reason_codes(
        &mut reason_codes,
        beads_reason_codes(&snapshot.sources.beads),
    );
    extend_reason_codes(&mut reason_codes, rch_reason_codes(&snapshot.sources.rch));
    extend_reason_codes(
        &mut reason_codes,
        agent_mail_reason_codes(&snapshot.sources.agent_mail),
    );
    extend_reason_codes(&mut reason_codes, git_reason_codes(&snapshot.sources.git));
    extend_reason_codes(
        &mut reason_codes,
        reservations_reason_codes(&snapshot.sources.reservations),
    );
    extend_reason_codes(
        &mut reason_codes,
        operating_envelope_reason_codes(&snapshot.sources.operating_envelope),
    );

    for rejected in &snapshot.validation.rejected_inputs {
        push_unique(&mut reason_codes, rejected.reason_code.clone());
    }

    sorted_reason_codes(reason_codes)
}

fn beads_reason_codes(source: &BeadsMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();

    if source.ready_count == 0 {
        push_unique(&mut reason_codes, "mission_twin.no_ready_work");
    }

    for blocker in &source.dependency_blockers {
        push_unique(&mut reason_codes, "beads.dependency_blocked");
        extend_reason_codes(&mut reason_codes, blocker.reason_codes.clone());
    }

    for owner in &source.owner_states {
        match owner.owner_state {
            OwnerState::Active => push_unique(&mut reason_codes, "beads.owner_active"),
            OwnerState::StaleCandidate => {
                push_unique(&mut reason_codes, "beads.stale_reopen_candidate");
            }
            OwnerState::Unknown => push_unique(&mut reason_codes, "beads.owner_unknown"),
        }
        extend_reason_codes(&mut reason_codes, owner.reason_codes.clone());
    }

    for owner in &source.stale_owner_candidates {
        push_unique(&mut reason_codes, "beads.stale_reopen_candidate");
        extend_reason_codes(&mut reason_codes, owner.reason_codes.clone());
    }

    sorted_reason_codes(reason_codes)
}

fn rch_reason_codes(source: &RchMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();
    extend_reason_codes(&mut reason_codes, source.admission_reasons.clone());

    match source.admission_state {
        RchAdmissionState::Ready => {}
        RchAdmissionState::NotReady => push_unique(&mut reason_codes, "rch.admission.not_ready"),
        RchAdmissionState::Unavailable => push_unique(&mut reason_codes, "rch.unavailable"),
    }

    if source.critical_pressure_count > 0 {
        push_unique(&mut reason_codes, "rch.critical_pressure");
    }
    if source.healthy_workers == 0 {
        push_unique(&mut reason_codes, "rch.no_workers_passed_health");
    }

    for lane in &source.blocked_proof_lanes {
        push_unique(&mut reason_codes, "rch.proof_substrate_blocked");
        extend_reason_codes(&mut reason_codes, lane.reason_codes.clone());
    }

    sorted_reason_codes(reason_codes)
}

fn agent_mail_reason_codes(source: &AgentMailMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();
    extend_reason_codes(&mut reason_codes, source.fallback_reason_codes.clone());

    match source.availability_state {
        AgentMailAvailabilityState::Healthy => {}
        AgentMailAvailabilityState::Red => push_unique(&mut reason_codes, "agent_mail.red"),
        AgentMailAvailabilityState::Fallback => {
            push_unique(&mut reason_codes, "agent_mail.fallback_required");
        }
        AgentMailAvailabilityState::Unknown => {
            push_unique(&mut reason_codes, "agent_mail.unknown");
        }
    }

    sorted_reason_codes(reason_codes)
}

fn git_reason_codes(source: &GitMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();

    if !source.dirty_paths.is_empty() {
        push_unique(&mut reason_codes, "git.dirty_paths_present");
    }
    if source
        .dirty_paths
        .iter()
        .any(|dirty_path| dirty_path.overlaps_owned_path)
        || !source.overlap_paths.is_empty()
    {
        push_unique(&mut reason_codes, "mission_twin.dirty_overlap");
        push_unique(&mut reason_codes, "dirty_overlap.owned_surface_blocked");
    }
    if source.deletion_paths_present {
        push_unique(&mut reason_codes, "git.deletion_paths_present");
    }

    sorted_reason_codes(reason_codes)
}

fn reservations_reason_codes(source: &ReservationsMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.evidence.reason_codes.clone();

    if source
        .active_reservations
        .iter()
        .any(|reservation| reservation.exclusive)
    {
        push_unique(&mut reason_codes, "reservations.exclusive_active");
    }

    sorted_reason_codes(reason_codes)
}

fn operating_envelope_reason_codes(source: &OperatingEnvelopeMissionTwinSnapshot) -> Vec<String> {
    let mut reason_codes = source.reason_codes.clone();
    extend_reason_codes(&mut reason_codes, source.evidence.reason_codes.clone());

    match source.verdict {
        OperatingEnvelopeVerdict::Admit => {}
        OperatingEnvelopeVerdict::Deny => {
            push_unique(&mut reason_codes, "operating_envelope.deny");
        }
        OperatingEnvelopeVerdict::Shed => {
            push_unique(&mut reason_codes, "operating_envelope.shed");
        }
        OperatingEnvelopeVerdict::Unknown => {
            push_unique(&mut reason_codes, "operating_envelope.unknown");
        }
    }

    sorted_reason_codes(reason_codes)
}

fn sorted_reason_codes(mut reason_codes: Vec<String>) -> Vec<String> {
    reason_codes.retain(|reason_code| !reason_code.trim().is_empty());
    reason_codes.sort();
    reason_codes.dedup();
    reason_codes
}

fn extend_reason_codes(target: &mut Vec<String>, reason_codes: Vec<String>) {
    for reason_code in reason_codes {
        push_unique(target, reason_code);
    }
}

fn extend_reasonless_paths(target: &mut Vec<String>, paths: &[String]) {
    for path in paths {
        push_unique(target, path.clone());
    }
}

fn retain_reason_codes_without_prefix(reason_codes: &mut Vec<String>, prefixes: &[&str]) {
    reason_codes.retain(|reason_code| {
        !prefixes
            .iter()
            .any(|prefix| reason_code.starts_with(prefix))
    });
}

fn retain_reason_codes_without_fragment(reason_codes: &mut Vec<String>, fragments: &[&str]) {
    reason_codes.retain(|reason_code| {
        let normalized = reason_code.to_ascii_lowercase();
        !fragments
            .iter()
            .any(|fragment| normalized.contains(&fragment.to_ascii_lowercase()))
    });
}

fn push_unique(target: &mut Vec<String>, value: impl Into<String>) {
    let value = value.into();
    if !value.trim().is_empty() && !target.contains(&value) {
        target.push(value);
    }
}

fn rch_admission_state_name(state: RchAdmissionState) -> &'static str {
    match state {
        RchAdmissionState::Ready => "ready",
        RchAdmissionState::NotReady => "not_ready",
        RchAdmissionState::Unavailable => "unavailable",
    }
}

fn agent_mail_availability_name(state: AgentMailAvailabilityState) -> &'static str {
    match state {
        AgentMailAvailabilityState::Healthy => "healthy",
        AgentMailAvailabilityState::Red => "red",
        AgentMailAvailabilityState::Fallback => "fallback",
        AgentMailAvailabilityState::Unknown => "unknown",
    }
}

fn operating_envelope_verdict_name(verdict: OperatingEnvelopeVerdict) -> &'static str {
    match verdict {
        OperatingEnvelopeVerdict::Admit => "admit",
        OperatingEnvelopeVerdict::Deny => "deny",
        OperatingEnvelopeVerdict::Shed => "shed",
        OperatingEnvelopeVerdict::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission_objective_plan::MissionObjectivePlanStatus;
    use crate::mission_twin_snapshot::{
        ActiveAgentSummary, AgentMailMissionTwinSnapshot, BeadOwnerState, BeadsMissionTwinSnapshot,
        BlockedProofLane, DependencyBlocker, DirtyPathSummary, EvidenceLevel,
        GitMissionTwinSnapshot, MissionTwinForbiddenAction, MissionTwinSources,
        MissionTwinValidationSummary, OperatingEnvelopeMissionTwinSnapshot, RejectedInputSummary,
        RemoteHead, ReservationSummary, ReservationsMissionTwinSnapshot, SourceEvidence,
        ValidationState,
    };

    #[test]
    fn replay_rejects_empty_snapshot_sets() {
        assert_eq!(
            build_mission_twin_replay_planner_input(&[]),
            Err(MissionTwinReplayError::EmptySnapshotSet)
        );
    }

    #[test]
    fn replay_input_order_is_deterministic_for_equivalent_snapshots() {
        let left = valid_snapshot("b-snapshot");
        let right = valid_snapshot("a-snapshot");

        let first =
            build_mission_twin_replay_surface_data(&[left.clone(), right.clone()], None, None)
                .expect("first replay succeeds");
        let second = build_mission_twin_replay_surface_data(&[right, left], None, None)
            .expect("second replay succeeds");

        let first_json = serde_json::to_string(&first).expect("first replay serializes");
        let second_json = serde_json::to_string(&second).expect("second replay serializes");

        assert_eq!(
            first_json, second_json,
            "snapshot ordering must not perturb replay output"
        );
    }

    #[test]
    fn replay_preserves_source_reason_codes_in_plan() {
        let mut snapshot = valid_snapshot("reason-preservation");
        snapshot.sources.rch.admission_state = RchAdmissionState::NotReady;
        snapshot.sources.rch.critical_pressure_count = 5;
        snapshot
            .sources
            .rch
            .blocked_proof_lanes
            .push(BlockedProofLane {
                bead_id: "ft-proof".to_string(),
                command_family: "cargo".to_string(),
                reason_codes: vec![
                    "rch.admission.not_ready".to_string(),
                    "rch.critical_pressure".to_string(),
                ],
            });
        snapshot.sources.agent_mail.availability_state = AgentMailAvailabilityState::Red;
        snapshot
            .sources
            .agent_mail
            .fallback_reason_codes
            .push("swarm_tick_fallback".to_string());
        snapshot
            .sources
            .beads
            .dependency_blockers
            .push(DependencyBlocker {
                blocked_bead_id: "ft-dependent".to_string(),
                blocking_bead_id: "ft-blocker".to_string(),
                blocker_status: "in_progress".to_string(),
                reason_codes: vec!["mission_twin.snapshot_contract_required".to_string()],
            });
        snapshot.sources.git.dirty_paths.push(DirtyPathSummary {
            path: "crates/frankenterm-core/src/mission_twin_snapshot.rs".to_string(),
            status: "M".to_string(),
            overlaps_owned_path: true,
        });
        snapshot
            .sources
            .git
            .overlap_paths
            .push("crates/frankenterm-core/src/mission_twin_snapshot.rs".to_string());
        snapshot
            .sources
            .operating_envelope
            .reason_codes
            .push("mission_twin.dirty_overlap".to_string());

        let plan = replay_mission_twin_snapshots(&[snapshot]).expect("replay succeeds");

        assert_eq!(plan.plan_status, MissionObjectivePlanStatus::DirtyOverlap);
        for reason_code in [
            "agent_mail.red",
            "swarm_tick_fallback",
            "rch.critical_pressure",
            "rch.proof_substrate_blocked",
            "mission_twin.snapshot_contract_required",
            "mission_twin.dirty_overlap",
            "dirty_overlap.owned_surface_blocked",
        ] {
            assert!(
                plan.reason_codes.iter().any(|code| code == reason_code),
                "missing replay reason code {reason_code}"
            );
        }
    }

    #[test]
    fn surface_report_current_plan_preserves_read_only_contract() {
        let snapshot = valid_snapshot("healthy");
        let request = surface_request(MissionTwinSurfaceAction::CurrentPlan)
            .with_snapshot_paths(vec!["fixtures/mission-twin/snapshot/valid/healthy.json"]);

        let report =
            build_mission_twin_surface_report(&[snapshot], &request).expect("surface builds");

        assert_eq!(report.contract_id, MISSION_TWIN_SURFACE_CONTRACT_ID);
        assert_eq!(report.source_bead, MISSION_TWIN_SURFACE_SOURCE_BEAD);
        assert_eq!(report.action, MissionTwinSurfaceAction::CurrentPlan);
        assert_eq!(
            report.action_contract.robot_command,
            "robot mission-twin current-plan"
        );
        assert_eq!(
            report.action_contract.mcp_tool_name,
            "ft.mission_twin.current_plan"
        );
        assert!(report.action_contract.read_only);
        assert!(report.action_contract.idempotent);
        assert!(!report.simulated);
        assert!(!report.side_effects_executed);
        assert!(!report.raw_pane_content_stored);
        assert!(
            report
                .artifact_paths
                .iter()
                .any(|path| path == "fixtures/mission-twin/snapshot/valid/healthy.json")
        );
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.surface.read_only".to_string())
        );
    }

    #[test]
    fn surface_report_explains_steps_and_reasons() {
        let snapshot = valid_snapshot("healthy");
        let step_request = surface_request(MissionTwinSurfaceAction::ExplainStep)
            .with_explain_step("mission-twin.healthy.ready-work");
        let step_report =
            build_mission_twin_surface_report(std::slice::from_ref(&snapshot), &step_request)
                .expect("step explain surface builds");

        let step_explain = step_report
            .plan_surface
            .explain
            .as_ref()
            .expect("step explanation present");
        assert_eq!(
            step_explain.mode,
            crate::mission_objective_plan::MissionObjectivePlanExplainMode::Step
        );
        assert!(step_explain.matched);
        assert!(
            step_report
                .reason_codes
                .contains(&"mission_twin.surface.explain.step".to_string())
        );

        let reason_request = surface_request(MissionTwinSurfaceAction::ExplainReason)
            .with_explain_reason("planner.status.actionable");
        let reason_report = build_mission_twin_surface_report(&[snapshot], &reason_request)
            .expect("reason explain surface builds");

        let reason_explain = reason_report
            .plan_surface
            .explain
            .as_ref()
            .expect("reason explanation present");
        assert_eq!(
            reason_explain.mode,
            crate::mission_objective_plan::MissionObjectivePlanExplainMode::Reason
        );
        assert!(reason_explain.matched);
        assert!(
            reason_report
                .reason_codes
                .contains(&"mission_twin.surface.explain.reason".to_string())
        );
    }

    #[test]
    fn surface_report_simulate_embeds_counterfactual_and_ownership_reports() {
        let snapshot = valid_snapshot("surface-simulate");
        let request = surface_request(MissionTwinSurfaceAction::Simulate)
            .with_counterfactual_requests(vec![counterfactual_request(
                "surface-mail-recovered",
                vec![MissionTwinCounterfactualToggle::AgentMailRecovered],
                None,
            )])
            .with_ownership_request(ownership_request(
                "mission-twin.surface-simulate.ready-work",
                None,
                vec!["docs/surface.md"],
                DEFAULT_STALE_AFTER_SECONDS,
                false,
            ));

        let report =
            build_mission_twin_surface_report(&[snapshot], &request).expect("surface builds");

        assert_eq!(report.action, MissionTwinSurfaceAction::Simulate);
        assert!(report.simulated);
        assert!(report.counterfactual_report.is_some());
        assert!(report.ownership_report.is_some());
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.surface.action.simulate".to_string())
        );
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.counterfactual.agent_mail_recovered".to_string())
        );
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.ownership.side_effect_free".to_string())
        );
    }

    #[test]
    fn surface_report_rejects_empty_explain_queries() {
        let snapshot = valid_snapshot("invalid-surface");
        let request = surface_request(MissionTwinSurfaceAction::ExplainStep);

        let err = build_mission_twin_surface_report(&[snapshot], &request)
            .expect_err("missing explain_step must fail");

        assert!(matches!(
            err,
            MissionTwinReplayError::InvalidSurfaceRequest { .. }
        ));
    }

    #[test]
    fn ownership_overlap_classifier_handles_exact_parent_child_glob_like_and_empty() {
        assert_eq!(
            classify_mission_twin_owned_path_overlap(
                "crates/frankenterm-core/src/mission_twin_replay.rs",
                "crates/frankenterm-core/src/mission_twin_replay.rs",
            ),
            Some(MissionTwinOwnershipOverlapKind::Exact)
        );
        assert_eq!(
            classify_mission_twin_owned_path_overlap(
                "crates/frankenterm-core",
                "crates/frankenterm-core/src/mission_twin_replay.rs",
            ),
            Some(MissionTwinOwnershipOverlapKind::ParentChild)
        );
        assert_eq!(
            classify_mission_twin_owned_path_overlap(
                "crates/frankenterm-core/*",
                "crates/frankenterm-core/src/mission_twin_replay.rs",
            ),
            Some(MissionTwinOwnershipOverlapKind::GlobLike)
        );
        assert_eq!(
            classify_mission_twin_owned_path_overlap(
                "",
                "crates/frankenterm-core/src/mission_twin_replay.rs",
            ),
            None
        );
        assert_eq!(
            classify_mission_twin_owned_path_overlap(
                "crates/frankenterm-core/src/mission_twin_replay.rs",
                "",
            ),
            None
        );
    }

    #[test]
    fn ownership_simulator_marks_dirty_overlap_as_unsafe_without_side_effects() {
        let mut snapshot = valid_snapshot("dirty-ownership");
        snapshot.sources.git.dirty_paths.push(DirtyPathSummary {
            path: "crates/frankenterm-core/src/mission_twin_replay.rs".to_string(),
            status: "M".to_string(),
            overlaps_owned_path: true,
        });
        snapshot
            .sources
            .reservations
            .active_reservations
            .push(ReservationSummary {
                holder: "PinkTrout".to_string(),
                path_pattern: "crates/frankenterm-core/src/*".to_string(),
                exclusive: true,
                reason: "mission twin snapshot contract".to_string(),
            });

        let report = simulate_mission_twin_ownership_handoff(
            &[snapshot],
            &ownership_request(
                "mission-twin.dirty-ownership.ready-work",
                Some("ft-0kvfc"),
                vec!["crates/frankenterm-core/src/mission_twin_replay.rs"],
                DEFAULT_STALE_AFTER_SECONDS,
                false,
            ),
        )
        .expect("ownership simulation succeeds");

        assert_eq!(
            report.handoff_state,
            MissionTwinOwnershipHandoffState::UnsafeOverlap
        );
        assert_eq!(report.dirty_overlaps.len(), 1);
        assert_eq!(report.reservation_overlaps.len(), 1);
        assert!(report.simulated);
        assert!(!report.side_effects_executed);
        assert!(!report.raw_pane_content_stored);
        assert!(
            report
                .next_actions
                .contains(&MissionTwinOwnershipNextAction::AskOwner)
        );
        assert!(
            report
                .next_actions
                .contains(&MissionTwinOwnershipNextAction::RunStaticOnlyVerifier)
        );
    }

    #[test]
    fn ownership_simulator_marks_active_owner_reservation_as_handoff_required() {
        let mut snapshot = valid_snapshot("active-owner-handoff");
        snapshot.sources.beads.owner_states.push(BeadOwnerState {
            bead_id: "ft-u7r37.7".to_string(),
            assignee: "SilverTrout".to_string(),
            owner_state: OwnerState::Active,
            age_seconds: 48,
            last_activity_source: "agent_mail".to_string(),
            reason_codes: vec!["recent_closeout".to_string()],
        });
        snapshot
            .sources
            .reservations
            .active_reservations
            .push(ReservationSummary {
                holder: "SilverTrout".to_string(),
                path_pattern: "docs/mission-twin-privacy-safety.md".to_string(),
                exclusive: true,
                reason: "safety policy closeout".to_string(),
            });

        let report = simulate_mission_twin_ownership_handoff(
            &[snapshot],
            &ownership_request(
                "mission-twin.active-owner.owner.ft-u7r37.7",
                Some("ft-u7r37.7"),
                vec!["docs/mission-twin-privacy-safety.md"],
                DEFAULT_STALE_AFTER_SECONDS,
                false,
            ),
        )
        .expect("ownership simulation succeeds");

        assert_eq!(
            report.handoff_state,
            MissionTwinOwnershipHandoffState::HandoffRequired
        );
        assert_eq!(report.owner_summaries.len(), 1);
        assert_eq!(report.reservation_overlaps.len(), 1);
        assert!(
            report
                .next_actions
                .contains(&MissionTwinOwnershipNextAction::AskOwner)
        );
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.ownership.active_owner_handoff".to_string())
        );
    }

    #[test]
    fn ownership_simulator_prefers_stale_check_after_threshold() {
        let mut snapshot = valid_snapshot("stale-owner-check");
        snapshot.sources.beads.owner_states.push(BeadOwnerState {
            bead_id: "ft-stale".to_string(),
            assignee: "GreyRiver".to_string(),
            owner_state: OwnerState::Active,
            age_seconds: DEFAULT_STALE_AFTER_SECONDS,
            last_activity_source: "beads".to_string(),
            reason_codes: vec!["owner_age_at_threshold".to_string()],
        });

        let report = simulate_mission_twin_ownership_handoff(
            &[snapshot],
            &ownership_request(
                "mission-twin.stale-owner.owner.ft-stale",
                Some("ft-stale"),
                vec!["docs/stale-owner.md"],
                DEFAULT_STALE_AFTER_SECONDS,
                false,
            ),
        )
        .expect("ownership simulation succeeds");

        assert_eq!(
            report.handoff_state,
            MissionTwinOwnershipHandoffState::StaleCheckNeeded
        );
        assert!(
            report
                .next_actions
                .contains(&MissionTwinOwnershipNextAction::Comment)
        );
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.ownership.stale_check_needed".to_string())
        );
    }

    #[test]
    fn ownership_simulator_keeps_empty_owned_paths_safe_and_non_matching() {
        let mut snapshot = valid_snapshot("empty-owned-paths");
        snapshot.sources.git.dirty_paths.push(DirtyPathSummary {
            path: "crates/frankenterm-core/src/mission_twin_replay.rs".to_string(),
            status: "M".to_string(),
            overlaps_owned_path: true,
        });

        let report = simulate_mission_twin_ownership_handoff(
            &[snapshot],
            &ownership_request(
                "mission-twin.empty-owned-paths.ready-work",
                None,
                Vec::<&str>::new(),
                DEFAULT_STALE_AFTER_SECONDS,
                false,
            ),
        )
        .expect("ownership simulation succeeds");

        assert_eq!(
            report.handoff_state,
            MissionTwinOwnershipHandoffState::SafeToOpen
        );
        assert!(report.dirty_overlaps.is_empty());
        assert!(
            report
                .reason_codes
                .contains(&"mission_twin.ownership.empty_owned_paths".to_string())
        );
    }

    #[test]
    fn counterfactual_rch_recovered_unblocks_waiting_rch() {
        let mut snapshot = valid_snapshot("rch-recovered");
        snapshot.sources.rch.admission_state = RchAdmissionState::NotReady;
        snapshot.sources.rch.critical_pressure_count = 5;
        snapshot.sources.rch.evidence.status = SourceStatus::Blocked;
        snapshot
            .sources
            .rch
            .blocked_proof_lanes
            .push(BlockedProofLane {
                bead_id: "ft-proof".to_string(),
                command_family: "cargo".to_string(),
                reason_codes: vec!["rch.critical_pressure".to_string()],
            });
        snapshot.sources.operating_envelope.verdict = OperatingEnvelopeVerdict::Shed;
        snapshot
            .sources
            .operating_envelope
            .reason_codes
            .push("rch.critical_pressure".to_string());

        let report = simulate_mission_twin_counterfactuals(
            &[snapshot],
            &[counterfactual_request(
                "rch-recovered",
                vec![
                    MissionTwinCounterfactualToggle::RchRecovered,
                    MissionTwinCounterfactualToggle::ProofLanesBudgeted,
                ],
                Some(MissionTwinProofLaneBudget {
                    remote_cargo_lanes: 2,
                    static_verifier_lanes: 1,
                }),
            )],
        )
        .expect("counterfactual simulation succeeds");

        assert_eq!(
            report.live_plan.plan_status,
            MissionObjectivePlanStatus::RchSubstrateBlocked
        );
        let simulated = &report.counterfactual_plans[0];
        assert!(simulated.simulated);
        assert_eq!(
            simulated.plan_status,
            MissionObjectivePlanStatus::Actionable
        );
        assert!(
            simulated
                .live_execution_blocked_by
                .iter()
                .any(|blocker| blocker == "rch.recovery_required")
        );
        assert!(
            simulated
                .unblocked_reason_codes
                .iter()
                .any(|reason| reason == "mission_twin.counterfactual.rch_recovered")
        );
    }

    #[test]
    fn counterfactual_agent_mail_recovered_removes_degraded_mail_blocker() {
        let mut snapshot = valid_snapshot("mail-recovered");
        snapshot.sources.agent_mail.availability_state = AgentMailAvailabilityState::Red;
        snapshot.sources.agent_mail.evidence.status = SourceStatus::Unavailable;
        snapshot.sources.agent_mail.evidence.freshness_state = FreshnessState::NotCollected;
        snapshot.sources.agent_mail.evidence.collected_at_ms = None;
        snapshot.sources.agent_mail.evidence.freshness_ms = None;
        snapshot.sources.agent_mail.evidence.evidence_level = EvidenceLevel::NotCollected;
        snapshot
            .sources
            .agent_mail
            .fallback_reason_codes
            .push("swarm_tick_fallback".to_string());

        let report = simulate_mission_twin_counterfactuals(
            &[snapshot],
            &[counterfactual_request(
                "mail-recovered",
                vec![MissionTwinCounterfactualToggle::AgentMailRecovered],
                None,
            )],
        )
        .expect("counterfactual simulation succeeds");

        assert_eq!(
            report.live_plan.plan_status,
            MissionObjectivePlanStatus::Degraded
        );
        assert_eq!(
            report.counterfactual_plans[0].plan_status,
            MissionObjectivePlanStatus::Actionable
        );
        assert!(
            report.counterfactual_plans[0]
                .live_execution_blocked_by
                .iter()
                .any(|blocker| blocker == "agent_mail.recovery_required")
        );
    }

    #[test]
    fn counterfactual_dirty_overlap_cleared_makes_ready_candidate_actionable() {
        let mut snapshot = valid_snapshot("dirty-cleared");
        snapshot.sources.git.evidence.status = SourceStatus::Degraded;
        snapshot
            .sources
            .git
            .evidence
            .reason_codes
            .push("mission_twin.dirty_overlap".to_string());
        snapshot.sources.git.dirty_paths.push(DirtyPathSummary {
            path: "crates/frankenterm-core/src/mission_twin_replay.rs".to_string(),
            status: "M".to_string(),
            overlaps_owned_path: true,
        });
        snapshot
            .sources
            .git
            .overlap_paths
            .push("crates/frankenterm-core/src/mission_twin_replay.rs".to_string());
        snapshot.sources.operating_envelope.evidence.status = SourceStatus::Degraded;
        snapshot
            .sources
            .operating_envelope
            .reason_codes
            .push("mission_twin.dirty_overlap".to_string());

        let report = simulate_mission_twin_counterfactuals(
            &[snapshot],
            &[counterfactual_request(
                "dirty-cleared",
                vec![MissionTwinCounterfactualToggle::DirtyOverlapCleared],
                None,
            )],
        )
        .expect("counterfactual simulation succeeds");

        assert_eq!(
            report.live_plan.plan_status,
            MissionObjectivePlanStatus::DirtyOverlap
        );
        assert_eq!(
            report.counterfactual_plans[0].plan_status,
            MissionObjectivePlanStatus::Actionable
        );
        assert!(
            report.counterfactual_plans[0]
                .live_execution_blocked_by
                .iter()
                .any(|blocker| blocker == "dirty_overlap.clear_required")
        );
    }

    #[test]
    fn counterfactual_owner_handoff_accepted_clears_waiting_owner() {
        let mut snapshot = valid_snapshot("owner-handoff");
        snapshot.sources.beads.owner_states.push(BeadOwnerState {
            bead_id: "ft-u7r37.7".to_string(),
            assignee: "SilverTrout".to_string(),
            owner_state: OwnerState::Active,
            age_seconds: 48,
            last_activity_source: "agent_mail".to_string(),
            reason_codes: vec!["recent_closeout".to_string()],
        });
        snapshot
            .sources
            .reservations
            .active_reservations
            .push(ReservationSummary {
                holder: "SilverTrout".to_string(),
                path_pattern: "docs/mission-twin-privacy-safety.md".to_string(),
                exclusive: true,
                reason: "safety policy closeout".to_string(),
            });

        let report = simulate_mission_twin_counterfactuals(
            &[snapshot],
            &[counterfactual_request(
                "owner-handoff",
                vec![MissionTwinCounterfactualToggle::OwnerHandoffAccepted],
                None,
            )],
        )
        .expect("counterfactual simulation succeeds");

        assert_eq!(
            report.live_plan.plan_status,
            MissionObjectivePlanStatus::WaitingOwner
        );
        assert_eq!(
            report.counterfactual_plans[0].plan_status,
            MissionObjectivePlanStatus::Actionable
        );
        assert!(
            report.counterfactual_plans[0]
                .live_execution_blocked_by
                .iter()
                .any(|blocker| blocker == "owner_handoff.required")
        );
    }

    #[test]
    fn counterfactual_target_class_proof_available_clears_waiting_external() {
        let mut snapshot = valid_snapshot("target-proof");
        snapshot.sources.operating_envelope.verdict = OperatingEnvelopeVerdict::Shed;
        snapshot
            .sources
            .operating_envelope
            .reason_codes
            .push("target_class.skipped_not_proven".to_string());

        let live = replay_mission_twin_snapshots(&[snapshot.clone()]).expect("live replay");
        assert_eq!(
            live.plan_status,
            MissionObjectivePlanStatus::WaitingExternal
        );
        assert_eq!(
            live.plan_steps[0].proof_lane,
            MissionObjectiveProofLane::RchCargo
        );

        let report = simulate_mission_twin_counterfactuals(
            &[snapshot],
            &[counterfactual_request(
                "target-proof",
                vec![MissionTwinCounterfactualToggle::TargetClassProofAvailable],
                None,
            )],
        )
        .expect("counterfactual simulation succeeds");

        assert_eq!(
            report.counterfactual_plans[0].plan_status,
            MissionObjectivePlanStatus::Actionable
        );
        assert_eq!(
            report.counterfactual_plans[0].proof_lane_broker.decisions[0].lane_class,
            MissionTwinProofLaneClass::NotRequired
        );
    }

    #[test]
    fn counterfactual_validation_rejects_forbidden_budget_combinations() {
        let snapshot = valid_snapshot("invalid-counterfactual");

        let without_budget = simulate_mission_twin_counterfactuals(
            std::slice::from_ref(&snapshot),
            &[counterfactual_request(
                "missing-budget",
                vec![MissionTwinCounterfactualToggle::ProofLanesBudgeted],
                None,
            )],
        );
        assert!(matches!(
            without_budget,
            Err(MissionTwinReplayError::InvalidCounterfactual { .. })
        ));

        let budget_without_toggle = simulate_mission_twin_counterfactuals(
            std::slice::from_ref(&snapshot),
            &[counterfactual_request(
                "budget-without-toggle",
                vec![MissionTwinCounterfactualToggle::RchRecovered],
                Some(MissionTwinProofLaneBudget {
                    remote_cargo_lanes: 1,
                    static_verifier_lanes: 0,
                }),
            )],
        );
        assert!(matches!(
            budget_without_toggle,
            Err(MissionTwinReplayError::InvalidCounterfactual { .. })
        ));

        let zero_budget = simulate_mission_twin_counterfactuals(
            std::slice::from_ref(&snapshot),
            &[counterfactual_request(
                "zero-budget",
                vec![MissionTwinCounterfactualToggle::ProofLanesBudgeted],
                Some(MissionTwinProofLaneBudget {
                    remote_cargo_lanes: 0,
                    static_verifier_lanes: 0,
                }),
            )],
        );
        assert!(matches!(
            zero_budget,
            Err(MissionTwinReplayError::InvalidCounterfactual { .. })
        ));
    }

    #[test]
    fn proof_lane_broker_classifies_waiting_rch_static_coordination_and_not_required() {
        let healthy =
            build_mission_twin_replay_surface_data(&[valid_snapshot("healthy")], None, None)
                .expect("healthy surface");
        assert_eq!(
            classify_mission_twin_proof_lanes(&healthy).decisions[0].lane_class,
            MissionTwinProofLaneClass::NotRequired
        );

        let no_ready = {
            let mut snapshot = valid_snapshot("no-ready");
            snapshot.sources.beads.ready_count = 0;
            build_mission_twin_replay_surface_data(&[snapshot], None, None)
                .expect("no-ready surface")
        };
        let no_ready_classes = classify_mission_twin_proof_lanes(&no_ready)
            .decisions
            .into_iter()
            .map(|decision| decision.lane_class)
            .collect::<Vec<_>>();
        assert!(no_ready_classes.contains(&MissionTwinProofLaneClass::CoordinationOnly));
        assert!(no_ready_classes.contains(&MissionTwinProofLaneClass::StaticVerifier));

        let waiting_rch = {
            let mut snapshot = valid_snapshot("waiting-rch");
            snapshot.sources.rch.admission_state = RchAdmissionState::NotReady;
            snapshot.sources.rch.critical_pressure_count = 5;
            build_mission_twin_replay_surface_data(&[snapshot], None, None)
                .expect("waiting-rch surface")
        };
        assert_eq!(
            classify_mission_twin_proof_lanes(&waiting_rch).decisions[0].lane_class,
            MissionTwinProofLaneClass::WaitingRch
        );
    }

    fn counterfactual_request(
        scenario_id: &str,
        toggles: Vec<MissionTwinCounterfactualToggle>,
        proof_lane_budget: Option<MissionTwinProofLaneBudget>,
    ) -> MissionTwinCounterfactualRequest {
        MissionTwinCounterfactualRequest {
            scenario_id: scenario_id.to_string(),
            toggles,
            proof_lane_budget,
        }
    }

    fn ownership_request(
        candidate_id: &str,
        target_bead_id: Option<&str>,
        owned_paths: Vec<&str>,
        stale_after_seconds: u64,
        fallback_only_coordination: bool,
    ) -> MissionTwinOwnershipSimulationRequest {
        MissionTwinOwnershipSimulationRequest {
            candidate_id: candidate_id.to_string(),
            target_bead_id: target_bead_id.map(str::to_string),
            owned_paths: owned_paths.into_iter().map(str::to_string).collect(),
            stale_after_seconds,
            fallback_only_coordination,
        }
    }

    fn surface_request(action: MissionTwinSurfaceAction) -> MissionTwinSurfaceRequest {
        MissionTwinSurfaceRequest {
            action,
            snapshot_paths: Vec::new(),
            explain_step: None,
            explain_reason: None,
            counterfactual_requests: Vec::new(),
            ownership_request: None,
        }
    }

    trait SurfaceRequestExt {
        fn with_snapshot_paths(self, paths: Vec<&str>) -> Self;
        fn with_explain_step(self, query: &str) -> Self;
        fn with_explain_reason(self, query: &str) -> Self;
        fn with_counterfactual_requests(
            self,
            requests: Vec<MissionTwinCounterfactualRequest>,
        ) -> Self;
        fn with_ownership_request(self, request: MissionTwinOwnershipSimulationRequest) -> Self;
    }

    impl SurfaceRequestExt for MissionTwinSurfaceRequest {
        fn with_snapshot_paths(mut self, paths: Vec<&str>) -> Self {
            self.snapshot_paths = paths.into_iter().map(str::to_string).collect();
            self
        }

        fn with_explain_step(mut self, query: &str) -> Self {
            self.explain_step = Some(query.to_string());
            self
        }

        fn with_explain_reason(mut self, query: &str) -> Self {
            self.explain_reason = Some(query.to_string());
            self
        }

        fn with_counterfactual_requests(
            mut self,
            requests: Vec<MissionTwinCounterfactualRequest>,
        ) -> Self {
            self.counterfactual_requests = requests;
            self
        }

        fn with_ownership_request(
            mut self,
            request: MissionTwinOwnershipSimulationRequest,
        ) -> Self {
            self.ownership_request = Some(request);
            self
        }
    }

    fn valid_snapshot(snapshot_id: &str) -> MissionTwinSnapshotEnvelope {
        MissionTwinSnapshotEnvelope {
            schema_version: crate::mission_twin_snapshot::MISSION_TWIN_SNAPSHOT_SCHEMA_VERSION,
            contract_id: crate::mission_twin_snapshot::MISSION_TWIN_SNAPSHOT_CONTRACT_ID
                .to_string(),
            source_bead: crate::mission_twin_snapshot::MISSION_TWIN_SNAPSHOT_SOURCE_BEAD
                .to_string(),
            snapshot_id: snapshot_id.to_string(),
            generated_at_ms: 1_771_376_400_000,
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
                    active_agents: Vec::<ActiveAgentSummary>::new(),
                    fallback_reason_codes: Vec::new(),
                },
                git: GitMissionTwinSnapshot {
                    evidence: evidence("git"),
                    branch: "main".to_string(),
                    head: "0123456789abcdef0123456789abcdef01234567".to_string(),
                    remote_heads: Vec::<RemoteHead>::new(),
                    dirty_paths: Vec::new(),
                    overlap_paths: Vec::new(),
                    deletion_paths_present: false,
                },
                reservations: ReservationsMissionTwinSnapshot {
                    evidence: evidence("reservations"),
                    active_reservations: Vec::<ReservationSummary>::new(),
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
                rejected_inputs: Vec::<RejectedInputSummary>::new(),
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
            collected_at_ms: Some(1_771_376_400_000),
            freshness_ms: Some(1),
            evidence_level: EvidenceLevel::Fixture,
            redacted: true,
            raw_pane_content_stored: false,
            reason_codes: Vec::new(),
            artifact_paths: Vec::new(),
        }
    }
}
