#![cfg(feature = "subprocess-bridge")]

//! ft-1i2ge.8.10: Unit/property/concurrency correctness suite for tx semantics.
//!
//! Cross-module integration tests covering:
//! - State machine transition validity and terminal state invariants
//! - Commit phase → compensation phase pipeline integrity
//! - Idempotency guard correctness across all verdict paths
//! - Resume state reconstruction fidelity
//! - Deterministic replay and reason code assertions
//! - Step ordering and count invariants across phases
//! - Receipt chain monotonicity and continuity

use frankenterm_core::plan::{
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction,
    TxCommitOutcome, TxCommitStepInput, TxCompensation, TxCompensationOutcome,
    TxCompensationStepInput, TxExecutionRecord, TxId, TxIdempotencyVerdict, TxIntent, TxOutcome,
    TxPhase, TxPlan, TxPlanId, TxReceipt, TxResumeState, TxStep, TxStepExecutionRecord, TxStepId,
    execute_commit_phase, execute_compensation_phase, reconstruct_tx_resume_state,
    validate_tx_idempotency,
};
use serde_json;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn receipt_seq(v: &serde_json::Value) -> u64 {
    v["seq"].as_u64().unwrap()
}

fn build_contract(num_steps: usize, state: MissionTxState) -> MissionTxContract {
    let steps: Vec<TxStep> = (1..=num_steps)
        .map(|i| TxStep {
            step_id: TxStepId(format!("s{i}")),
            ordinal: i,
            action: StepAction::SendText {
                pane_id: i as u64,
                text: format!("step-{i}"),
                paste_mode: None,
            },
            description: String::new(),
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
            tx_id: TxId("tx:h10".into()),
            requested_by: MissionActorRole::Dispatcher,
            summary: "correctness-test".into(),
            correlation_id: "h10-corr-1".into(),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan:h10".into()),
            tx_id: TxId("tx:h10".into()),
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

fn build_execution_record(
    contract: &MissionTxContract,
    state: MissionTxState,
    commit_hash: Option<&str>,
    comp_hash: Option<&str>,
) -> TxExecutionRecord {
    TxExecutionRecord {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        lifecycle_state: state,
        correlation_id: contract.intent.correlation_id.clone(),
        tx_idempotency_key: TxExecutionRecord::compute_tx_key(contract),
        step_records: vec![],
        commit_report_hash: commit_hash.map(|s| s.to_string()),
        compensation_report_hash: comp_hash.map(|s| s.to_string()),
        updated_at_ms: 5000,
    }
}

// ── State Machine Transition Tests ──────────────────────────────────────────

#[test]
fn sm_commit_requires_prepared_or_committing() {
    let states = [
        MissionTxState::Draft,
        MissionTxState::Planned,
        MissionTxState::Committed,
        MissionTxState::Compensating,
        MissionTxState::RolledBack,
        MissionTxState::Failed,
    ];
    for state in &states {
        let contract = build_contract(3, *state);
        let inputs = success_commit_inputs(3);
        let result = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        );
        assert!(result.is_err(), "commit should fail for state {:?}", state);
    }
}

#[test]
fn sm_commit_accepts_prepared() {
    let contract = build_contract(3, MissionTxState::Prepared);
    let inputs = success_commit_inputs(3);
    let result = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    );
    assert!(result.is_ok());
}

#[test]
fn sm_commit_accepts_committing() {
    let contract = build_contract(3, MissionTxState::Committing);
    let inputs = success_commit_inputs(3);
    let result = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    );
    assert!(result.is_ok());
}

#[test]
fn sm_compensation_requires_compensating() {
    let states = [
        MissionTxState::Draft,
        MissionTxState::Planned,
        MissionTxState::Prepared,
        MissionTxState::Committing,
        MissionTxState::Committed,
        MissionTxState::RolledBack,
        MissionTxState::Failed,
    ];
    for state in &states {
        let contract = build_contract(3, *state);
        let commit_contract = build_contract(3, MissionTxState::Prepared);
        let commit_inputs = success_commit_inputs(3);
        let commit_report = execute_commit_phase(
            &commit_contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();
        let comp_inputs = success_comp_inputs(3);
        let result = execute_compensation_phase(&contract, &commit_report, &comp_inputs, 20_000);
        assert!(
            result.is_err(),
            "compensation should fail for state {:?}",
            state
        );
    }
}

#[test]
fn sm_terminal_states_are_terminal() {
    assert!(MissionTxState::Committed.is_terminal());
    assert!(MissionTxState::RolledBack.is_terminal());
    assert!(MissionTxState::Failed.is_terminal());
}

#[test]
fn sm_non_terminal_states_are_not_terminal() {
    assert!(!MissionTxState::Draft.is_terminal());
    assert!(!MissionTxState::Planned.is_terminal());
    assert!(!MissionTxState::Prepared.is_terminal());
    assert!(!MissionTxState::Committing.is_terminal());
    assert!(!MissionTxState::Compensating.is_terminal());
}

// ── Full Pipeline: Commit → Compensation ────────────────────────────────────

#[test]
fn pipeline_full_commit_then_full_rollback() {
    // 1. Commit all 5 steps
    let contract = build_contract(5, MissionTxState::Prepared);
    let commit_inputs = success_commit_inputs(5);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();
    assert!(commit_report.is_fully_committed());
    assert_eq!(commit_report.committed_count, 5);

    // 2. Compensate all 5 steps
    let comp_contract = build_contract(5, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(5);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();
    assert!(comp_report.is_fully_rolled_back());
    assert_eq!(comp_report.compensated_count, 5);
    assert!(!comp_report.has_residual_risk());
}

#[test]
fn pipeline_partial_commit_then_partial_rollback() {
    // 1. Commit steps 1-2 succeed, step 3 fails (5-step plan)
    let contract = build_contract(5, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(5, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();
    assert!(commit_report.has_failures());
    assert_eq!(commit_report.committed_count, 2);
    assert_eq!(commit_report.failure_boundary.as_deref(), Some("s3"));

    // 2. Compensate the 2 committed steps
    let comp_contract = build_contract(5, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(5);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();
    assert!(comp_report.is_fully_rolled_back());
    // Only 2 steps were committed, so only 2 should be compensated
    assert_eq!(comp_report.compensated_count, 2);
    assert_eq!(comp_report.receipts.len(), 2);
}

#[test]
fn pipeline_first_step_failure_nothing_to_compensate() {
    // 1. First step fails → ImmediateFailure, 0 committed
    let contract = build_contract(3, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(3, 1);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();
    let is_immediate = matches!(commit_report.outcome, TxCommitOutcome::ImmediateFailure);
    assert!(is_immediate);
    assert_eq!(commit_report.committed_count, 0);

    // 2. Compensation → NothingToCompensate
    let comp_contract = build_contract(3, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(3);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();
    let is_nothing = matches!(
        comp_report.outcome,
        TxCompensationOutcome::NothingToCompensate
    );
    assert!(is_nothing);
}

#[test]
fn pipeline_commit_step_counts_sum() {
    for num_steps in 1..=7 {
        let contract = build_contract(num_steps, MissionTxState::Prepared);
        let inputs = success_commit_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let total = report.committed_count + report.failed_count + report.skipped_count;
        assert_eq!(total, num_steps, "counts must sum for {} steps", num_steps);
    }
}

#[test]
fn pipeline_compensation_step_counts_sum() {
    for num_steps in 1..=5 {
        let contract = build_contract(num_steps, MissionTxState::Prepared);
        let commit_inputs = success_commit_inputs(num_steps);
        let commit_report = execute_commit_phase(
            &contract,
            &commit_inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let comp_contract = build_contract(num_steps, MissionTxState::Compensating);
        let comp_inputs = success_comp_inputs(num_steps);
        let comp_report =
            execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000)
                .unwrap();

        let total = comp_report.compensated_count
            + comp_report.failed_count
            + comp_report.no_compensation_count
            + comp_report.skipped_count;
        assert_eq!(
            total, num_steps,
            "comp counts must sum for {} steps",
            num_steps
        );
    }
}

// ── Receipt Chain Invariants ────────────────────────────────────────────────

#[test]
fn receipts_monotonic_through_commit() {
    let contract = build_contract(5, MissionTxState::Prepared);
    let inputs = success_commit_inputs(5);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    assert!(!report.receipts.is_empty());
    let mut prev = 0u64;
    for r in &report.receipts {
        let seq = receipt_seq(r);
        assert!(seq > prev, "receipt seq {} must be > prev {}", seq, prev);
        prev = seq;
    }
}

#[test]
fn receipts_monotonic_through_compensation() {
    let commit_contract = build_contract(3, MissionTxState::Prepared);
    let commit_inputs = success_commit_inputs(3);
    let commit_report = execute_commit_phase(
        &commit_contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let comp_contract = build_contract(3, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(3);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    let mut prev = 0u64;
    for r in &comp_report.receipts {
        let seq = receipt_seq(r);
        assert!(
            seq > prev,
            "comp receipt seq {} must be > prev {}",
            seq,
            prev
        );
        prev = seq;
    }
}

#[test]
fn receipts_continue_sequence_from_prior() {
    let mut contract = build_contract(3, MissionTxState::Prepared);
    contract.receipts.push(
        serde_json::to_value(TxReceipt {
            seq: 42,
            state: MissionTxState::Prepared,
            emitted_at_ms: 500,
            reason_code: Some("prior".into()),
            error_code: None,
        })
        .unwrap(),
    );

    let inputs = success_commit_inputs(3);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    // First new receipt should continue from seq 43
    assert!(receipt_seq(&report.receipts[0]) > 42);
}

// ── Idempotency Guard Correctness ──────────────────────────────────────────

#[test]
fn idempotency_full_lifecycle_fresh_commit_then_duplicate() {
    let contract = build_contract(3, MissionTxState::Prepared);

    // Step 1: Fresh check → proceed
    let check1 = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    assert!(check1.should_proceed());

    // Step 2: After commit completes, record is terminal
    let record = TxExecutionRecord {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        lifecycle_state: MissionTxState::Committed,
        correlation_id: contract.intent.correlation_id.clone(),
        tx_idempotency_key: TxExecutionRecord::compute_tx_key(&contract),
        step_records: vec![],
        commit_report_hash: Some("hash".into()),
        compensation_report_hash: None,
        updated_at_ms: 5000,
    };

    // Step 3: Double-commit blocked
    let check2 = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
    assert!(!check2.should_proceed());
    let is_blocked = matches!(
        check2.verdict,
        TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
    );
    assert!(is_blocked);
}

#[test]
fn idempotency_resume_after_crash_mid_commit() {
    let contract = build_contract(5, MissionTxState::Prepared);

    // Simulate crash mid-commit: record shows Committing with step 1 done
    let record = TxExecutionRecord {
        tx_id: contract.intent.tx_id.clone(),
        plan_id: contract.plan.plan_id.clone(),
        lifecycle_state: MissionTxState::Committing,
        correlation_id: contract.intent.correlation_id.clone(),
        tx_idempotency_key: TxExecutionRecord::compute_tx_key(&contract),
        step_records: vec![
            TxStepExecutionRecord {
                step_id: TxStepId("s1".into()),
                ordinal: 1,
                phase: TxPhase::Commit,
                succeeded: true,
                step_idempotency_key: "sk1".into(),
                attempt_count: 1,
                last_attempted_at_ms: 2000,
            },
            TxStepExecutionRecord {
                step_id: TxStepId("s2".into()),
                ordinal: 2,
                phase: TxPhase::Commit,
                succeeded: true,
                step_idempotency_key: "sk2".into(),
                attempt_count: 1,
                last_attempted_at_ms: 3000,
            },
        ],
        commit_report_hash: None, // Not completed yet
        compensation_report_hash: None,
        updated_at_ms: 3000,
    };

    let check = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
    assert!(check.should_proceed());
    match &check.verdict {
        TxIdempotencyVerdict::Resumable {
            resume_from_state,
            completed_steps,
        } => {
            assert_eq!(*resume_from_state, MissionTxState::Committing);
            assert_eq!(completed_steps.len(), 2);
        }
        other => panic!("expected Resumable, got {:?}", other),
    }
}

#[test]
fn idempotency_step_level_already_succeeded_guard() {
    let record = TxStepExecutionRecord {
        step_id: TxStepId("s1".into()),
        ordinal: 1,
        phase: TxPhase::Commit,
        succeeded: true,
        step_idempotency_key: "sk".into(),
        attempt_count: 1,
        last_attempted_at_ms: 2000,
    };
    // Same phase → already succeeded
    assert!(record.is_already_succeeded(&TxPhase::Commit));
    // Different phase → not already succeeded
    assert!(!record.is_already_succeeded(&TxPhase::Compensate));
    assert!(!record.is_already_succeeded(&TxPhase::Prepare));
}

#[test]
fn idempotency_step_key_uniqueness_across_tx_ids() {
    let k1 = TxStepExecutionRecord::compute_step_key(
        &TxId("tx:a".into()),
        &TxStepId("s1".into()),
        &TxPhase::Commit,
    );
    let k2 = TxStepExecutionRecord::compute_step_key(
        &TxId("tx:b".into()),
        &TxStepId("s1".into()),
        &TxPhase::Commit,
    );
    assert_ne!(k1, k2);
}

// ── Resume State Reconstruction ─────────────────────────────────────────────

#[test]
fn resume_no_progress_shows_all_pending() {
    let contract = build_contract(4, MissionTxState::Prepared);
    let resume = reconstruct_tx_resume_state(&contract, None, None, 10_000);
    assert_eq!(resume.pending_step_ids.len(), 4);
    assert!(resume.committed_step_ids.is_empty());
    assert!(resume.compensated_step_ids.is_empty());
    assert!(!resume.commit_phase_completed);
    assert!(!resume.compensation_phase_completed);
}

#[test]
fn resume_after_full_commit_no_pending() {
    let contract = build_contract(3, MissionTxState::Prepared);
    let commit_inputs = success_commit_inputs(3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    // Put terminal receipt in contract
    let mut terminal_contract = build_contract(3, MissionTxState::Prepared);
    terminal_contract.receipts.push(
        serde_json::to_value(TxReceipt {
            seq: 1,
            state: MissionTxState::Committed,
            emitted_at_ms: 11_000,
            reason_code: None,
            error_code: None,
        })
        .unwrap(),
    );

    let resume =
        reconstruct_tx_resume_state(&terminal_contract, Some(&commit_report), None, 15_000);
    assert_eq!(resume.committed_step_ids.len(), 3);
    assert!(resume.commit_phase_completed);
    assert!(resume.is_fully_resolved());
    assert!(resume.pending_step_ids.is_empty());
}

#[test]
fn resume_after_partial_commit_shows_correct_pending() {
    let contract = build_contract(5, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(5, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let resume = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 15_000);
    assert_eq!(resume.committed_step_ids.len(), 2); // s1, s2 committed
    assert!(resume.commit_phase_completed);
}

#[test]
fn resume_after_full_pipeline_is_resolved() {
    // Full commit
    let commit_contract = build_contract(3, MissionTxState::Prepared);
    let commit_inputs = success_commit_inputs(3);
    let commit_report = execute_commit_phase(
        &commit_contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    // Full compensation
    let comp_contract = build_contract(3, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(3);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    // Resume with terminal receipt
    let mut final_contract = build_contract(3, MissionTxState::Prepared);
    final_contract.receipts.push(
        serde_json::to_value(TxReceipt {
            seq: 1,
            state: MissionTxState::RolledBack,
            emitted_at_ms: 21_000,
            reason_code: None,
            error_code: None,
        })
        .unwrap(),
    );

    let resume = reconstruct_tx_resume_state(
        &final_contract,
        Some(&commit_report),
        Some(&comp_report),
        25_000,
    );
    assert!(resume.is_fully_resolved());
    assert_eq!(resume.committed_step_ids.len(), 3);
    assert_eq!(resume.compensated_step_ids.len(), 3);
    assert!(resume.commit_phase_completed);
    assert!(resume.compensation_phase_completed);
}

// ── Kill-Switch and Pause Interaction ───────────────────────────────────────

#[test]
fn killswitch_blocks_then_idempotency_allows_retry() {
    let contract = build_contract(3, MissionTxState::Prepared);
    let inputs = success_commit_inputs(3);

    // Kill-switch blocks
    let blocked = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::SafeMode,
        false,
        10_000,
    )
    .unwrap();
    let is_blocked = matches!(blocked.outcome, TxCommitOutcome::KillSwitchBlocked);
    assert!(is_blocked);

    // Idempotency check with no prior record → fresh (retry allowed)
    let check = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    assert!(check.should_proceed());
}

#[test]
fn pause_suspends_then_idempotency_allows_retry() {
    let contract = build_contract(3, MissionTxState::Prepared);
    let inputs = success_commit_inputs(3);

    // Pause suspends
    let paused = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        true,
        10_000,
    )
    .unwrap();
    let is_paused = matches!(paused.outcome, TxCommitOutcome::PauseSuspended);
    assert!(is_paused);

    // Idempotency → fresh (retry allowed)
    let check = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    assert!(check.should_proceed());
}

// ── Deterministic Replay ────────────────────────────────────────────────────

#[test]
fn deterministic_replay_same_inputs_same_results() {
    let contract = build_contract(5, MissionTxState::Prepared);
    let inputs = partial_commit_inputs(5, 3);

    let r1 = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let r2 = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    // Same outcome
    assert_eq!(r1.outcome, r2.outcome);
    assert_eq!(r1.committed_count, r2.committed_count);
    assert_eq!(r1.failed_count, r2.failed_count);
    assert_eq!(r1.skipped_count, r2.skipped_count);
    assert_eq!(r1.failure_boundary, r2.failure_boundary);

    // Same step results
    assert_eq!(r1.step_results.len(), r2.step_results.len());
    for (s1, s2) in r1.step_results.iter().zip(r2.step_results.iter()) {
        assert_eq!(s1.step_id, s2.step_id);
        assert_eq!(s1.ordinal, s2.ordinal);
        assert_eq!(s1.outcome, s2.outcome);
    }

    // Same canonical strings
    assert_eq!(r1.canonical_string(), r2.canonical_string());
}

#[test]
fn deterministic_compensation_replay() {
    let commit_contract = build_contract(3, MissionTxState::Prepared);
    let commit_inputs = success_commit_inputs(3);
    let commit_report = execute_commit_phase(
        &commit_contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let comp_contract = build_contract(3, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(3);

    let c1 =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    let c2 =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    assert_eq!(c1.canonical_string(), c2.canonical_string());
    assert_eq!(c1.outcome, c2.outcome);
}

// ── Reason Code Assertions ──────────────────────────────────────────────────

#[test]
fn reason_codes_on_success() {
    let contract = build_contract(3, MissionTxState::Prepared);
    let inputs = success_commit_inputs(3);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    assert!(!report.reason_code.is_empty());
    assert!(report.error_code.is_none());
}

#[test]
fn reason_codes_on_failure() {
    let contract = build_contract(3, MissionTxState::Prepared);
    let inputs = partial_commit_inputs(3, 2);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    assert!(!report.reason_code.is_empty());
    assert!(report.error_code.is_some());
}

// ── Serde Roundtrip Correctness ─────────────────────────────────────────────

#[test]
fn serde_roundtrip_full_pipeline() {
    // Commit
    let contract = build_contract(4, MissionTxState::Prepared);
    let inputs = partial_commit_inputs(4, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();
    let cr_json = serde_json::to_string(&commit_report).unwrap();
    let cr_restored: frankenterm_core::plan::TxCommitReport =
        serde_json::from_str(&cr_json).unwrap();
    assert_eq!(commit_report.tx_id, cr_restored.tx_id);
    assert_eq!(commit_report.plan_id, cr_restored.plan_id);
    assert_eq!(commit_report.outcome, cr_restored.outcome);
    assert_eq!(commit_report.committed_count, cr_restored.committed_count);
    assert_eq!(commit_report.failed_count, cr_restored.failed_count);
    assert_eq!(commit_report.skipped_count, cr_restored.skipped_count);
    assert_eq!(commit_report.failure_boundary, cr_restored.failure_boundary);
    assert_eq!(commit_report.reason_code, cr_restored.reason_code);
    assert_eq!(commit_report.error_code, cr_restored.error_code);
    assert_eq!(
        commit_report.step_results.len(),
        cr_restored.step_results.len()
    );
    assert_eq!(commit_report.receipts.len(), cr_restored.receipts.len());

    // Compensation
    let comp_contract = build_contract(4, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(4);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();
    let comp_json = serde_json::to_string(&comp_report).unwrap();
    let comp_restored: frankenterm_core::plan::TxCompensationReport =
        serde_json::from_str(&comp_json).unwrap();
    assert_eq!(comp_report.outcome, comp_restored.outcome);
    assert_eq!(
        comp_report.compensated_count,
        comp_restored.compensated_count
    );
    assert_eq!(comp_report.failed_count, comp_restored.failed_count);
    assert_eq!(comp_report.skipped_count, comp_restored.skipped_count);
    assert_eq!(comp_report.reason_code, comp_restored.reason_code);
    assert_eq!(comp_report.error_code, comp_restored.error_code);
    assert_eq!(
        comp_report.step_results.len(),
        comp_restored.step_results.len()
    );
    assert_eq!(comp_report.receipts.len(), comp_restored.receipts.len());

    // Resume state
    let resume = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 25_000);
    let rs_json = serde_json::to_string(&resume).unwrap();
    let rs_restored: TxResumeState = serde_json::from_str(&rs_json).unwrap();
    assert_eq!(resume, rs_restored);

    // Idempotency check
    let check = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    let ic_json = serde_json::to_string(&check).unwrap();
    let ic_restored: frankenterm_core::plan::TxIdempotencyCheck =
        serde_json::from_str(&ic_json).unwrap();
    assert_eq!(check, ic_restored);
}

// ── Commit Step Ordering ────────────────────────────────────────────────────

#[test]
fn commit_step_results_in_ordinal_order() {
    let contract = build_contract(7, MissionTxState::Prepared);
    let inputs = success_commit_inputs(7);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    for (i, sr) in report.step_results.iter().enumerate() {
        assert_eq!(sr.ordinal, i + 1);
    }
}

#[test]
fn compensation_step_results_in_reverse_ordinal_order() {
    let commit_contract = build_contract(5, MissionTxState::Prepared);
    let commit_inputs = success_commit_inputs(5);
    let commit_report = execute_commit_phase(
        &commit_contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let comp_contract = build_contract(5, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(5);
    let comp_report =
        execute_compensation_phase(&comp_contract, &commit_report, &comp_inputs, 20_000).unwrap();

    // Should be in descending ordinal order
    for i in 1..comp_report.step_results.len() {
        assert!(comp_report.step_results[i].ordinal < comp_report.step_results[i - 1].ordinal);
    }
}

// ── Empty Plan Edge Cases ───────────────────────────────────────────────────

#[test]
fn empty_plan_commit_produces_empty_report() {
    let contract = build_contract(0, MissionTxState::Prepared);
    let inputs: Vec<TxCommitStepInput> = vec![];
    let result = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    );
    // Current implementation accepts empty plans and produces a fully-committed report.
    let report = result.unwrap();
    assert_eq!(report.committed_count, 0);
    assert_eq!(report.failed_count, 0);
    assert_eq!(report.skipped_count, 0);
}

// ── Outcome Target State Consistency ────────────────────────────────────────

#[test]
fn commit_outcome_target_states() {
    // FullyCommitted → Committed
    assert_eq!(
        TxCommitOutcome::FullyCommitted.target_tx_state(),
        MissionTxState::Committed
    );
    // PartialFailure → Failed
    assert_eq!(
        TxCommitOutcome::PartialFailure.target_tx_state(),
        MissionTxState::Failed
    );
    // ImmediateFailure → Failed
    assert_eq!(
        TxCommitOutcome::ImmediateFailure.target_tx_state(),
        MissionTxState::Failed
    );
}

#[test]
fn compensation_outcome_target_states() {
    assert_eq!(
        TxCompensationOutcome::FullyRolledBack.target_tx_state(),
        MissionTxState::RolledBack
    );
    assert_eq!(
        TxCompensationOutcome::CompensationFailed.target_tx_state(),
        MissionTxState::Failed
    );
    assert_eq!(
        TxCompensationOutcome::NothingToCompensate.target_tx_state(),
        MissionTxState::Compensated
    );
}

// ── Concurrency Stress Tests ────────────────────────────────────────────────
//
// These tests verify that tx semantics produce deterministic results under
// concurrent execution. Since the tx types are pure (no shared mutable state),
// the key invariant is: parallel calls on the same inputs yield identical outputs.

#[test]
fn concurrent_commit_determinism() {
    // Run 10 parallel commit phases on identical contracts — all must produce
    // identical reports.
    let handles: Vec<_> = (0..10)
        .map(|_| {
            std::thread::spawn(move || {
                let contract = build_contract(5, MissionTxState::Prepared);
                let inputs = success_commit_inputs(5);
                execute_commit_phase(
                    &contract,
                    &inputs,
                    MissionKillSwitchLevel::Off,
                    false,
                    10_000,
                )
                .unwrap()
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("commit thread should not panic"))
        .collect();

    let reference = &results[0];
    for (i, report) in results.iter().enumerate().skip(1) {
        assert_eq!(
            reference.committed_count, report.committed_count,
            "committed_count mismatch at iter {i}"
        );
        assert_eq!(
            reference.failed_count, report.failed_count,
            "failed_count mismatch at iter {i}"
        );
        assert_eq!(
            reference.outcome, report.outcome,
            "outcome mismatch at iter {i}"
        );
        assert_eq!(
            reference.step_results.len(),
            report.step_results.len(),
            "step_results len mismatch at iter {i}"
        );
    }
}

#[test]
fn concurrent_compensation_determinism() {
    // Build a commit report, then run 10 parallel compensation phases.
    let contract = build_contract(5, MissionTxState::Prepared);
    let commit_inputs = partial_commit_inputs(5, 3);
    let commit_report = execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let cr = commit_report.clone();
            std::thread::spawn(move || {
                let comp_contract = build_contract(5, MissionTxState::Compensating);
                let comp_inputs = success_comp_inputs(5);
                execute_compensation_phase(&comp_contract, &cr, &comp_inputs, 20_000).unwrap()
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("compensation thread should not panic"))
        .collect();

    let reference = &results[0];
    for (i, report) in results.iter().enumerate().skip(1) {
        assert_eq!(
            reference.compensated_count, report.compensated_count,
            "compensated_count mismatch at iter {i}"
        );
        assert_eq!(
            reference.outcome, report.outcome,
            "outcome mismatch at iter {i}"
        );
    }
}

#[test]
fn concurrent_idempotency_checks_consistent() {
    // Run 20 parallel idempotency checks on the same contract/record pair.
    let contract = build_contract(5, MissionTxState::Prepared);
    let record = build_execution_record(&contract, MissionTxState::Committed, Some("h1"), None);

    let handles: Vec<_> = (0..20)
        .map(|_| {
            let c = contract.clone();
            let r = record.clone();
            std::thread::spawn(move || validate_tx_idempotency(&c, TxPhase::Commit, Some(&r)))
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("idempotency thread should not panic"))
        .collect();

    // All must agree: double-commit blocked
    for (i, result) in results.iter().enumerate() {
        assert!(
            !result.should_proceed(),
            "should_proceed should be false at iter {i}"
        );
        let is_blocked = matches!(
            result.verdict,
            TxIdempotencyVerdict::DoubleExecutionBlocked { .. }
        );
        assert!(is_blocked, "expected DoubleExecutionBlocked at iter {i}");
    }
}

#[test]
fn concurrent_resume_reconstruction_determinism() {
    // Build a commit report, then reconstruct resume state from 10 parallel tasks.
    let contract = build_contract(7, MissionTxState::Prepared);
    let inputs = partial_commit_inputs(7, 4);
    let commit_report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap();

    let handles: Vec<_> = (0..10)
        .map(|_| {
            let c = contract.clone();
            let cr = commit_report.clone();
            std::thread::spawn(move || reconstruct_tx_resume_state(&c, Some(&cr), None, 15_000))
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("resume reconstruction thread should not panic")
        })
        .collect();

    let reference = &results[0];
    for (i, resume) in results.iter().enumerate().skip(1) {
        assert_eq!(
            reference.committed_step_ids, resume.committed_step_ids,
            "committed_step_ids mismatch at iter {i}"
        );
        assert_eq!(
            reference.pending_step_ids, resume.pending_step_ids,
            "pending_step_ids mismatch at iter {i}"
        );
        assert_eq!(
            reference.commit_phase_completed, resume.commit_phase_completed,
            "commit_phase_completed mismatch at iter {i}"
        );
    }
}

#[test]
fn concurrent_tx_key_computation_determinism() {
    // Compute tx idempotency keys from 50 parallel tasks.
    let contract = build_contract(10, MissionTxState::Prepared);

    let handles: Vec<_> = (0..50)
        .map(|_| {
            let c = contract.clone();
            std::thread::spawn(move || TxExecutionRecord::compute_tx_key(&c))
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().expect("tx key thread should not panic"))
        .collect();

    let reference = &results[0];
    for (i, key) in results.iter().enumerate().skip(1) {
        assert_eq!(reference, key, "key mismatch at iter {i}");
    }
}

#[test]
fn concurrent_mixed_tx_non_interference() {
    // Run commits on 10 different tx contracts in parallel — each should
    // produce results independent of the others.
    let handles: Vec<_> = (1..=10)
        .map(|n| {
            std::thread::spawn(move || {
                let contract = build_contract(n, MissionTxState::Prepared);
                let inputs = success_commit_inputs(n);
                let report = execute_commit_phase(
                    &contract,
                    &inputs,
                    MissionKillSwitchLevel::Off,
                    false,
                    10_000,
                )
                .unwrap();
                (n, report)
            })
        })
        .collect();

    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("mixed tx non-interference thread should not panic")
        })
        .collect();

    for (n, report) in &results {
        assert_eq!(
            report.committed_count, *n,
            "contract with {n} steps should commit {n}"
        );
        assert_eq!(
            report.failed_count, 0,
            "no failures expected for contract {n}"
        );
    }
}

// ── Reason Code Exhaustion ──────────────────────────────────────────────────

#[test]
fn all_commit_outcomes_have_target_states() {
    // Every TxCommitOutcome variant maps to a valid MissionTxState.
    let outcomes = [
        TxCommitOutcome::FullyCommitted,
        TxCommitOutcome::PartialFailure,
        TxCommitOutcome::ImmediateFailure,
        TxCommitOutcome::KillSwitchBlocked,
        TxCommitOutcome::PauseSuspended,
    ];
    for outcome in outcomes {
        let state: MissionTxState = outcome.target_tx_state();
        // Verify it's one of the expected states (not Draft/Planned/Prepared)
        let valid = matches!(
            state,
            MissionTxState::Committed
                | MissionTxState::Compensating
                | MissionTxState::Failed
                | MissionTxState::Committing // pause returns to committing
        );
        assert!(
            valid,
            "outcome {:?} maps to unexpected state {:?}",
            outcome, state
        );
    }
}

#[test]
fn all_compensation_outcomes_have_target_states() {
    let outcomes = [
        TxCompensationOutcome::FullyRolledBack,
        TxCompensationOutcome::CompensationFailed,
        TxCompensationOutcome::NothingToCompensate,
    ];
    for outcome in &outcomes {
        let state = outcome.target_tx_state();
        let valid = matches!(
            state,
            MissionTxState::RolledBack | MissionTxState::Compensated | MissionTxState::Failed
        );
        assert!(
            valid,
            "outcome {:?} maps to unexpected state {:?}",
            outcome, state
        );
    }
}

// ── Kill-Switch and Pause in Pipeline ───────────────────────────────────────

#[test]
fn pipeline_kill_switch_safe_mode_blocks_commit() {
    let contract = build_contract(5, MissionTxState::Prepared);
    let inputs = success_commit_inputs(5);
    let report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::SafeMode,
        false,
        10_000,
    )
    .unwrap();

    // Kill switch should block all steps
    assert_eq!(report.committed_count, 0);
    let is_killed = matches!(report.outcome, TxCommitOutcome::KillSwitchBlocked);
    assert!(
        is_killed,
        "expected KillSwitchAborted, got {:?}",
        report.outcome
    );
}

#[test]
fn pipeline_pause_suspends_commit_then_resume_idempotent() {
    // Paused commit
    let contract = build_contract(5, MissionTxState::Prepared);
    let inputs = success_commit_inputs(5);
    let paused_report = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        true, // paused
        10_000,
    )
    .unwrap();

    let is_paused = matches!(paused_report.outcome, TxCommitOutcome::PauseSuspended);
    assert!(
        is_paused,
        "expected PauseSuspended, got {:?}",
        paused_report.outcome
    );

    // Idempotency check on paused state should allow retry
    let record = build_execution_record(&contract, MissionTxState::Committing, None, None);
    let check = validate_tx_idempotency(&contract, TxPhase::Commit, Some(&record));
    assert!(
        check.should_proceed(),
        "should be able to retry after pause"
    );
}
