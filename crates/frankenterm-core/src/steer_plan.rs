//! ft-7h5da.6.2: read-only `ft steer plan` pipeline.
//!
//! Runs the real mission-objective planner over a deterministic per-scenario
//! fixture, scores the result through the live rehearsal scorer, evaluates the
//! future execution preflight through [`PolicyEngine::authorize_preview`], and
//! emits a [`SteeringReceipt`] (W5.1 type). The pipeline is side-effect-free:
//! the planner is `dry_run` by construction, policy preview consumes no rate
//! budget, and the only durable writes a caller performs are the receipt itself
//! + one audit row.
//!
//! Scenario fixtures map to the mission-objective status taxonomy so each one
//! yields a deterministic, golden-comparable receipt:
//! clean-ready → Actionable, dirty-overlap → DirtyOverlap, rch-blocked →
//! RchSubstrateBlocked, approval-required → WaitingOwner, capacity-red →
//! WaitingExternal.
//!
use crate::mission_objective_plan::{
    MissionObjectiveCandidateReadiness, MissionObjectiveCandidateWork,
    MissionObjectiveCapacityPosture, MissionObjectiveDirtyPath, MissionObjectivePlanStatus,
    MissionObjectivePlannerInput, MissionObjectiveProofAvailability, MissionObjectiveSourceKind,
    MissionObjectiveSourceSnapshot, plan_mission_objective,
};
use crate::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyEngine, PolicyInput,
};
use crate::rehearsal_score::{
    RehearsalCriterionKind, RehearsalCriterionReceipt, RehearsalEvidenceRef,
    RehearsalEvidenceState, RehearsalScoreReceipt, RehearsalScoringEngine, RehearsalVerdict,
};
use crate::steering::SteeringReceipt;

/// Standard read-only steer-plan scenarios.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerPlanScenario {
    CleanReady,
    DirtyOverlap,
    RchBlocked,
    ApprovalRequired,
    CapacityRed,
}

impl SteerPlanScenario {
    /// All scenarios, in canonical order.
    pub const ALL: [SteerPlanScenario; 5] = [
        Self::CleanReady,
        Self::DirtyOverlap,
        Self::RchBlocked,
        Self::ApprovalRequired,
        Self::CapacityRed,
    ];

    /// Parse a scenario name (the `--scenario` value).
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "clean-ready" => Ok(Self::CleanReady),
            "dirty-overlap" => Ok(Self::DirtyOverlap),
            "rch-blocked" => Ok(Self::RchBlocked),
            "approval-required" => Ok(Self::ApprovalRequired),
            "capacity-red" => Ok(Self::CapacityRed),
            other => Err(format!(
                "unknown scenario `{other}`; expected one of: clean-ready, \
                 dirty-overlap, rch-blocked, approval-required, capacity-red"
            )),
        }
    }

    /// Canonical scenario name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CleanReady => "clean-ready",
            Self::DirtyOverlap => "dirty-overlap",
            Self::RchBlocked => "rch-blocked",
            Self::ApprovalRequired => "approval-required",
            Self::CapacityRed => "capacity-red",
        }
    }
}

/// Stable lowercase label for a plan status (used in audit + diagnostics).
#[must_use]
pub fn plan_status_label(status: MissionObjectivePlanStatus) -> &'static str {
    use MissionObjectivePlanStatus::{
        Actionable, Blocked, Degraded, DirtyOverlap, NoReadyWork, PlanningOnly,
        RchSubstrateBlocked, Unavailable, WaitingExternal, WaitingOwner,
    };
    match status {
        Actionable => "actionable",
        PlanningOnly => "planning_only",
        Blocked => "blocked",
        WaitingOwner => "waiting_owner",
        WaitingExternal => "waiting_external",
        DirtyOverlap => "dirty_overlap",
        NoReadyWork => "no_ready_work",
        RchSubstrateBlocked => "rch_substrate_blocked",
        Degraded => "degraded",
        Unavailable => "unavailable",
    }
}

/// Build the deterministic planner input fixture for a scenario. A healthy
/// Beads snapshot keeps `source_posture` clean so the candidate's readiness
/// drives the plan status.
fn scenario_input(
    scenario: SteerPlanScenario,
    generated_at_ms: u64,
    objective: &str,
) -> MissionObjectivePlannerInput {
    let snapshot = MissionObjectiveSourceSnapshot::new("beads", MissionObjectiveSourceKind::Beads);
    let input = MissionObjectivePlannerInput::new(generated_at_ms, "ft.steer.plan", objective)
        .with_source_snapshot(snapshot);
    let candidate = MissionObjectiveCandidateWork::new(
        "steer.candidate",
        MissionObjectiveCandidateReadiness::ReadyBead,
    );
    match scenario {
        SteerPlanScenario::CleanReady => input.with_candidate(candidate),
        SteerPlanScenario::DirtyOverlap => input
            .with_dirty_path(MissionObjectiveDirtyPath::new(
                "crates/frankenterm-core/src/lib.rs",
                "modified",
            ))
            .with_candidate(candidate.with_owned_path("crates/frankenterm-core/src/lib.rs")),
        SteerPlanScenario::RchBlocked => input.with_candidate(
            candidate.proof_availability(MissionObjectiveProofAvailability::Blocked),
        ),
        SteerPlanScenario::ApprovalRequired => {
            input.with_candidate(MissionObjectiveCandidateWork::new(
                "steer.candidate",
                MissionObjectiveCandidateReadiness::ActiveSameDomain,
            ))
        }
        SteerPlanScenario::CapacityRed => {
            input.with_candidate(candidate.capacity_posture(MissionObjectiveCapacityPosture::Pause))
        }
    }
}

/// Base envelope verdict implied by the mission-objective status.
fn status_envelope_verdict(status: MissionObjectivePlanStatus) -> &'static str {
    use MissionObjectivePlanStatus::{
        Actionable, Blocked, Degraded, DirtyOverlap, NoReadyWork, PlanningOnly,
        RchSubstrateBlocked, Unavailable, WaitingExternal, WaitingOwner,
    };
    match status {
        Actionable => "envelope.admit",
        WaitingOwner | DirtyOverlap => "envelope.requires_approval",
        RchSubstrateBlocked => "envelope.blocked.rch_substrate",
        WaitingExternal => "envelope.blocked.capacity",
        Degraded => "envelope.blocked.degraded",
        Blocked | NoReadyWork | PlanningOnly | Unavailable => "envelope.blocked",
    }
}

fn status_required_approvals(status: MissionObjectivePlanStatus) -> Vec<String> {
    match status {
        MissionObjectivePlanStatus::WaitingOwner => vec!["approval:owner_handoff".to_string()],
        MissionObjectivePlanStatus::DirtyOverlap => vec!["approval:dirty_overlap".to_string()],
        _ => Vec::new(),
    }
}

fn policy_required_approvals(decision: &PolicyDecision) -> Vec<String> {
    if !decision.requires_approval() {
        return Vec::new();
    }
    let rule = decision.rule_id().unwrap_or("policy.approval_required");
    vec![format!("approval:{rule}")]
}

fn combined_required_approvals(
    status: MissionObjectivePlanStatus,
    decision: &PolicyDecision,
) -> Vec<String> {
    let mut approvals = status_required_approvals(status);
    for approval in policy_required_approvals(decision) {
        if !approvals.contains(&approval) {
            approvals.push(approval);
        }
    }
    approvals
}

fn combined_envelope_verdict(
    status: MissionObjectivePlanStatus,
    decision: &PolicyDecision,
) -> &'static str {
    let status_verdict = status_envelope_verdict(status);
    if status_verdict.starts_with("envelope.blocked") {
        return status_verdict;
    }
    match decision {
        PolicyDecision::Allow { .. } => status_verdict,
        PolicyDecision::RequireApproval { .. } => "envelope.requires_approval",
        PolicyDecision::Deny { .. } => "envelope.blocked.policy",
    }
}

/// Run the live policy preflight for the future steer execution step.
///
/// The input is intentionally synthetic and harmless: the operator objective is
/// audit context, not command text, so destructive words in an objective cannot
/// trip command-gate policy as though they were executable shell bytes.
fn policy_preflight_decision(scenario: SteerPlanScenario, objective: &str) -> PolicyDecision {
    let capabilities = if scenario == SteerPlanScenario::ApprovalRequired {
        PaneCapabilities::unknown()
    } else {
        PaneCapabilities::prompt()
    };
    let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
        .with_pane(0)
        .with_capabilities(capabilities)
        .with_text_summary(format!("steer plan objective: {objective}"))
        .with_command_text("ft steer plan preflight");
    PolicyEngine::strict().authorize_preview(&input)
}

fn criterion(
    id: &str,
    kind: RehearsalCriterionKind,
    verdict: RehearsalVerdict,
    evidence_state: RehearsalEvidenceState,
    note: &str,
) -> RehearsalCriterionReceipt {
    RehearsalCriterionReceipt::new(format!("steer_plan.{id}"), kind, verdict)
        .with_evidence(RehearsalEvidenceRef::new(
            "ft.steer.plan",
            format!("scenario.{id}"),
            evidence_state,
        ))
        .with_note(note)
}

fn policy_criterion(decision: &PolicyDecision) -> RehearsalCriterionReceipt {
    match decision {
        PolicyDecision::Allow { rule_id, .. } => criterion(
            "policy_preflight",
            RehearsalCriterionKind::SafetyPolicy,
            RehearsalVerdict::Pass,
            RehearsalEvidenceState::FixtureOnly,
            rule_id
                .as_deref()
                .unwrap_or("authorize_preview allowed preflight"),
        ),
        PolicyDecision::RequireApproval {
            reason, rule_id, ..
        } => criterion(
            "policy_preflight",
            RehearsalCriterionKind::SafetyPolicy,
            RehearsalVerdict::Degraded,
            RehearsalEvidenceState::Degraded,
            &format!(
                "{}: {reason}",
                rule_id.as_deref().unwrap_or("policy.require_approval")
            ),
        ),
        PolicyDecision::Deny {
            reason, rule_id, ..
        } => criterion(
            "policy_preflight",
            RehearsalCriterionKind::SafetyPolicy,
            RehearsalVerdict::Fail,
            RehearsalEvidenceState::Blocked,
            &format!("{}: {reason}", rule_id.as_deref().unwrap_or("policy.deny")),
        ),
    }
}

fn rehearsal_criteria(
    status: MissionObjectivePlanStatus,
    decision: &PolicyDecision,
) -> Vec<RehearsalCriterionReceipt> {
    use MissionObjectivePlanStatus::{
        Actionable, DirtyOverlap, RchSubstrateBlocked, WaitingExternal, WaitingOwner,
    };
    let mut criteria = vec![policy_criterion(decision)];
    match status {
        Actionable => {
            criteria.push(criterion(
                "mission_status",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "mission objective planner returned actionable",
            ));
            criteria.push(criterion(
                "rch_proof",
                RehearsalCriterionKind::RchProof,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "proof substrate is available for the plan fixture",
            ));
            criteria.push(criterion(
                "resource_envelope",
                RehearsalCriterionKind::ResourceEnvelope,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "capacity posture admits the plan fixture",
            ));
            criteria.push(criterion(
                "dirty_overlap",
                RehearsalCriterionKind::DirtyOverlapOwnership,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "owned paths do not overlap dirty paths",
            ));
        }
        WaitingOwner => {
            criteria.push(criterion(
                "mission_status",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Degraded,
                RehearsalEvidenceState::Degraded,
                "mission objective planner requires owner handoff",
            ));
            criteria.push(criterion(
                "rch_proof",
                RehearsalCriterionKind::RchProof,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "RCH substrate is not the blocker for this fixture",
            ));
            criteria.push(criterion(
                "resource_envelope",
                RehearsalCriterionKind::ResourceEnvelope,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "capacity posture is not the blocker for this fixture",
            ));
            criteria.push(criterion(
                "dirty_overlap",
                RehearsalCriterionKind::DirtyOverlapOwnership,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "dirty-overlap ownership is clear",
            ));
        }
        DirtyOverlap => {
            criteria.push(criterion(
                "mission_status",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Degraded,
                RehearsalEvidenceState::Degraded,
                "mission objective planner detected dirty overlap",
            ));
            criteria.push(criterion(
                "dirty_overlap",
                RehearsalCriterionKind::DirtyOverlapOwnership,
                RehearsalVerdict::Blocked,
                RehearsalEvidenceState::Blocked,
                "owned paths overlap dirty paths",
            ));
            criteria.push(criterion(
                "artifact_integrity",
                RehearsalCriterionKind::ArtifactIntegrity,
                RehearsalVerdict::Degraded,
                RehearsalEvidenceState::Degraded,
                "receipt remains deterministic but execution needs owner clearance",
            ));
            criteria.push(criterion(
                "rch_proof",
                RehearsalCriterionKind::RchProof,
                RehearsalVerdict::Pass,
                RehearsalEvidenceState::FixtureOnly,
                "RCH substrate is not the blocker for this fixture",
            ));
        }
        RchSubstrateBlocked => {
            criteria.push(criterion(
                "mission_status",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Blocked,
                RehearsalEvidenceState::Blocked,
                "mission objective planner reports RCH substrate blocked",
            ));
            criteria.push(criterion(
                "rch_proof",
                RehearsalCriterionKind::RchProof,
                RehearsalVerdict::Blocked,
                RehearsalEvidenceState::Blocked,
                "remote proof substrate is unavailable",
            ));
            criteria.push(criterion(
                "resource_envelope",
                RehearsalCriterionKind::ResourceEnvelope,
                RehearsalVerdict::Degraded,
                RehearsalEvidenceState::Degraded,
                "plan is held until proof substrate recovers",
            ));
            criteria.push(criterion(
                "artifact_integrity",
                RehearsalCriterionKind::ArtifactIntegrity,
                RehearsalVerdict::Degraded,
                RehearsalEvidenceState::Degraded,
                "receipt is replayable only after remote proof recovery",
            ));
        }
        WaitingExternal => {
            criteria.push(criterion(
                "mission_status",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Blocked,
                RehearsalEvidenceState::Blocked,
                "mission objective planner reports external capacity block",
            ));
            criteria.push(criterion(
                "resource_envelope",
                RehearsalCriterionKind::ResourceEnvelope,
                RehearsalVerdict::Blocked,
                RehearsalEvidenceState::Blocked,
                "capacity posture refuses the plan fixture",
            ));
            criteria.push(criterion(
                "artifact_integrity",
                RehearsalCriterionKind::ArtifactIntegrity,
                RehearsalVerdict::Degraded,
                RehearsalEvidenceState::Degraded,
                "receipt is valid but execution waits for capacity recovery",
            ));
        }
        _ => {
            criteria.push(criterion(
                "mission_status",
                RehearsalCriterionKind::ScenarioCompletion,
                RehearsalVerdict::Blocked,
                RehearsalEvidenceState::Blocked,
                "mission objective planner did not produce actionable work",
            ));
        }
    }
    criteria
}

fn rehearsal_score_for_plan(
    status: MissionObjectivePlanStatus,
    decision: &PolicyDecision,
) -> RehearsalScoreReceipt {
    RehearsalScoringEngine::score_criteria(
        "ft-7h5da.6.2",
        plan_status_label(status),
        rehearsal_criteria(status, decision),
    )
    .receipt
}

/// Outcome of a read-only steer-plan pass.
#[derive(Debug, Clone)]
pub struct SteerPlanResult {
    /// The deterministic steering receipt.
    pub receipt: SteeringReceipt,
    /// The mission-objective plan status driving the verdict.
    pub plan_status: MissionObjectivePlanStatus,
    /// The policy-preflight envelope verdict label.
    pub policy_verdict: String,
    /// The live policy preview decision for the future execution step.
    pub policy_decision: PolicyDecision,
    /// The rehearsal-score receipt produced by the live scoring engine.
    pub rehearsal_score: RehearsalScoreReceipt,
}

/// Run the read-only steer-plan pipeline for a scenario.
///
/// Deterministic: the planner is `dry_run`, and `SteeringReceipt::receipt_id` is
/// content-addressed (excludes timestamps), so the same scenario / objective /
/// workspace yields a stable receipt.
#[must_use]
pub fn steer_plan(
    scenario: SteerPlanScenario,
    objective: &str,
    workspace_id: &str,
    generated_at_ms: u64,
    created_at_ms: i64,
    ttl_ms: Option<i64>,
) -> SteerPlanResult {
    let input = scenario_input(scenario, generated_at_ms, objective);
    let plan = plan_mission_objective(&input);
    let status = plan.plan_status;
    let policy_decision = policy_preflight_decision(scenario, objective);
    let rehearsal_score = rehearsal_score_for_plan(status, &policy_decision);
    let verdict = combined_envelope_verdict(status, &policy_decision);
    let approvals = combined_required_approvals(status, &policy_decision);
    let score = u32::from(rehearsal_score.aggregate_score.score_percent) * 10;
    let receipt = SteeringReceipt::new(
        objective,
        workspace_id,
        None,
        None,
        verdict,
        Some(score),
        approvals,
        created_at_ms,
        ttl_ms,
    );
    SteerPlanResult {
        receipt,
        plan_status: status,
        policy_verdict: verdict.to_string(),
        policy_decision,
        rehearsal_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(scenario: SteerPlanScenario) -> SteerPlanResult {
        steer_plan(
            scenario,
            "ship the W3 family",
            "ws-test",
            1_704_000_000_000,
            1_704_000_000_000,
            None,
        )
    }

    #[test]
    fn scenarios_produce_distinct_deterministic_statuses() {
        let cr = run(SteerPlanScenario::CleanReady);
        assert_eq!(cr.plan_status, MissionObjectivePlanStatus::Actionable);
        assert_eq!(cr.receipt.envelope_verdict, "envelope.admit");
        assert_eq!(cr.receipt.required_approvals, [] as [std::string::String; 0]);
        assert_eq!(cr.receipt.rehearsal_score, Some(1000));
        assert!(cr.policy_decision.is_allowed());
        assert_eq!(cr.rehearsal_score.aggregate_verdict, RehearsalVerdict::Pass);

        let dirty = run(SteerPlanScenario::DirtyOverlap);
        assert_eq!(dirty.plan_status, MissionObjectivePlanStatus::DirtyOverlap);
        assert_eq!(
            dirty.receipt.required_approvals,
            vec!["approval:dirty_overlap".to_string()]
        );
        assert_eq!(dirty.receipt.rehearsal_score, Some(400));

        let rch = run(SteerPlanScenario::RchBlocked);
        assert_eq!(
            rch.plan_status,
            MissionObjectivePlanStatus::RchSubstrateBlocked
        );
        assert_eq!(
            rch.receipt.envelope_verdict,
            "envelope.blocked.rch_substrate"
        );
        assert_eq!(rch.receipt.rehearsal_score, Some(200));

        let approval = run(SteerPlanScenario::ApprovalRequired);
        assert_eq!(
            approval.plan_status,
            MissionObjectivePlanStatus::WaitingOwner
        );
        assert_eq!(
            approval.receipt.envelope_verdict,
            "envelope.requires_approval"
        );
        assert!(approval.policy_decision.requires_approval());
        assert_eq!(
            approval.receipt.required_approvals,
            vec![
                "approval:owner_handoff".to_string(),
                "approval:policy.alt_screen_unknown".to_string()
            ]
        );
        assert_eq!(approval.receipt.rehearsal_score, Some(600));

        let cap = run(SteerPlanScenario::CapacityRed);
        assert_eq!(cap.plan_status, MissionObjectivePlanStatus::WaitingExternal);
        assert_eq!(cap.receipt.envelope_verdict, "envelope.blocked.capacity");
        assert_eq!(cap.receipt.rehearsal_score, Some(250));
    }

    #[test]
    fn receipt_id_is_deterministic_and_validates() {
        let a = run(SteerPlanScenario::CleanReady);
        let b = run(SteerPlanScenario::CleanReady);
        assert_eq!(a.receipt.receipt_id, b.receipt.receipt_id);
        assert!(a.receipt.validate().is_ok());
        assert!(a.receipt.receipt_id.starts_with("steer:"));
    }

    #[test]
    fn distinct_scenarios_yield_distinct_receipt_ids_via_verdict() {
        // Same objective/workspace, different scenario -> different verdict/score
        // -> different content-addressed receipt id.
        let clean = run(SteerPlanScenario::CleanReady).receipt.receipt_id;
        let rch = run(SteerPlanScenario::RchBlocked).receipt.receipt_id;
        assert_ne!(clean, rch);
    }

    #[test]
    fn scenario_parse_roundtrip() {
        for s in SteerPlanScenario::ALL {
            assert_eq!(SteerPlanScenario::parse(s.as_str()), Ok(s));
        }
        assert!(SteerPlanScenario::parse("bogus").is_err());
    }

    #[test]
    fn pipeline_is_side_effect_free_dry_run() {
        let input = scenario_input(SteerPlanScenario::CleanReady, 1_704_000_000_000, "obj");
        let plan = plan_mission_objective(&input);
        assert!(plan.dry_run);
        assert!(!plan.side_effects_executed);
        assert!(!plan.raw_pane_content_stored);
    }
}
