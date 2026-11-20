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
use crate::plan::{
    MissionKillSwitchLevel, MissionTxContract, TxPrecondition, TxPrepareApprovalChecker,
    TxPrepareEvaluationContext, TxPrepareGateInput, TxPrepareOutcome, TxPreparePolicyAuthorizer,
    TxPrepareTargetLookup, evaluate_prepare_phase, mission_tx_prepare_gate_inputs,
};
use crate::policy::{
    ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyEngine, PolicyInput,
};
use crate::rehearsal_score::{
    RehearsalCriterionKind, RehearsalCriterionReceipt, RehearsalEvidenceRef,
    RehearsalEvidenceState, RehearsalScoreReceipt, RehearsalScoringEngine, RehearsalVerdict,
};
use crate::steering::SteeringReceipt;
use serde::{Deserialize, Serialize};

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

/// Run the policy evaluator against a synthetic steer rehearsal scenario.
///
/// The input is intentionally synthetic and harmless: the operator objective is
/// audit context, not command text, so destructive words in an objective cannot
/// trip command-gate policy as though they were executable shell bytes.
fn policy_preflight_decision(scenario: SteerPlanScenario, objective: &str) -> PolicyDecision {
    let mut capabilities = if scenario == SteerPlanScenario::ApprovalRequired {
        PaneCapabilities::unknown()
    } else {
        PaneCapabilities::prompt()
    };
    // The scenario models an unreserved target. This is fixture evidence,
    // never reservation authority for a subsequent live execution.
    capabilities.is_reserved = Some(false);
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

/// Summary of real contract prepare-gate evaluation performed by
/// [`steer_plan_for_contract`] (ft-hhisq).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerContractPrepareSummary {
    /// Contract transaction identifier.
    pub tx_id: String,
    /// Contract plan identifier.
    pub plan_id: String,
    /// Number of steps evaluated in the bound contract.
    pub step_count: usize,
    /// Stable prepare outcome label (`all_ready`, `require_approval`, `denied`, `deferred`, `error`).
    pub outcome: String,
    /// Kill-switch level evaluated at plan time.
    pub kill_switch: String,
    /// Prepare gate dimensions that passed across every step of the contract.
    pub gates_passed: Vec<String>,
    /// Prepare gate dimensions that failed or were unsatisfied for at least one step.
    pub gates_failed: Vec<String>,
    /// Per-step prepare gate evaluations produced by `mission_tx_prepare_gate_inputs`.
    pub gate_inputs: Vec<TxPrepareGateInput>,
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
    /// Real bound-contract prepare-gate evaluation when a tx contract was evaluated.
    pub contract_prepare: Option<SteerContractPrepareSummary>,
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
    steer_plan_bound(
        scenario,
        objective,
        workspace_id,
        generated_at_ms,
        created_at_ms,
        ttl_ms,
        None,
    )
}

/// [`steer_plan`] with the receipt bound to a tx contract's canonical hash
/// (`MissionTxContract::compute_hash`). The binding is part of the
/// content-addressed receipt id, and `steer_run_gate` only admits a receipt
/// whose bound hash equals the live contract it executes; an unbound receipt
/// is planning-only.
///
/// Note: callers that have a real [`MissionTxContract`] MUST use
/// [`steer_plan_for_contract`] so `envelope_verdict` and `required_approvals`
/// are derived from evaluating the contract's actual steps under policy rather
/// than trusting the caller-supplied `--scenario` hint alone.
pub fn steer_plan_bound(
    scenario: SteerPlanScenario,
    objective: &str,
    workspace_id: &str,
    generated_at_ms: u64,
    created_at_ms: i64,
    ttl_ms: Option<i64>,
    tx_contract_hash: Option<String>,
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
        tx_contract_hash,
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
        contract_prepare: None,
    }
}

fn kill_switch_label(level: MissionKillSwitchLevel) -> &'static str {
    match level {
        MissionKillSwitchLevel::Off => "off",
        MissionKillSwitchLevel::SafeMode => "safe_mode",
        MissionKillSwitchLevel::HardStop => "hard_stop",
    }
}

fn contract_prepare_policy_decision(
    outcome: &str,
    kill_switch: MissionKillSwitchLevel,
    gate_inputs: &[TxPrepareGateInput],
    error_detail: Option<&str>,
) -> PolicyDecision {
    if kill_switch == MissionKillSwitchLevel::HardStop {
        return PolicyDecision::deny_with_rule(
            "bound contract denied: hard_stop kill switch active",
            "tx.kill_switch.hard_stop",
        );
    }
    if let Some(err) = error_detail {
        return PolicyDecision::deny_with_rule(
            format!("bound contract prepare evaluation failed: {err}"),
            "tx.prepare.evaluation_error",
        );
    }
    match outcome {
        "all_ready" => PolicyDecision::allow_with_rule("tx.prepare.all_ready"),
        "require_approval" => {
            let reason = gate_inputs
                .iter()
                .find(|g| !g.approval_satisfied)
                .and_then(|g| g.approval_reason_code.clone())
                .unwrap_or_else(|| "tx.prepare.approval_required".to_string());
            PolicyDecision::require_approval_with_rule(
                "bound contract requires step approval",
                reason,
            )
        }
        "deferred" => PolicyDecision::deny_with_rule(
            "bound contract prepare evaluation deferred (incomplete step gates)",
            "tx.prepare.deferred",
        ),
        _ => {
            let rule = gate_inputs
                .iter()
                .find_map(|g| {
                    if !g.policy_passed {
                        g.policy_reason_code
                            .clone()
                            .or_else(|| Some("tx.prepare.policy_denied".to_string()))
                    } else if !g.preconditions_satisfied {
                        g.precondition_reason_code
                            .clone()
                            .or_else(|| Some("tx.prepare.precondition_failed".to_string()))
                    } else if !g.target_liveness {
                        g.liveness_reason_code
                            .clone()
                            .or_else(|| Some("tx.prepare.target_not_live".to_string()))
                    } else if !g.reservation_available {
                        g.reservation_reason_code
                            .clone()
                            .or_else(|| Some("tx.prepare.reservation_conflict".to_string()))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "tx.prepare.denied".to_string());
            PolicyDecision::deny_with_rule("bound contract denied by prepare gates", rule)
        }
    }
}

fn contract_gate_criterion(
    id: &str,
    passed: bool,
    degraded_only: bool,
    note: &str,
) -> RehearsalCriterionReceipt {
    let (verdict, evidence_state) = if passed {
        (RehearsalVerdict::Pass, RehearsalEvidenceState::Proven)
    } else if degraded_only {
        (RehearsalVerdict::Degraded, RehearsalEvidenceState::Degraded)
    } else {
        (RehearsalVerdict::Fail, RehearsalEvidenceState::Blocked)
    };
    RehearsalCriterionReceipt::new(
        format!("steer_plan.contract.{id}"),
        RehearsalCriterionKind::SafetyPolicy,
        verdict,
    )
    .with_evidence(RehearsalEvidenceRef::new(
        "ft.steer.plan",
        format!("contract.{id}"),
        evidence_state,
    ))
    .with_note(note)
}

/// Evaluate a bound [`MissionTxContract`] through the exact same prepare gates
/// `ft steer run` enforces (`mission_tx_prepare_gate_inputs` +
/// `evaluate_prepare_phase`), deriving `envelope_verdict`,
/// `required_approvals`, and per-gate pass/fail receipts from the live contract
/// evaluation (ft-hhisq).
///
/// The `--scenario` label acts as a planning hint only: it may further restrict
/// a contract (e.g. `dirty-overlap` or `rch-blocked`), but can NEVER relax a
/// contract denial, kill-switch block, target/precondition failure, or step
/// approval requirement into `envelope.admit`.
#[allow(clippy::too_many_arguments)]
pub fn steer_plan_for_contract<P, A, T>(
    scenario: SteerPlanScenario,
    objective: &str,
    workspace_id: &str,
    generated_at_ms: u64,
    created_at_ms: i64,
    ttl_ms: Option<i64>,
    contract: &MissionTxContract,
    policy: &P,
    approvals: &A,
    targets: &T,
    prepare_context: &TxPrepareEvaluationContext,
    kill_switch: MissionKillSwitchLevel,
) -> SteerPlanResult
where
    P: TxPreparePolicyAuthorizer,
    A: TxPrepareApprovalChecker,
    T: TxPrepareTargetLookup,
{
    let input = scenario_input(scenario, generated_at_ms, objective);
    let plan = plan_mission_objective(&input);
    let status = plan.plan_status;

    let gate_inputs = mission_tx_prepare_gate_inputs(
        contract,
        policy,
        approvals,
        targets,
        prepare_context,
        created_at_ms,
    );
    let prepare_eval = evaluate_prepare_phase(
        &contract.plan.tx_id,
        &contract.plan,
        &gate_inputs,
        kill_switch,
        created_at_ms,
    );

    let has_custom_precondition = contract
        .plan
        .preconditions
        .iter()
        .any(|p| matches!(p, TxPrecondition::Custom { .. }));
    let has_steps = !contract.plan.steps.is_empty() && !gate_inputs.is_empty();

    let kill_switch_ok = kill_switch != MissionKillSwitchLevel::HardStop;
    let preconditions_ok = has_steps
        && !has_custom_precondition
        && (contract.plan.preconditions.is_empty()
            || gate_inputs.iter().all(|g| g.preconditions_satisfied));
    let policy_ok = has_steps && gate_inputs.iter().all(|g| g.policy_passed);
    let reservations_ok = has_steps && gate_inputs.iter().all(|g| g.reservation_available);
    let target_liveness_ok = has_steps && gate_inputs.iter().all(|g| g.target_liveness);
    let approvals_ok = has_steps && gate_inputs.iter().all(|g| g.approval_satisfied);

    let mut gates_passed = Vec::new();
    let mut gates_failed = Vec::new();
    for (name, ok) in [
        ("kill_switch", kill_switch_ok),
        ("preconditions", preconditions_ok),
        ("policy", policy_ok),
        ("reservations", reservations_ok),
        ("target_liveness", target_liveness_ok),
        ("approvals", approvals_ok),
    ] {
        if ok {
            gates_passed.push(name.to_string());
        } else {
            gates_failed.push(name.to_string());
        }
    }

    let (outcome_label, contract_verdict, error_detail) = match &prepare_eval {
        Ok(report) => match report.outcome {
            TxPrepareOutcome::AllReady => ("all_ready", "envelope.admit", None),
            TxPrepareOutcome::RequireApproval => {
                ("require_approval", "envelope.requires_approval", None)
            }
            TxPrepareOutcome::Deferred => {
                ("deferred", "envelope.blocked.prepare_deferred", None)
            }
            TxPrepareOutcome::Denied => {
                let verdict = if !kill_switch_ok {
                    "envelope.blocked.kill_switch"
                } else {
                    "envelope.blocked.policy"
                };
                ("denied", verdict, None)
            }
        },
        Err(err) => ("error", "envelope.blocked.policy", Some(err.as_str())),
    };

    // Real contract gates always dominate over a permissive scenario hint.
    let status_verdict = status_envelope_verdict(status);
    let verdict = if contract_verdict.starts_with("envelope.blocked") {
        contract_verdict
    } else if status_verdict.starts_with("envelope.blocked") {
        status_verdict
    } else if contract_verdict == "envelope.requires_approval"
        || status_verdict == "envelope.requires_approval"
    {
        "envelope.requires_approval"
    } else {
        "envelope.admit"
    };

    let mut required_approvals = status_required_approvals(status);
    for gate in &gate_inputs {
        if !gate.approval_satisfied {
            let reason = gate
                .approval_reason_code
                .as_deref()
                .or_else(|| {
                    gate.required_approval
                        .as_ref()
                        .and_then(|req| req.reason_code.as_deref())
                })
                .unwrap_or("tx.prepare.approval_required");
            let token = format!("approval:step:{}:{reason}", gate.step_id.0);
            if !required_approvals.contains(&token) {
                required_approvals.push(token);
            }
        }
    }

    let policy_decision = contract_prepare_policy_decision(
        outcome_label,
        kill_switch,
        &gate_inputs,
        error_detail,
    );

    let mut criteria = rehearsal_criteria(status, &policy_decision);
    criteria.push(contract_gate_criterion(
        "kill_switch",
        kill_switch_ok,
        false,
        if kill_switch_ok {
            "kill switch allows prepare"
        } else {
            "hard_stop kill switch blocks contract"
        },
    ));
    criteria.push(contract_gate_criterion(
        "preconditions",
        preconditions_ok,
        false,
        if preconditions_ok {
            "all contract preconditions satisfied"
        } else {
            "contract preconditions failed or unsupported"
        },
    ));
    criteria.push(contract_gate_criterion(
        "policy",
        policy_ok,
        false,
        if policy_ok {
            "all contract steps allowed by policy"
        } else {
            "one or more contract steps denied by policy"
        },
    ));
    criteria.push(contract_gate_criterion(
        "reservations",
        reservations_ok,
        false,
        if reservations_ok {
            "all target reservations available"
        } else {
            "one or more targets have conflicting reservations"
        },
    ));
    criteria.push(contract_gate_criterion(
        "target_liveness",
        target_liveness_ok,
        false,
        if target_liveness_ok {
            "all target panes verified live"
        } else {
            "one or more target panes missing or not live"
        },
    ));
    criteria.push(contract_gate_criterion(
        "approvals",
        approvals_ok,
        true,
        if approvals_ok {
            "all required step approvals satisfied"
        } else {
            "one or more contract steps require explicit approval"
        },
    ));
    let rehearsal_score = RehearsalScoringEngine::score_criteria(
        "ft-hhisq",
        plan_status_label(status),
        criteria,
    )
    .receipt;

    let score = u32::from(rehearsal_score.aggregate_score.score_percent) * 10;
    let receipt = SteeringReceipt::new(
        objective,
        workspace_id,
        None,
        Some(contract.compute_hash()),
        verdict,
        Some(score),
        required_approvals,
        created_at_ms,
        ttl_ms,
    );

    SteerPlanResult {
        receipt,
        plan_status: status,
        policy_verdict: verdict.to_string(),
        policy_decision,
        rehearsal_score,
        contract_prepare: Some(SteerContractPrepareSummary {
            tx_id: contract.plan.tx_id.0.clone(),
            plan_id: contract.plan.plan_id.0.clone(),
            step_count: contract.plan.steps.len(),
            outcome: outcome_label.to_string(),
            kill_switch: kill_switch_label(kill_switch).to_string(),
            gates_passed,
            gates_failed,
            gate_inputs,
        }),
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
    fn bound_receipt_passes_the_run_gate_only_for_its_contract() {
        use crate::steer_run::{SteerRunGate, steer_run_gate};
        let now = 1_704_000_000_000;
        let bound = steer_plan_bound(
            SteerPlanScenario::CleanReady,
            "ship the W3 family",
            "ws-test",
            now as u64,
            now,
            None,
            Some("tx-hash-a".to_string()),
        );
        assert_eq!(bound.receipt.tx_contract_hash.as_deref(), Some("tx-hash-a"));
        assert_eq!(
            steer_run_gate(&bound.receipt, None, Some("tx-hash-a"), now),
            SteerRunGate::Valid
        );
        assert_eq!(
            steer_run_gate(&bound.receipt, None, Some("tx-hash-b"), now),
            SteerRunGate::HashMismatch { contract: "tx" }
        );

        // The binding is part of the content-addressed id, and an unbound
        // planning-only receipt never admits a live contract.
        let unbound = run(SteerPlanScenario::CleanReady);
        assert_ne!(unbound.receipt.receipt_id, bound.receipt.receipt_id);
        assert!(!steer_run_gate(&unbound.receipt, None, Some("tx-hash-a"), now).is_valid());
    }

    #[test]
    fn scenarios_produce_distinct_deterministic_statuses() {
        let cr = run(SteerPlanScenario::CleanReady);
        assert_eq!(cr.plan_status, MissionObjectivePlanStatus::Actionable);
        assert_eq!(cr.receipt.envelope_verdict, "envelope.admit");
        assert!(cr.receipt.required_approvals.is_empty());
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

    mod contract_prepare_tests {
        use super::*;
        use crate::approval::ApprovalScope;
        use crate::plan::{
            MISSION_TX_SCHEMA_VERSION, MissionActorRole, MissionTxState, StepAction,
            TxCompensation, TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxPrepareTargetSnapshot,
            TxStep, TxStepId,
        };
        use std::collections::{HashMap, HashSet};

        struct StubApprovals {
            approved: bool,
        }
        impl TxPrepareApprovalChecker for StubApprovals {
            fn has_active_approval(
                &self,
                _scope: &ApprovalScope,
                _now_ms: i64,
            ) -> Result<bool, String> {
                Ok(self.approved)
            }
        }

        struct StubTargets {
            panes: HashMap<u64, TxPrepareTargetSnapshot>,
        }
        impl TxPrepareTargetLookup for StubTargets {
            fn lookup_target(
                &self,
                pane_id: u64,
            ) -> Result<Option<TxPrepareTargetSnapshot>, String> {
                Ok(self.panes.get(&pane_id).cloned())
            }
        }

        fn make_contract(action: StepAction) -> MissionTxContract {
            let tx_id = TxId("tx:hhisq-test".to_string());
            let step_id = TxStepId("step-1".to_string());
            MissionTxContract {
                tx_version: MISSION_TX_SCHEMA_VERSION,
                intent: TxIntent {
                    tx_id: tx_id.clone(),
                    requested_by: MissionActorRole::Operator,
                    summary: "test contract".to_string(),
                    correlation_id: "corr:hhisq".to_string(),
                    created_at_ms: 1_704_000_000_000,
                },
                plan: TxPlan {
                    plan_id: TxPlanId("plan:hhisq-test".to_string()),
                    tx_id,
                    steps: vec![TxStep {
                        step_id: step_id.clone(),
                        ordinal: 1,
                        action,
                        description: "test step".to_string(),
                    }],
                    preconditions: Vec::new(),
                    compensations: vec![TxCompensation {
                        for_step_id: step_id,
                        action: StepAction::StoreData {
                            key: "rollback".to_string(),
                            value: serde_json::json!(true),
                        },
                    }],
                },
                lifecycle_state: MissionTxState::Planned,
                outcome: TxOutcome::Pending,
                receipts: Vec::new(),
            }
        }

        fn live_prompt_target(pane_id: u64) -> TxPrepareTargetSnapshot {
            let mut capabilities = PaneCapabilities::prompt();
            capabilities.is_reserved = Some(false);
            TxPrepareTargetSnapshot {
                pane_id,
                is_live: true,
                capabilities,
                active_reservation_owners: HashSet::new(),
                observed_at_ms: 1_704_000_000_000,
            }
        }

        #[test]
        fn clean_ready_scenario_cannot_admit_approval_required_contract() {
            // Even when caller passes --scenario clean-ready, a SendText step
            // targeting a pane with unknown capabilities (alt-screen unknown)
            // requires approval under PolicyEngine::strict().
            let contract = make_contract(StepAction::SendText {
                pane_id: 7,
                text: "echo hi".to_string(),
                paste_mode: Some(false),
            });
            let mut unknown_caps = PaneCapabilities::unknown();
            unknown_caps.is_reserved = Some(false);
            let targets = StubTargets {
                panes: HashMap::from([(
                    7,
                    TxPrepareTargetSnapshot {
                        pane_id: 7,
                        is_live: true,
                        capabilities: unknown_caps,
                        active_reservation_owners: HashSet::new(),
                        observed_at_ms: 1_704_000_000_000,
                    },
                )]),
            };
            let engine = std::cell::RefCell::new(PolicyEngine::strict());
            let approvals = StubApprovals { approved: false };
            let ctx = TxPrepareEvaluationContext::new("ws-hhisq");
            let now = 1_704_000_000_000i64;

            let result = steer_plan_for_contract(
                SteerPlanScenario::CleanReady,
                "ship gated step",
                "ws-hhisq",
                now as u64,
                now,
                None,
                &contract,
                &engine,
                &approvals,
                &targets,
                &ctx,
                MissionKillSwitchLevel::Off,
            );
            assert_eq!(result.receipt.envelope_verdict, "envelope.requires_approval");
            assert!(
                result
                    .receipt
                    .required_approvals
                    .iter()
                    .any(|a| a.starts_with("approval:step:step-1:")),
                "expected step approval requirement, got {:?}",
                result.receipt.required_approvals
            );
            let summary = result.contract_prepare.expect("contract_prepare summary");
            assert_eq!(summary.outcome, "require_approval");
            assert!(summary.gates_failed.contains(&"approvals".to_string()));
            assert!(summary.gates_passed.contains(&"policy".to_string()));
            assert!(summary.gates_passed.contains(&"target_liveness".to_string()));
        }

        #[test]
        fn clean_ready_scenario_cannot_admit_destructive_or_missing_pane_contract() {
            // Destructive rm -rf command denied by command gate even on live prompt pane.
            let destructive = make_contract(StepAction::SendText {
                pane_id: 7,
                text: "rm -rf /tmp/important".to_string(),
                paste_mode: Some(false),
            });
            let targets = StubTargets {
                panes: HashMap::from([(7, live_prompt_target(7))]),
            };
            let engine = std::cell::RefCell::new(PolicyEngine::strict());
            let approvals = StubApprovals { approved: true };
            let ctx = TxPrepareEvaluationContext::new("ws-hhisq");
            let now = 1_704_000_000_000i64;

            let denied = steer_plan_for_contract(
                SteerPlanScenario::CleanReady,
                "dangerous command",
                "ws-hhisq",
                now as u64,
                now,
                None,
                &destructive,
                &engine,
                &approvals,
                &targets,
                &ctx,
                MissionKillSwitchLevel::Off,
            );
            assert_ne!(denied.receipt.envelope_verdict, "envelope.admit");
            let summary = denied.contract_prepare.expect("summary");
            assert!(
                summary.outcome == "denied" || summary.outcome == "require_approval",
                "destructive command must never be all_ready: {}",
                summary.outcome
            );

            // Missing target pane must fail target_liveness and block admission.
            let benign_missing = make_contract(StepAction::SendText {
                pane_id: 99,
                text: "echo ok".to_string(),
                paste_mode: Some(false),
            });
            let missing = steer_plan_for_contract(
                SteerPlanScenario::CleanReady,
                "missing target",
                "ws-hhisq",
                now as u64,
                now,
                None,
                &benign_missing,
                &engine,
                &approvals,
                &targets,
                &ctx,
                MissionKillSwitchLevel::Off,
            );
            assert_eq!(missing.receipt.envelope_verdict, "envelope.blocked.policy");
            let missing_summary = missing.contract_prepare.expect("summary");
            assert_eq!(missing_summary.outcome, "denied");
            assert!(missing_summary.gates_failed.contains(&"target_liveness".to_string()));
        }

        #[test]
        fn hard_stop_kill_switch_blocks_admissible_contract() {
            let contract = make_contract(StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!({"ok": true}),
            });
            let targets = StubTargets {
                panes: HashMap::new(),
            };
            let engine = std::cell::RefCell::new(PolicyEngine::strict());
            let approvals = StubApprovals { approved: false };
            let ctx = TxPrepareEvaluationContext::new("ws-hhisq");
            let now = 1_704_000_000_000i64;

            let blocked = steer_plan_for_contract(
                SteerPlanScenario::CleanReady,
                "blocked by kill switch",
                "ws-hhisq",
                now as u64,
                now,
                None,
                &contract,
                &engine,
                &approvals,
                &targets,
                &ctx,
                MissionKillSwitchLevel::HardStop,
            );
            assert_eq!(
                blocked.receipt.envelope_verdict,
                "envelope.blocked.kill_switch"
            );
            let summary = blocked.contract_prepare.expect("summary");
            assert!(summary.gates_failed.contains(&"kill_switch".to_string()));
        }

        #[test]
        fn all_ready_contract_admits_under_clean_ready_and_respects_restrictive_scenario() {
            let contract = make_contract(StepAction::SendText {
                pane_id: 7,
                text: "echo hello".to_string(),
                paste_mode: Some(false),
            });
            let targets = StubTargets {
                panes: HashMap::from([(7, live_prompt_target(7))]),
            };
            let engine = std::cell::RefCell::new(PolicyEngine::strict());
            let approvals = StubApprovals { approved: false };
            let ctx = TxPrepareEvaluationContext::new("ws-hhisq");
            let now = 1_704_000_000_000i64;

            let admitted = steer_plan_for_contract(
                SteerPlanScenario::CleanReady,
                "safe command",
                "ws-hhisq",
                now as u64,
                now,
                None,
                &contract,
                &engine,
                &approvals,
                &targets,
                &ctx,
                MissionKillSwitchLevel::Off,
            );
            assert_eq!(admitted.receipt.envelope_verdict, "envelope.admit");
            assert!(admitted.receipt.required_approvals.is_empty());
            assert_eq!(
                admitted.receipt.tx_contract_hash.as_deref(),
                Some(contract.compute_hash().as_str())
            );
            let summary = admitted.contract_prepare.expect("summary");
            assert_eq!(summary.outcome, "all_ready");
            assert!(summary.gates_failed.is_empty());
            assert_eq!(summary.gates_passed.len(), 6);

            // Restrictive scenario hint still tightens an otherwise all-ready contract.
            let restricted = steer_plan_for_contract(
                SteerPlanScenario::RchBlocked,
                "safe command",
                "ws-hhisq",
                now as u64,
                now,
                None,
                &contract,
                &engine,
                &approvals,
                &targets,
                &ctx,
                MissionKillSwitchLevel::Off,
            );
            assert_eq!(
                restricted.receipt.envelope_verdict,
                "envelope.blocked.rch_substrate"
            );
        }
    }
}
