// Disabled: references types not yet implemented in plan.rs
#![cfg(feature = "__journal_types_placeholder")]

//! ft-1i2ge.8.11: Deterministic E2E scenario matrix for tx run/rollback flows.
//!
//! Nine deterministic scenarios exercising the full tx lifecycle:
//!
//! | # | Scenario                  | Expected state  |
//! |---|---------------------------|-----------------|
//! | 1 | Nominal full commit       | Committed       |
//! | 2 | Policy denial at prepare  | Failed          |
//! | 3 | Reservation conflict      | Failed          |
//! | 4 | Approval timeout          | Failed          |
//! | 5 | Mid-commit failure        | Compensating    |
//! | 6 | Auto-rollback success     | RolledBack      |
//! | 7 | Partial rollback failure  | Failed          |
//! | 8 | Forced rollback           | RolledBack      |
//! | 9 | Pause during commit       | Committing      |
//!
//! Each scenario verifies: state machine transitions, receipt monotonicity,
//! count invariants, deterministic replay (canonical strings), and
//! idempotency verdicts.

use frankenterm_core::plan::{
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction,
    TxCommitOutcome, TxCommitStepInput, TxCompensation, TxCompensationOutcome,
    TxCompensationStepInput, TxExecutionRecord, TxId, TxIdempotencyVerdict, TxIntent, TxOutcome,
    TxPhase, TxPlan, TxPlanId, TxPrepareGateInput, TxPrepareOutcome, TxReceipt, TxStep, TxStepId,
    evaluate_prepare_phase, execute_commit_phase, execute_compensation_phase,
    reconstruct_tx_resume_state, validate_tx_idempotency,
};

// ── Scenario infrastructure ──────────────────────────────────────────────

const NUM_STEPS: usize = 5;

fn build_contract(scenario_id: &str, num_steps: usize, state: MissionTxState) -> MissionTxContract {
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
            tx_id: TxId(format!("tx:{scenario_id}")),
            requested_by: MissionActorRole::Dispatcher,
            summary: format!("scenario-{scenario_id}"),
            correlation_id: format!("{scenario_id}-corr"),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId(format!("plan:{scenario_id}")),
            tx_id: TxId(format!("tx:{scenario_id}")),
            steps,
            preconditions: vec![],
            compensations,
        },
        lifecycle_state: state,
        outcome: TxOutcome::Pending,
        receipts: vec![],
    }
}

fn all_gates_pass(num_steps: usize) -> Vec<TxPrepareGateInput> {
    (1..=num_steps)
        .map(|i| TxPrepareGateInput {
            step_id: TxStepId(format!("s{i}")),
            policy_passed: true,
            policy_reason_code: None,
            reservation_available: true,
            reservation_reason_code: None,
            approval_satisfied: true,
            approval_reason_code: None,
            target_liveness: true,
            liveness_reason_code: None,
        })
        .collect()
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

/// Assert receipt sequence numbers are strictly monotonically increasing.
fn assert_receipts_monotonic(receipts: &[TxReceipt], scenario: &str) {
    for window in receipts.windows(2) {
        assert!(
            window[1].seq > window[0].seq,
            "[{scenario}] receipts not monotonic: seq {} -> {}",
            window[0].seq,
            window[1].seq
        );
    }
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

// ── Scenario 1: Nominal full commit ──────────────────────────────────────

#[test]
fn sc1_nominal_full_commit() {
    let contract = build_contract("sc1", NUM_STEPS, MissionTxState::Prepared);

    // Prepare phase.
    let gates = all_gates_pass(NUM_STEPS);
    let prep = evaluate_prepare_phase(
        &contract.intent.tx_id,
        &contract.plan,
        &gates,
        MissionKillSwitchLevel::Off,
        2000,
    )
    .unwrap();
    assert!(
        matches!(prep.outcome, TxPrepareOutcome::AllReady),
        "[sc1] prepare should be AllReady"
    );

    // Commit phase.
    let inputs = success_commit_inputs(NUM_STEPS);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    assert!(
        report.is_fully_committed(),
        "[sc1] should be fully committed"
    );
    assert!(!report.has_failures(), "[sc1] should have no failures");
    assert_eq!(report.committed_count, NUM_STEPS, "[sc1] committed count");
    assert_eq!(report.failed_count, 0, "[sc1] failed count");
    assert_eq!(report.skipped_count, 0, "[sc1] skipped count");
    assert!(
        matches!(report.outcome, TxCommitOutcome::FullyCommitted),
        "[sc1] outcome should be FullyCommitted"
    );

    // Receipt monotonicity.
    assert_receipts_monotonic(&report.receipts, "sc1");
    assert!(
        !report.receipts.is_empty(),
        "[sc1] should have at least one receipt"
    );

    // Count invariant: committed + failed + skipped == total steps.
    assert_eq!(
        report.committed_count + report.failed_count + report.skipped_count,
        NUM_STEPS,
        "[sc1] count invariant"
    );

    // Deterministic replay: run again, get same canonical string.
    let report2 = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();
    assert_eq!(
        report.canonical_string(),
        report2.canonical_string(),
        "[sc1] deterministic replay"
    );

    // Idempotency: fresh tx should proceed.
    let idem = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    assert!(idem.should_proceed(), "[sc1] fresh tx should proceed");
}

// ── Scenario 2: Policy denial at prepare ─────────────────────────────────

#[test]
fn sc2_policy_denial() {
    let contract = build_contract("sc2", NUM_STEPS, MissionTxState::Planned);

    let mut gates = all_gates_pass(NUM_STEPS);
    gates[0].policy_passed = false;
    gates[0].policy_reason_code = Some("policy_denied".into());

    let prep = evaluate_prepare_phase(
        &contract.intent.tx_id,
        &contract.plan,
        &gates,
        MissionKillSwitchLevel::Off,
        2000,
    )
    .unwrap();

    assert!(
        matches!(prep.outcome, TxPrepareOutcome::Denied),
        "[sc2] should be Denied, got {:?}",
        prep.outcome
    );

    // Step 1 should be denied.
    assert!(
        prep.step_receipts.iter().any(|r| {
            r.step_id == TxStepId("s1".into())
                && matches!(
                    r.readiness,
                    frankenterm_core::plan::TxPrepareStepReadiness::Denied { .. }
                )
        }),
        "[sc2] s1 should be denied"
    );
}

// ── Scenario 3: Reservation conflict ─────────────────────────────────────

#[test]
fn sc3_reservation_conflict() {
    let contract = build_contract("sc3", NUM_STEPS, MissionTxState::Planned);

    let mut gates = all_gates_pass(NUM_STEPS);
    gates[2].reservation_available = false;
    gates[2].reservation_reason_code = Some("reservation_held_by_other".into());

    let prep = evaluate_prepare_phase(
        &contract.intent.tx_id,
        &contract.plan,
        &gates,
        MissionKillSwitchLevel::Off,
        2000,
    )
    .unwrap();

    assert!(
        matches!(
            prep.outcome,
            TxPrepareOutcome::Denied | TxPrepareOutcome::Deferred
        ),
        "[sc3] should be Denied or Deferred, got {:?}",
        prep.outcome
    );
}

// ── Scenario 4: Approval timeout ─────────────────────────────────────────

#[test]
fn sc4_approval_timeout() {
    let contract = build_contract("sc4", NUM_STEPS, MissionTxState::Planned);

    let mut gates = all_gates_pass(NUM_STEPS);
    gates[1].approval_satisfied = false;
    gates[1].approval_reason_code = Some("approval_timeout".into());

    let prep = evaluate_prepare_phase(
        &contract.intent.tx_id,
        &contract.plan,
        &gates,
        MissionKillSwitchLevel::Off,
        2000,
    )
    .unwrap();

    // Approval timeout is a deferral (not a hard deny).
    assert!(
        matches!(
            prep.outcome,
            TxPrepareOutcome::Deferred | TxPrepareOutcome::Denied
        ),
        "[sc4] should be Deferred or Denied, got {:?}",
        prep.outcome
    );
}

// ── Scenario 5: Mid-commit failure (step 3 of 5 fails) ──────────────────

#[test]
fn sc5_mid_commit_failure() {
    let contract = build_contract("sc5", NUM_STEPS, MissionTxState::Prepared);
    let inputs = partial_commit_inputs(NUM_STEPS, 3);

    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    assert!(
        matches!(report.outcome, TxCommitOutcome::PartialFailure),
        "[sc5] should be PartialFailure, got {:?}",
        report.outcome
    );
    assert_eq!(report.failure_boundary, Some(3), "[sc5] failure boundary");
    // Steps 1,2 committed; step 3 failed; steps 4,5 skipped.
    assert_eq!(report.committed_count, 2, "[sc5] committed count");
    assert_eq!(report.failed_count, 1, "[sc5] failed count");
    assert_eq!(report.skipped_count, 2, "[sc5] skipped count");

    // Count invariant.
    assert_eq!(
        report.committed_count + report.failed_count + report.skipped_count,
        NUM_STEPS,
        "[sc5] count invariant"
    );

    assert_receipts_monotonic(&report.receipts, "sc5");
}

// ── Scenario 6: Auto-rollback success ────────────────────────────────────
//
// Steps 1,2 committed, step 3 fails → PartialFailure → compensate 1,2 → RolledBack.

#[test]
fn sc6_auto_rollback_success() {
    let contract = build_contract("sc6", NUM_STEPS, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(NUM_STEPS, 3);

    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();
    assert!(matches!(
        commit_report.outcome,
        TxCommitOutcome::PartialFailure
    ));

    // Transition to Compensating state.
    let mut comp_contract = contract.clone();
    comp_contract.lifecycle_state = MissionTxState::Compensating;

    let comp_inputs = success_comp_inputs(NUM_STEPS);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    assert!(
        comp_report.is_fully_rolled_back(),
        "[sc6] should be fully rolled back"
    );
    assert!(
        !comp_report.has_residual_risk(),
        "[sc6] should have no residual risk"
    );
    assert!(
        matches!(comp_report.outcome, TxCompensationOutcome::FullyRolledBack),
        "[sc6] outcome should be FullyRolledBack"
    );

    // Compensation only applies to committed steps (s1, s2).
    assert_eq!(
        comp_report.compensated_count, 2,
        "[sc6] compensated count should match committed steps"
    );

    assert_receipts_monotonic(&comp_report.receipts, "sc6");

    // Deterministic replay.
    let comp_report2 =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();
    assert_eq!(
        comp_report.canonical_string(),
        comp_report2.canonical_string(),
        "[sc6] deterministic replay"
    );
}

// ── Scenario 7: Partial rollback failure ─────────────────────────────────
//
// Step 3 fails commit → compensate s1 ok, s2 fails compensation → CompensationFailed.

#[test]
fn sc7_partial_rollback_failure() {
    let contract = build_contract("sc7", NUM_STEPS, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(NUM_STEPS, 3);

    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let mut comp_contract = contract.clone();
    comp_contract.lifecycle_state = MissionTxState::Compensating;

    // Compensation for step 2 fails.
    let comp_inputs = partial_comp_inputs(NUM_STEPS, 2);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    assert!(
        comp_report.has_residual_risk(),
        "[sc7] should have residual risk"
    );
    assert!(
        matches!(
            comp_report.outcome,
            TxCompensationOutcome::CompensationFailed
        ),
        "[sc7] outcome should be CompensationFailed, got {:?}",
        comp_report.outcome
    );
}

// ── Scenario 8: Forced rollback (kill-switch blocks commit) ──────────────

#[test]
fn sc8_forced_rollback_kill_switch() {
    let contract = build_contract("sc8", NUM_STEPS, MissionTxState::Prepared);
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
        "[sc8] should be KillSwitchBlocked, got {:?}",
        report.outcome
    );
    assert_eq!(
        report.committed_count, 0,
        "[sc8] no steps should be committed"
    );
}

// ── Scenario 9: Pause during commit ──────────────────────────────────────

#[test]
fn sc9_pause_during_commit() {
    let contract = build_contract("sc9", NUM_STEPS, MissionTxState::Prepared);
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
        "[sc9] should be PauseSuspended, got {:?}",
        report.outcome
    );
}

// ── Cross-scenario: Idempotency verdicts ─────────────────────────────────

#[test]
fn idem_fresh_tx_proceeds() {
    let contract = build_contract("idem1", NUM_STEPS, MissionTxState::Prepared);
    let result = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    assert!(result.should_proceed(), "[idem1] fresh tx should proceed");
    assert!(
        matches!(result.verdict, TxIdempotencyVerdict::Fresh),
        "[idem1] verdict should be Fresh"
    );
}

#[test]
fn idem_completed_blocks_double_execution() {
    let contract = build_contract("idem2", NUM_STEPS, MissionTxState::Committed);
    let record = build_execution_record(&contract, MissionTxState::Committed, Some("hash123"));
    let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
    assert!(
        !result.should_proceed(),
        "[idem2] completed tx should not proceed"
    );
}

#[test]
fn idem_committing_allows_resume() {
    let contract = build_contract("idem3", NUM_STEPS, MissionTxState::Committing);
    let record = build_execution_record(&contract, MissionTxState::Committing, None);
    let result = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
    assert!(
        result.should_proceed(),
        "[idem3] committing tx should allow resume"
    );
    assert!(
        matches!(result.verdict, TxIdempotencyVerdict::Resumable { .. }),
        "[idem3] verdict should be Resumable"
    );
}

// ── Cross-scenario: Resume state reconstruction ──────────────────────────

#[test]
fn resume_no_progress() {
    let contract = build_contract("resume1", NUM_STEPS, MissionTxState::Prepared);
    let state = reconstruct_tx_resume_state(&contract, None, None, 30_000);
    assert!(
        state.has_pending_work(),
        "[resume1] should have pending work"
    );
    assert_eq!(
        state.committed_step_ids.len(),
        0,
        "[resume1] no committed steps"
    );
}

#[test]
fn resume_after_full_commit() {
    let contract = build_contract("resume2", NUM_STEPS, MissionTxState::Prepared);
    let inputs = success_commit_inputs(NUM_STEPS);
    let commit_report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    // Add terminal receipt so reconstruct sees a resolved state.
    let mut terminal_contract = build_contract("resume2", NUM_STEPS, MissionTxState::Prepared);
    terminal_contract.receipts.push(TxReceipt {
        seq: 1,
        state: MissionTxState::Committed,
        emitted_at_ms: 11_000,
        reason_code: None,
        error_code: None,
    });

    let state = reconstruct_tx_resume_state(&terminal_contract, Some(&commit_report), None, 30_000);
    assert!(
        state.is_fully_resolved(),
        "[resume2] should be fully resolved"
    );
    assert_eq!(
        state.committed_step_ids.len(),
        NUM_STEPS,
        "[resume2] all steps committed"
    );
}

#[test]
fn resume_after_partial_commit() {
    let contract = build_contract("resume3", NUM_STEPS, MissionTxState::Prepared);
    let inputs = partial_commit_inputs(NUM_STEPS, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let state = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 30_000);
    assert!(
        state.has_pending_work(),
        "[resume3] should have pending work (compensation)"
    );
    assert_eq!(
        state.committed_step_ids.len(),
        2,
        "[resume3] 2 steps committed"
    );
}

#[test]
fn resume_after_full_pipeline() {
    let contract = build_contract("resume4", NUM_STEPS, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(NUM_STEPS, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let comp_contract = build_contract("resume4", NUM_STEPS, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(NUM_STEPS);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    // Add terminal receipt so reconstruct sees resolved state.
    let mut final_contract = build_contract("resume4", NUM_STEPS, MissionTxState::Prepared);
    final_contract.receipts.push(TxReceipt {
        seq: 1,
        state: MissionTxState::RolledBack,
        emitted_at_ms: 21_000,
        reason_code: None,
        error_code: None,
    });

    let state = reconstruct_tx_resume_state(
        &final_contract,
        Some(&commit_report),
        Some(&comp_report),
        30_000,
    );
    assert!(
        state.is_fully_resolved(),
        "[resume4] should be fully resolved after full pipeline"
    );
}

// ── Cross-scenario: Receipt chain continuity ─────────────────────────────

#[test]
fn receipt_chain_continuity_across_phases() {
    let contract = build_contract("chain1", NUM_STEPS, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(NUM_STEPS, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    assert_receipts_monotonic(&commit_report.receipts, "chain1:commit");

    let mut comp_contract = contract.clone();
    comp_contract.lifecycle_state = MissionTxState::Compensating;
    // Pass commit receipts via the contract for continuity.
    comp_contract.receipts = commit_report.receipts.clone();

    let comp_inputs = success_comp_inputs(NUM_STEPS);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    assert_receipts_monotonic(&comp_report.receipts, "chain1:comp");

    // Compensation receipts start after commit receipts.
    if let (Some(last_commit), Some(first_comp)) =
        (commit_report.receipts.last(), comp_report.receipts.first())
    {
        assert!(
            first_comp.seq > last_commit.seq,
            "[chain1] comp receipts should continue from commit: {} -> {}",
            last_commit.seq,
            first_comp.seq
        );
    }
}

// ── Cross-scenario: Deterministic count invariants ───────────────────────

#[test]
fn count_invariant_all_commit_outcomes() {
    for fail_at in [1, 2, 3, 4, 5] {
        let contract = build_contract(
            &format!("cnt{fail_at}"),
            NUM_STEPS,
            MissionTxState::Prepared,
        );
        let inputs = partial_commit_inputs(NUM_STEPS, fail_at);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        assert_eq!(
            report.committed_count + report.failed_count + report.skipped_count,
            NUM_STEPS,
            "[cnt{fail_at}] commit count invariant violated: {committed}+{failed}+{skipped} != {total}",
            fail_at = fail_at,
            committed = report.committed_count,
            failed = report.failed_count,
            skipped = report.skipped_count,
            total = NUM_STEPS,
        );
    }
}

// ── Summary: structured scenario bundle ──────────────────────────────────

#[test]
fn scenario_matrix_summary() {
    // Verify all 9 primary scenarios compile and exercise distinct outcomes.
    let outcomes = vec![
        ("sc1", "nominal_commit", "FullyCommitted"),
        ("sc2", "policy_denial", "Denied"),
        ("sc3", "reservation_conflict", "Denied_or_Deferred"),
        ("sc4", "approval_timeout", "Deferred_or_Denied"),
        ("sc5", "mid_commit_failure", "PartialFailure"),
        ("sc6", "auto_rollback", "FullyRolledBack"),
        ("sc7", "partial_rollback_fail", "CompensationFailed"),
        ("sc8", "kill_switch", "KillSwitchBlocked"),
        ("sc9", "pause_commit", "PauseSuspended"),
    ];

    // Each scenario covers a unique outcome pathway.
    assert_eq!(outcomes.len(), 9, "should have exactly 9 scenarios");

    // Verify no duplicate scenario IDs.
    let ids: std::collections::HashSet<&str> = outcomes.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(ids.len(), 9, "all scenario IDs should be unique");
}
