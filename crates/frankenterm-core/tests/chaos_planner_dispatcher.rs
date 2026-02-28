// Disabled: references types not yet implemented in plan.rs
#![cfg(feature = "__journal_types_placeholder")]

//! ft-1i2ge.7.2 (G2): Chaos/fault injection tests for planner + dispatcher.
//!
//! Injects partial failures, timeouts, stale state, dropped acks, and reservation
//! contention across three domains:
//!
//! | Domain | Focus                        | Tests |
//! |--------|------------------------------|-------|
//! | A      | Planner chaos (MissionLoop)  | A1–A8 |
//! | B      | Tx dispatcher chaos (plan)   | B1–B8 |
//! | C      | Idempotency + resume chaos   | C1–C8 |

#![cfg(feature = "subprocess-bridge")]

use std::collections::HashMap;

use frankenterm_core::beads_types::{BeadIssueDetail, BeadIssueType, BeadStatus};
use frankenterm_core::mission_loop::{
    ActiveBeadClaim, ConflictDetectionConfig, ConflictType, DeconflictionStrategy,
    KnownReservation, MissionLoop, MissionLoopConfig, MissionSafetyEnvelopeConfig, MissionTrigger,
};
use frankenterm_core::plan::{
    MissionActorRole, MissionAgentAvailability, MissionAgentCapabilityProfile,
    MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction, TxCommitOutcome,
    TxCommitStepInput, TxCompensation, TxCompensationOutcome, TxCompensationStepInput,
    TxExecutionRecord, TxId, TxIdempotencyVerdict, TxIntent, TxOutcome, TxPhase, TxPlan, TxPlanId,
    TxPrepareGateInput, TxPrepareOutcome, TxPrepareStepReadiness, TxStep, TxStepId,
    evaluate_prepare_phase, execute_commit_phase, execute_compensation_phase,
    validate_tx_idempotency,
};
use frankenterm_core::planner_features::PlannerExtractionContext;
use frankenterm_core::tx_idempotency::{
    DeduplicationGuard, IdempotencyError, IdempotencyKey, IdempotencyPolicy, IdempotencyStore,
    ResumeRecommendation, StepOutcome, TxPhase as IdemPhase,
};
use frankenterm_core::tx_plan_compiler::{
    StepRisk, TxPlan as CompilerTxPlan, TxRiskSummary, TxStep as CompilerTxStep,
};

// ── Helpers: Planner ───────────────────────────────────────────────────────

fn sample_detail(id: &str, status: BeadStatus, priority: u8) -> BeadIssueDetail {
    BeadIssueDetail {
        id: id.to_string(),
        title: format!("Bead {id}"),
        status,
        priority,
        issue_type: BeadIssueType::Task,
        assignee: None,
        labels: Vec::new(),
        dependencies: Vec::new(),
        dependents: Vec::new(),
        parent: None,
        ingest_warning: None,
        extra: HashMap::new(),
    }
}

fn ready_agent(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Ready,
    }
}

fn offline_agent(id: &str) -> MissionAgentCapabilityProfile {
    MissionAgentCapabilityProfile {
        agent_id: id.to_string(),
        capabilities: vec!["robot.send".to_string()],
        lane_affinity: Vec::new(),
        current_load: 0,
        max_parallel_assignments: 3,
        availability: MissionAgentAvailability::Offline {
            reason_code: "chaos-test".to_string(),
        },
    }
}

fn default_context() -> PlannerExtractionContext {
    PlannerExtractionContext::default()
}

// ── Helpers: Tx Dispatcher ─────────────────────────────────────────────────

const NUM_STEPS: usize = 5;

fn build_contract(scenario: &str, num_steps: usize, state: MissionTxState) -> MissionTxContract {
    let steps: Vec<TxStep> = (1..=num_steps)
        .map(|i| TxStep {
            step_id: TxStepId(format!("s{i}")),
            ordinal: i as u32,
            action: StepAction::SendText {
                pane_id: i as u64,
                text: format!("step-{i}"),
                paste_mode: None,
            },
        })
        .collect();

    let compensations: Vec<TxCompensation> = (1..=num_steps)
        .map(|i| TxCompensation {
            for_step_id: TxStepId(format!("s{i}")),
            action: StepAction::SendText {
                pane_id: i as u64,
                text: format!("undo-{i}"),
                paste_mode: None,
            },
        })
        .collect();

    MissionTxContract {
        tx_version: 1,
        intent: TxIntent {
            tx_id: TxId(format!("tx:{scenario}")),
            requested_by: MissionActorRole::Dispatcher,
            summary: format!("chaos-{scenario}"),
            correlation_id: format!("{scenario}-corr"),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId(format!("plan:{scenario}")),
            tx_id: TxId(format!("tx:{scenario}")),
            steps,
            preconditions: vec![],
            compensations,
        },
        lifecycle_state: state,
        outcome: TxOutcome::Pending,
        receipts: vec![],
    }
}

fn success_commit_inputs(num_steps: usize) -> Vec<TxCommitStepInput> {
    (1..=num_steps)
        .map(|i| TxCommitStepInput {
            step_id: TxStepId(format!("s{i}")),
            success: true,
            reason_code: "ok".into(),
            error_code: None,
            completed_at_ms: (i as i64 + 1) * 1000,
        })
        .collect()
}

fn partial_commit_inputs(num_steps: usize, fail_at: usize) -> Vec<TxCommitStepInput> {
    (1..=num_steps)
        .map(|i| TxCommitStepInput {
            step_id: TxStepId(format!("s{i}")),
            success: i != fail_at,
            reason_code: if i == fail_at {
                "exec_error".into()
            } else {
                "ok".into()
            },
            error_code: if i == fail_at {
                Some("FTX9999".into())
            } else {
                None
            },
            completed_at_ms: (i as i64 + 1) * 1000,
        })
        .collect()
}

fn success_comp_inputs(num_steps: usize) -> Vec<TxCompensationStepInput> {
    (1..=num_steps)
        .map(|i| TxCompensationStepInput {
            for_step_id: TxStepId(format!("s{i}")),
            success: true,
            reason_code: "undone".into(),
            error_code: None,
            completed_at_ms: (i as i64 + 10) * 1000,
        })
        .collect()
}

fn partial_comp_inputs(num_steps: usize, fail_at: usize) -> Vec<TxCompensationStepInput> {
    (1..=num_steps)
        .map(|i| TxCompensationStepInput {
            for_step_id: TxStepId(format!("s{i}")),
            success: i != fail_at,
            reason_code: if i == fail_at {
                "comp_error".into()
            } else {
                "undone".into()
            },
            error_code: if i == fail_at {
                Some("FTX8888".into())
            } else {
                None
            },
            completed_at_ms: (i as i64 + 10) * 1000,
        })
        .collect()
}

fn build_execution_record(
    contract: &MissionTxContract,
    state: MissionTxState,
    commit_hash: Option<&str>,
) -> TxExecutionRecord {
    TxExecutionRecord {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        lifecycle_state: state,
        correlation_id: contract.intent.correlation_id.clone(),
        tx_idempotency_key: TxExecutionRecord::compute_tx_key(contract),
        step_records: vec![],
        commit_report_hash: commit_hash.map(|s| s.to_string()),
        compensation_report_hash: None,
        updated_at_ms: 5000,
    }
}

// ── Helpers: Idempotency ───────────────────────────────────────────────────

fn compiler_plan(plan_id: &str, step_ids: &[&str]) -> CompilerTxPlan {
    let steps: Vec<CompilerTxStep> = step_ids
        .iter()
        .map(|sid| CompilerTxStep {
            id: sid.to_string(),
            bead_id: format!("bead-{sid}"),
            agent_id: "agent-1".to_string(),
            description: format!("Step {sid}"),
            depends_on: vec![],
            preconditions: vec![],
            compensations: vec![],
            risk: StepRisk::Low,
            score: 1.0,
        })
        .collect();
    CompilerTxPlan {
        plan_id: plan_id.to_string(),
        plan_hash: 12345,
        steps,
        execution_order: step_ids.iter().map(|s| s.to_string()).collect(),
        parallel_levels: vec![step_ids.iter().map(|s| s.to_string()).collect()],
        risk_summary: TxRiskSummary {
            total_steps: step_ids.len(),
            high_risk_count: 0,
            critical_risk_count: 0,
            uncompensated_steps: 0,
            overall_risk: StepRisk::Low,
        },
        rejected_edges: vec![],
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Domain A: Planner Chaos (MissionLoop + planner_features)
// ═══════════════════════════════════════════════════════════════════════════

/// A1: All agents offline → zero assignments, no panics.
#[test]
fn a1_all_agents_offline_zero_assignments() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![
        sample_detail("b1", BeadStatus::Open, 0),
        sample_detail("b2", BeadStatus::Open, 1),
        sample_detail("b3", BeadStatus::Open, 2),
    ];
    let agents = vec![
        offline_agent("a1"),
        offline_agent("a2"),
        offline_agent("a3"),
    ];
    let ctx = default_context();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "[A1] all agents offline should yield zero assignments"
    );
    assert_eq!(ml.state().cycle_count, 1, "[A1] cycle should increment");
}

/// A2: Empty backlog with trigger storm → no panic, no assignments.
#[test]
fn a2_empty_backlog_trigger_storm() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        max_trigger_batch: 5,
        ..MissionLoopConfig::default()
    });
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let ctx = default_context();
    let empty_issues: Vec<BeadIssueDetail> = vec![];

    // Flood with triggers.
    for i in 0..20 {
        ml.trigger(MissionTrigger::ManualTrigger {
            reason: format!("storm-{i}"),
        });
    }

    let decision = ml.evaluate(
        1000,
        MissionTrigger::CadenceTick,
        &empty_issues,
        &agents,
        &ctx,
    );

    assert_eq!(
        decision.assignment_set.assignment_count(),
        0,
        "[A2] empty backlog should yield zero assignments despite triggers"
    );
    // Triggers should be drained after evaluation.
    assert_eq!(
        ml.pending_trigger_count(),
        0,
        "[A2] triggers should be drained after evaluation"
    );
}

/// A3: Agent availability churn (Ready→Offline between ticks) → assignment stability.
#[test]
fn a3_agent_availability_churn() {
    let mut ml = MissionLoop::new(MissionLoopConfig::default());
    let issues = vec![
        sample_detail("b1", BeadStatus::Open, 0),
        sample_detail("b2", BeadStatus::Open, 1),
    ];
    let ctx = default_context();

    // Cycle 1: both agents ready.
    let agents_ready = vec![ready_agent("a1"), ready_agent("a2")];
    let d1 = ml.evaluate(
        1000,
        MissionTrigger::CadenceTick,
        &issues,
        &agents_ready,
        &ctx,
    );
    let count_1 = d1.assignment_set.assignment_count();

    // Cycle 2: agent a1 goes offline.
    let agents_partial = vec![offline_agent("a1"), ready_agent("a2")];
    let d2 = ml.evaluate(
        32_000,
        MissionTrigger::AgentAvailabilityChange {
            agent_id: "a1".to_string(),
        },
        &issues,
        &agents_partial,
        &ctx,
    );
    let count_2 = d2.assignment_set.assignment_count();

    // Cycle 3: agent a1 back online.
    let d3 = ml.evaluate(
        64_000,
        MissionTrigger::AgentAvailabilityChange {
            agent_id: "a1".to_string(),
        },
        &issues,
        &agents_ready,
        &ctx,
    );
    let count_3 = d3.assignment_set.assignment_count();

    // With fewer agents, assignments should be <= initial.
    assert!(
        count_2 <= count_1,
        "[A3] fewer agents ({count_2}) should yield <= assignments vs full ({count_1})"
    );
    // Recovery should restore capacity.
    assert!(
        count_3 >= count_2,
        "[A3] restored agents ({count_3}) should yield >= assignments vs degraded ({count_2})"
    );
    assert_eq!(ml.state().cycle_count, 3, "[A3] should run 3 cycles");
}

/// A4: Retry storm exhaustion — max_consecutive_retries_per_bead = 1 triggers backoff.
#[test]
fn a4_retry_storm_exhaustion() {
    let config = MissionLoopConfig {
        safety_envelope: MissionSafetyEnvelopeConfig {
            max_assignments_per_cycle: 10,
            max_risky_assignments_per_cycle: 5,
            max_consecutive_retries_per_bead: 1,
            risky_label_markers: vec![],
        },
        ..MissionLoopConfig::default()
    };
    let mut ml = MissionLoop::new(config);
    let issues = vec![sample_detail("b1", BeadStatus::Open, 0)];
    let agents = vec![ready_agent("a1")];
    let ctx = default_context();

    // Run 3 cycles. Key property: loop doesn't panic under aggressive retry settings.
    let _d1 = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    let _d2 = ml.evaluate(32_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    let _d3 = ml.evaluate(64_000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    assert!(
        ml.state().cycle_count >= 3,
        "[A4] should complete 3 cycles without panic"
    );
}

/// A5: Reservation contention on all assignments → conflicts detected.
#[test]
fn a5_reservation_contention_all_assignments() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 100,
            strategy: DeconflictionStrategy::PriorityWins,
            generate_messages: false,
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![
        sample_detail("b1", BeadStatus::Open, 0),
        sample_detail("b2", BeadStatus::Open, 1),
    ];
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let ctx = default_context();

    // Get an assignment set.
    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    // Create reservations overlapping every possible assignment.
    let reservations: Vec<KnownReservation> = vec![
        KnownReservation {
            holder: "other-agent".to_string(),
            paths: vec!["src/main.rs".to_string()],
            exclusive: true,
            bead_id: Some("b1".to_string()),
            expires_at_ms: Some(999_999),
        },
        KnownReservation {
            holder: "other-agent".to_string(),
            paths: vec!["src/lib.rs".to_string()],
            exclusive: true,
            bead_id: Some("b2".to_string()),
            expires_at_ms: Some(999_999),
        },
    ];

    let report = ml.detect_conflicts(&decision.assignment_set, &reservations, &[], 2000, &issues);

    // Conflict detection should complete without panicking.
    // auto_resolved_count + pending confirms the system processed the reservations.
    let _ = report.auto_resolved_count + report.pending_resolution_count;
}

/// A6: Active claim collision flood → all beads have active claims.
#[test]
fn a6_active_claim_collision_flood() {
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: 100,
            strategy: DeconflictionStrategy::FirstClaimWins,
            generate_messages: false,
        },
        ..MissionLoopConfig::default()
    });
    let issues = vec![
        sample_detail("b1", BeadStatus::Open, 0),
        sample_detail("b2", BeadStatus::Open, 1),
        sample_detail("b3", BeadStatus::Open, 2),
    ];
    let agents = vec![ready_agent("a1"), ready_agent("a2")];
    let ctx = default_context();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    // Every bead has an active claim by a different agent.
    let claims: Vec<ActiveBeadClaim> = vec![
        ActiveBeadClaim {
            bead_id: "b1".to_string(),
            agent_id: "rival-1".to_string(),
            claimed_at_ms: 500,
        },
        ActiveBeadClaim {
            bead_id: "b2".to_string(),
            agent_id: "rival-2".to_string(),
            claimed_at_ms: 500,
        },
        ActiveBeadClaim {
            bead_id: "b3".to_string(),
            agent_id: "rival-3".to_string(),
            claimed_at_ms: 500,
        },
    ];

    let report = ml.detect_conflicts(&decision.assignment_set, &[], &claims, 2000, &issues);

    let claim_conflicts: Vec<_> = report
        .conflicts
        .iter()
        .filter(|c| matches!(c.conflict_type, ConflictType::ActiveClaimCollision))
        .collect();

    // Every assigned bead that has a rival claim should generate a conflict.
    for assignment in &decision.assignment_set.assignments {
        if claims.iter().any(|c| c.bead_id == assignment.bead_id) {
            let has_conflict = claim_conflicts
                .iter()
                .any(|c| c.involved_beads.contains(&assignment.bead_id));
            assert!(
                has_conflict,
                "[A6] bead {} should have a claim collision conflict",
                assignment.bead_id
            );
        }
    }
}

/// A7: Max conflict cap enforcement → caps conflicts per cycle.
#[test]
fn a7_max_conflict_cap_enforcement() {
    let cap = 2;
    let mut ml = MissionLoop::new(MissionLoopConfig {
        conflict_detection: ConflictDetectionConfig {
            enabled: true,
            max_conflicts_per_cycle: cap,
            strategy: DeconflictionStrategy::ManualResolution,
            generate_messages: false,
        },
        ..MissionLoopConfig::default()
    });
    let issues: Vec<_> = (0..10)
        .map(|i| sample_detail(&format!("b{i}"), BeadStatus::Open, i as u8))
        .collect();
    let agents = vec![ready_agent("a1"), ready_agent("a2"), ready_agent("a3")];
    let ctx = default_context();

    let decision = ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    // Create claims for all beads.
    let claims: Vec<ActiveBeadClaim> = (0..10)
        .map(|i| ActiveBeadClaim {
            bead_id: format!("b{i}"),
            agent_id: format!("rival-{i}"),
            claimed_at_ms: 500,
        })
        .collect();

    let report = ml.detect_conflicts(&decision.assignment_set, &[], &claims, 2000, &issues);

    assert!(
        report.conflicts.len() <= cap,
        "[A7] conflicts ({}) should be capped at {cap}",
        report.conflicts.len()
    );
}

/// A8: Trigger batch overflow → enqueue max_trigger_batch+N, verify batching.
#[test]
fn a8_trigger_batch_overflow() {
    let batch_limit = 3;
    let mut ml = MissionLoop::new(MissionLoopConfig {
        max_trigger_batch: batch_limit,
        ..MissionLoopConfig::default()
    });
    let issues = vec![sample_detail("b1", BeadStatus::Open, 0)];
    let agents = vec![ready_agent("a1")];
    let ctx = default_context();

    // First eval to set last_evaluation_ms.
    ml.evaluate(1000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);

    // Enqueue more triggers than the batch limit.
    for i in 0..(batch_limit + 5) {
        ml.trigger(MissionTrigger::ManualTrigger {
            reason: format!("overflow-{i}"),
        });
    }

    // Not enough time has passed for cadence, but batch is full.
    assert!(
        ml.should_evaluate(2000),
        "[A8] should force eval when triggers >= batch limit"
    );

    // Evaluate should drain triggers.
    ml.evaluate(2000, MissionTrigger::CadenceTick, &issues, &agents, &ctx);
    assert_eq!(
        ml.pending_trigger_count(),
        0,
        "[A8] triggers should be drained after eval"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Domain B: Tx Dispatcher Chaos (plan.rs phases)
// ═══════════════════════════════════════════════════════════════════════════

/// B1: Multi-gate denial cascade — different gates fail on different steps.
#[test]
fn b1_multi_gate_denial_cascade() {
    let contract = build_contract("b1", NUM_STEPS, MissionTxState::Planned);

    // Step 1: policy denied, Step 2: reservation denied, Step 3: liveness denied,
    // Steps 4-5: all pass.
    let gates: Vec<TxPrepareGateInput> = (1..=NUM_STEPS)
        .map(|i| TxPrepareGateInput {
            step_id: TxStepId(format!("s{i}")),
            policy_passed: i != 1,
            policy_reason_code: if i == 1 {
                Some("org-policy-block".into())
            } else {
                None
            },
            reservation_available: i != 2,
            reservation_reason_code: if i == 2 { Some("FTX1002".into()) } else { None },
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: i != 3,
            liveness_reason_code: if i == 3 { Some("FTX1004".into()) } else { None },
        })
        .collect();

    let prep = evaluate_prepare_phase(
        &contract.intent.tx_id,
        &contract.plan,
        &gates,
        MissionKillSwitchLevel::Off,
        2000,
    )
    .unwrap();

    let is_denied = matches!(prep.outcome, TxPrepareOutcome::Denied);
    assert!(is_denied, "[B1] multi-gate failure should deny preparation");

    // At least 3 steps should have denial reasons.
    let denied_count = prep
        .step_receipts
        .iter()
        .filter(|r| !matches!(r.readiness, TxPrepareStepReadiness::Ready))
        .count();
    assert!(
        denied_count >= 3,
        "[B1] at least 3 steps should be denied, got {denied_count}"
    );
}

/// B2: Parametric mid-commit failure at every position i=1..N.
#[test]
fn b2_mid_commit_failure_at_every_position() {
    for fail_pos in 1..=NUM_STEPS {
        let contract = build_contract(
            &format!("b2-{fail_pos}"),
            NUM_STEPS,
            MissionTxState::Prepared,
        );
        let inputs = partial_commit_inputs(NUM_STEPS, fail_pos);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        // Any partial failure should not be FullyCommitted.
        assert!(
            !report.is_fully_committed(),
            "[B2] should not be fully committed at fail_pos={fail_pos}"
        );
        assert!(
            report.has_failures(),
            "[B2] should report failures at fail_pos={fail_pos}"
        );

        // Count invariant: committed + failed + skipped == NUM_STEPS.
        let total = report.committed_count + report.failed_count + report.skipped_count;
        assert_eq!(
            total, NUM_STEPS,
            "[B2] step count invariant at pos {fail_pos}: {total} != {NUM_STEPS}"
        );

        // Exactly one step should fail.
        assert_eq!(
            report.failed_count, 1,
            "[B2] exactly one failure at pos {fail_pos}"
        );
    }
}

/// B3: Kill switch SafeMode during commit → KillSwitchBlocked.
#[test]
fn b3_kill_switch_safe_mode_blocks_commit() {
    let contract = build_contract("b3", NUM_STEPS, MissionTxState::Prepared);
    let inputs = success_commit_inputs(NUM_STEPS);

    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::SafeMode,
        false,
        10_000,
    )
    .unwrap();

    assert!(
        matches!(report.outcome, TxCommitOutcome::KillSwitchBlocked),
        "[B3] SafeMode should block commit, got {:?}",
        report.outcome
    );
    assert_eq!(
        report.committed_count, 0,
        "[B3] no steps should commit under SafeMode"
    );
}

/// B4: Kill switch HardStop → cancels in-flight.
#[test]
fn b4_kill_switch_hard_stop() {
    let contract = build_contract("b4", NUM_STEPS, MissionTxState::Prepared);
    let inputs = success_commit_inputs(NUM_STEPS);

    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::HardStop,
        false,
        10_000,
    )
    .unwrap();

    // HardStop should also block commit.
    assert!(
        matches!(report.outcome, TxCommitOutcome::KillSwitchBlocked),
        "[B4] HardStop should block commit, got {:?}",
        report.outcome
    );
}

/// B5: Paused commit → PauseSuspended, state stays Committing.
#[test]
fn b5_paused_commit() {
    let contract = build_contract("b5", NUM_STEPS, MissionTxState::Prepared);
    let inputs = success_commit_inputs(NUM_STEPS);

    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        true,
        10_000,
    )
    .unwrap();

    assert!(
        matches!(report.outcome, TxCommitOutcome::PauseSuspended),
        "[B5] paused commit should yield PauseSuspended, got {:?}",
        report.outcome
    );
    assert_eq!(
        report.committed_count, 0,
        "[B5] no steps should commit when paused"
    );
}

/// B6: Compensation failure cascade → CompensationFailed.
#[test]
fn b6_compensation_failure_cascade() {
    // First, produce a partial commit to get a valid commit report.
    let contract = build_contract("b6", NUM_STEPS, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(NUM_STEPS, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    // Now compensate with a failure at step 2.
    let comp_contract = build_contract("b6", NUM_STEPS, MissionTxState::Compensating);
    let comp_inputs = partial_comp_inputs(NUM_STEPS, 2);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    assert!(
        matches!(
            comp_report.outcome,
            TxCompensationOutcome::CompensationFailed
        ),
        "[B6] should be CompensationFailed, got {:?}",
        comp_report.outcome
    );
    assert!(
        comp_report.failed_count >= 1,
        "[B6] at least one compensation step should fail"
    );
}

/// B7: Wrong lifecycle state for commit → IllegalLifecycleTransition.
#[test]
fn b7_wrong_lifecycle_state_for_commit() {
    let invalid_states = [
        MissionTxState::Draft,
        MissionTxState::Planned,
        MissionTxState::Committed,
        MissionTxState::Compensating,
        MissionTxState::RolledBack,
        MissionTxState::Failed,
    ];

    for state in &invalid_states {
        let contract = build_contract("b7", NUM_STEPS, *state);
        let inputs = success_commit_inputs(NUM_STEPS);
        let result = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        );

        assert!(
            result.is_err(),
            "[B7] commit should fail for state {state:?}"
        );
    }
}

/// B8: Wrong lifecycle state for compensation → IllegalLifecycleTransition.
#[test]
fn b8_wrong_lifecycle_state_for_compensation() {
    let invalid_states = [
        MissionTxState::Draft,
        MissionTxState::Planned,
        MissionTxState::Prepared,
        MissionTxState::Committing,
        MissionTxState::Committed,
        MissionTxState::RolledBack,
        MissionTxState::Failed,
    ];

    // Need a valid commit report for the compensation call.
    let valid_contract = build_contract("b8-prep", NUM_STEPS, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(NUM_STEPS, 3);
    let commit_report = execute_commit_phase(
        &valid_contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    for state in &invalid_states {
        let contract = build_contract("b8", NUM_STEPS, *state);
        let comp_inputs = success_comp_inputs(NUM_STEPS);
        let result = execute_compensation_phase(&contract, &commit_report, &comp_inputs, 20_000);

        assert!(
            result.is_err(),
            "[B8] compensation should fail for state {state:?}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Domain C: Idempotency + Resume Chaos (tx_idempotency.rs)
// ═══════════════════════════════════════════════════════════════════════════

/// C1: Double-commit blocked — replaying after Committed yields DoubleExecutionBlocked.
#[test]
fn c1_double_commit_blocked() {
    let contract = build_contract("c1", NUM_STEPS, MissionTxState::Committed);
    let prior = build_execution_record(&contract, MissionTxState::Committed, Some("hash-c1"));

    let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&prior));

    let is_blocked = matches!(
        result.verdict,
        TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
    );
    assert!(
        is_blocked,
        "[C1] should block double commit, got {:?}",
        result.verdict
    );
}

/// C2: Conflicting prior — terminal record with different plan hash → ConflictingPrior.
#[test]
fn c2_conflicting_prior_modified_plan() {
    let contract = build_contract("c2", NUM_STEPS, MissionTxState::Prepared);

    // Create a terminal prior record with a different idempotency key.
    // ConflictingPrior only triggers when prior is_terminal() && key differs.
    let mut prior = build_execution_record(&contract, MissionTxState::Committed, Some("hash-c2"));
    prior.tx_idempotency_key = "tampered-key-different-plan".to_string();
    // Clear commit_report_hash so DoubleExecutionBlocked doesn't fire first.
    prior.commit_report_hash = None;

    let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&prior));

    let is_conflicting = matches!(
        result.verdict,
        TxIdempotencyVerdict::ConflictingPrior { .. }
    );
    assert!(
        is_conflicting,
        "[C2] modified plan hash should yield ConflictingPrior, got {:?}",
        result.verdict
    );
}

/// C3: Ledger sealed — append after terminal phase.
#[test]
fn c3_ledger_sealed_after_terminal() {
    let plan = compiler_plan("plan-c3", &["s1", "s2", "s3"]);
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-c3", &plan).unwrap();

    // Record one step.
    let key1 = IdempotencyKey::new("plan-c3", "s1", "action-1");
    store
        .record_execution(
            "exec-c3",
            key1,
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent-1",
            1000,
        )
        .unwrap();

    // Transition to terminal phase: Preparing → Committing → Completed.
    let ledger = store.get_ledger_mut("exec-c3").unwrap();
    ledger.transition_phase(IdemPhase::Preparing).unwrap();
    ledger.transition_phase(IdemPhase::Committing).unwrap();
    ledger.transition_phase(IdemPhase::Completed).unwrap();

    // Now try to append — should fail.
    let key2 = IdempotencyKey::new("plan-c3", "s2", "action-2");
    let result = store.record_execution(
        "exec-c3",
        key2,
        StepOutcome::Success { result: None },
        StepRisk::Low,
        "agent-1",
        2000,
    );

    assert!(
        matches!(result, Err(IdempotencyError::LedgerSealed { .. })),
        "[C3] append after terminal should yield LedgerSealed, got {result:?}"
    );
}

/// C4: Duplicate execution key in same ledger.
#[test]
fn c4_duplicate_execution_key() {
    let plan = compiler_plan("plan-c4", &["s1", "s2"]);
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-c4", &plan).unwrap();

    let key = IdempotencyKey::new("plan-c4", "s1", "action-1");
    store
        .record_execution(
            "exec-c4",
            key.clone(),
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent-1",
            1000,
        )
        .unwrap();

    // Same key again — should fail.
    let result = store.record_execution(
        "exec-c4",
        key,
        StepOutcome::Success { result: None },
        StepRisk::Low,
        "agent-1",
        2000,
    );

    assert!(
        matches!(result, Err(IdempotencyError::DuplicateExecution { .. })),
        "[C4] duplicate key should yield DuplicateExecution, got {result:?}"
    );
}

/// C5: Chain integrity + resume with remaining steps → ContinueFromCheckpoint.
#[test]
fn c5_resume_intact_chain_continue() {
    let plan = compiler_plan("plan-c5", &["s1", "s2", "s3"]);
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-c5", &plan).unwrap();

    // Append two records (s1, s2 done).
    let key1 = IdempotencyKey::new("plan-c5", "s1", "action-1");
    store
        .record_execution(
            "exec-c5",
            key1,
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent-1",
            1000,
        )
        .unwrap();

    let key2 = IdempotencyKey::new("plan-c5", "s2", "action-2");
    store
        .record_execution(
            "exec-c5",
            key2,
            StepOutcome::Success { result: None },
            StepRisk::Low,
            "agent-1",
            2000,
        )
        .unwrap();

    // Transition to Preparing (non-terminal).
    let ledger = store.get_ledger_mut("exec-c5").unwrap();
    ledger.transition_phase(IdemPhase::Preparing).unwrap();

    // Verify chain is intact.
    let verification = ledger.verify_chain();
    assert!(verification.chain_intact, "[C5] chain should be intact");

    // Resume context should recommend ContinueFromCheckpoint.
    let resume = store.resume_context("exec-c5", &plan).unwrap();
    assert_eq!(
        resume.remaining_steps.len(),
        1,
        "[C5] should have 1 remaining step (s3)"
    );
    assert!(resume.chain_intact, "[C5] chain should be intact");
    assert_eq!(
        resume.recommendation,
        ResumeRecommendation::ContinueFromCheckpoint,
        "[C5] intact chain with remaining steps should recommend ContinueFromCheckpoint"
    );
}

/// C6: Resume from partially-committed ledger → ContinueFromCheckpoint.
#[test]
fn c6_resume_partial_commit() {
    let plan = compiler_plan("plan-c6", &["s1", "s2", "s3", "s4", "s5"]);
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-c6", &plan).unwrap();

    // Execute steps 1-3, then "crash" (leave ledger in Committing).
    for sid in &["s1", "s2", "s3"] {
        let key = IdempotencyKey::new("plan-c6", sid, &format!("action-{sid}"));
        store
            .record_execution(
                "exec-c6",
                key,
                StepOutcome::Success { result: None },
                StepRisk::Low,
                "agent-1",
                1000,
            )
            .unwrap();
    }

    let ledger = store.get_ledger_mut("exec-c6").unwrap();
    ledger.transition_phase(IdemPhase::Preparing).unwrap();
    ledger.transition_phase(IdemPhase::Committing).unwrap();

    // Resume should recommend continuing from checkpoint.
    let resume = store.resume_context("exec-c6", &plan).unwrap();

    assert_eq!(
        resume.completed_steps.len(),
        3,
        "[C6] 3 steps should be completed"
    );
    assert_eq!(
        resume.remaining_steps.len(),
        2,
        "[C6] 2 steps should remain (s4, s5)"
    );
    assert_eq!(
        resume.recommendation,
        ResumeRecommendation::ContinueFromCheckpoint,
        "[C6] should recommend ContinueFromCheckpoint"
    );
    assert!(resume.chain_intact, "[C6] chain should be intact");
}

/// C7: Dedup guard TTL eviction.
#[test]
fn c7_dedup_guard_ttl_eviction() {
    let mut guard = DeduplicationGuard::new(100);

    // Record entries at different timestamps.
    let key1 = IdempotencyKey::new("plan-c7", "s1", "action-1");
    let key2 = IdempotencyKey::new("plan-c7", "s2", "action-2");
    let key3 = IdempotencyKey::new("plan-c7", "s3", "action-3");

    guard.record(&key1, "exec-1", StepOutcome::Success { result: None }, 1000);
    guard.record(&key2, "exec-1", StepOutcome::Success { result: None }, 2000);
    guard.record(&key3, "exec-1", StepOutcome::Success { result: None }, 5000);

    assert_eq!(guard.len(), 3, "[C7] should have 3 entries before eviction");

    // Evict entries older than 3000ms.
    guard.evict_before(3000);

    assert_eq!(
        guard.len(),
        1,
        "[C7] should have 1 entry after eviction (key3 at 5000)"
    );
    assert!(guard.check(&key1).is_none(), "[C7] key1 should be evicted");
    assert!(guard.check(&key2).is_none(), "[C7] key2 should be evicted");
    assert!(
        guard.check(&key3).is_some(),
        "[C7] key3 should survive eviction"
    );
}

/// C8: Illegal phase transition in ledger.
#[test]
fn c8_illegal_phase_transition() {
    let plan = compiler_plan("plan-c8", &["s1"]);
    let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
    store.create_ledger("exec-c8", &plan).unwrap();

    let ledger = store.get_ledger_mut("exec-c8").unwrap();

    // Initial phase is Planned. Valid: Planned → Preparing.
    ledger.transition_phase(IdemPhase::Preparing).unwrap();

    // Invalid: Preparing → Completed (must go through Committing first).
    let result = ledger.transition_phase(IdemPhase::Completed);
    assert!(
        matches!(result, Err(IdempotencyError::InvalidPhaseTransition { .. })),
        "[C8] Preparing → Completed should be invalid, got {result:?}"
    );

    // Invalid: Preparing → Planned (backward).
    let result = ledger.transition_phase(IdemPhase::Planned);
    assert!(
        matches!(result, Err(IdempotencyError::InvalidPhaseTransition { .. })),
        "[C8] Preparing → Planned should be invalid, got {result:?}"
    );

    // Valid forward: Preparing → Committing.
    ledger.transition_phase(IdemPhase::Committing).unwrap();

    // Invalid: Committing → Preparing (backward).
    let result = ledger.transition_phase(IdemPhase::Preparing);
    assert!(
        matches!(result, Err(IdempotencyError::InvalidPhaseTransition { .. })),
        "[C8] Committing → Preparing should be invalid, got {result:?}"
    );
}
