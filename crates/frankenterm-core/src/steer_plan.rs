//! ft-7h5da.6.2: read-only `ft steer plan` pipeline.
//!
//! Runs the real mission-objective planner over a deterministic per-scenario
//! fixture, derives a policy-preflight verdict + rehearsal score from the
//! resulting plan status, and emits a [`SteeringReceipt`] (W5.1 type). The
//! pipeline is side-effect-free: the planner is `dry_run` by construction, and
//! the only writes a caller performs are the receipt itself + one audit row.
//!
//! Scenario fixtures map to the mission-objective status taxonomy so each one
//! yields a deterministic, golden-comparable receipt:
//! clean-ready → Actionable, dirty-overlap → DirtyOverlap, rch-blocked →
//! RchSubstrateBlocked, approval-required → WaitingOwner, capacity-red →
//! WaitingExternal.
//!
//! The live `RehearsalScoreEngine` + `PolicyEngine::authorize_preview` wiring
//! (vs. the deterministic status-derived score/verdict here) is the W5.2
//! refinement; this module preserves the score-binding + verdict shape the
//! SteeringReceipt records.

use crate::mission_objective_plan::{
    MissionObjectiveCandidateReadiness, MissionObjectiveCandidateWork,
    MissionObjectiveCapacityPosture, MissionObjectiveDirtyPath, MissionObjectivePlanStatus,
    MissionObjectivePlannerInput, MissionObjectiveProofAvailability, MissionObjectiveSourceKind,
    MissionObjectiveSourceSnapshot, plan_mission_objective,
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
    let snapshot =
        MissionObjectiveSourceSnapshot::new("beads", MissionObjectiveSourceKind::Beads);
    let input = MissionObjectivePlannerInput::new(generated_at_ms, "ft.steer.plan", objective)
        .with_source_snapshot(snapshot);
    let candidate =
        MissionObjectiveCandidateWork::new("steer.candidate", MissionObjectiveCandidateReadiness::ReadyBead);
    match scenario {
        SteerPlanScenario::CleanReady => input.with_candidate(candidate),
        SteerPlanScenario::DirtyOverlap => input
            .with_dirty_path(MissionObjectiveDirtyPath::new(
                "crates/frankenterm-core/src/lib.rs",
                "modified",
            ))
            .with_candidate(candidate.with_owned_path("crates/frankenterm-core/src/lib.rs")),
        SteerPlanScenario::RchBlocked => input
            .with_candidate(candidate.proof_availability(MissionObjectiveProofAvailability::Blocked)),
        SteerPlanScenario::ApprovalRequired => input.with_candidate(
            MissionObjectiveCandidateWork::new(
                "steer.candidate",
                MissionObjectiveCandidateReadiness::ActiveSameDomain,
            ),
        ),
        SteerPlanScenario::CapacityRed => input
            .with_candidate(candidate.capacity_posture(MissionObjectiveCapacityPosture::Pause)),
    }
}

/// Read-only policy-preflight verdict + required approvals derived from the
/// plan status. Mirrors `authorize_preview`'s allow / deny / require-approval
/// semantics without consuming policy budget.
fn verdict_for_status(status: MissionObjectivePlanStatus) -> (&'static str, Vec<String>) {
    use MissionObjectivePlanStatus::{
        Actionable, Blocked, Degraded, DirtyOverlap, NoReadyWork, PlanningOnly,
        RchSubstrateBlocked, Unavailable, WaitingExternal, WaitingOwner,
    };
    match status {
        Actionable => ("envelope.admit", Vec::new()),
        WaitingOwner => (
            "envelope.requires_approval",
            vec!["approval:owner_handoff".to_string()],
        ),
        DirtyOverlap => (
            "envelope.requires_approval",
            vec!["approval:dirty_overlap".to_string()],
        ),
        RchSubstrateBlocked => ("envelope.blocked.rch_substrate", Vec::new()),
        WaitingExternal => ("envelope.blocked.capacity", Vec::new()),
        Degraded => ("envelope.blocked.degraded", Vec::new()),
        Blocked | NoReadyWork | PlanningOnly | Unavailable => ("envelope.blocked", Vec::new()),
    }
}

/// Deterministic rehearsal score (0..=1000) per status — preserves the
/// score-binding shape the receipt records.
fn score_for_status(status: MissionObjectivePlanStatus) -> u32 {
    use MissionObjectivePlanStatus::{
        Actionable, Blocked, Degraded, DirtyOverlap, NoReadyWork, PlanningOnly,
        RchSubstrateBlocked, Unavailable, WaitingExternal, WaitingOwner,
    };
    match status {
        Actionable => 950,
        WaitingOwner => 600,
        DirtyOverlap => 400,
        WaitingExternal | Degraded => 250,
        RchSubstrateBlocked => 150,
        Blocked | NoReadyWork | PlanningOnly | Unavailable => 100,
    }
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
    let (verdict, approvals) = verdict_for_status(status);
    let score = score_for_status(status);
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
        assert!(cr.receipt.required_approvals.is_empty());
        assert_eq!(cr.receipt.rehearsal_score, Some(950));

        let dirty = run(SteerPlanScenario::DirtyOverlap);
        assert_eq!(dirty.plan_status, MissionObjectivePlanStatus::DirtyOverlap);
        assert_eq!(
            dirty.receipt.required_approvals,
            vec!["approval:dirty_overlap".to_string()]
        );

        let rch = run(SteerPlanScenario::RchBlocked);
        assert_eq!(rch.plan_status, MissionObjectivePlanStatus::RchSubstrateBlocked);
        assert_eq!(rch.receipt.envelope_verdict, "envelope.blocked.rch_substrate");

        let approval = run(SteerPlanScenario::ApprovalRequired);
        assert_eq!(approval.plan_status, MissionObjectivePlanStatus::WaitingOwner);
        assert_eq!(approval.receipt.envelope_verdict, "envelope.requires_approval");

        let cap = run(SteerPlanScenario::CapacityRed);
        assert_eq!(cap.plan_status, MissionObjectivePlanStatus::WaitingExternal);
        assert_eq!(cap.receipt.envelope_verdict, "envelope.blocked.capacity");
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
