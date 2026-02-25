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
    execute_commit_phase, execute_compensation_phase, reconstruct_tx_resume_state,
    validate_tx_idempotency, MissionActorRole, MissionKillSwitchLevel, MissionTxContract,
    MissionTxState, MissionTxTransitionKind, MissionTxValidationError, StepAction, TxCommitOutcome,
    TxCommitStepInput, TxCompensation, TxCompensationOutcome, TxCompensationStepInput,
    TxExecutionRecord, TxId, TxIdempotencyVerdict, TxIntent, TxOutcome, TxPhase, TxPlan, TxPlanId,
    TxReceipt, TxResumeState, TxStep, TxStepExecutionRecord, TxStepId,
};

// ── Helpers ─────────────────────────────────────────────────────────────────

fn build_contract(num_steps: usize, state: MissionTxState) -> MissionTxContract {
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
        assert!(
            result.is_err(),
            "commit should fail for state {:?}",
            state
        );
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
        let result =
            execute_compensation_phase(&contract, &commit_report, &comp_inputs, 20_000);
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
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();
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
    assert_eq!(commit_report.failure_boundary, Some(3));

    // 2. Compensate the 2 committed steps
    let comp_contract = build_contract(5, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(5);
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();
    assert!(comp_report.is_fully_rolled_back());
    // Only 2 steps were committed, so only 2 should be compensated
    assert_eq!(comp_report.compensated_count, 2);
    assert_eq!(comp_report.step_results.len(), 2);
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
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();
    let is_nothing = matches!(comp_report.outcome, TxCompensationOutcome::NothingToCompensate);
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
        assert_eq!(
            total, num_steps,
            "counts must sum for {} steps",
            num_steps
        );
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
        let comp_report = execute_compensation_phase(
            &comp_contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
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
        assert!(r.seq > prev, "receipt seq {} must be > prev {}", r.seq, prev);
        prev = r.seq;
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
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();

    let mut prev = 0u64;
    for r in &comp_report.receipts {
        assert!(r.seq > prev, "comp receipt seq {} must be > prev {}", r.seq, prev);
        prev = r.seq;
    }
}

#[test]
fn receipts_continue_sequence_from_prior() {
    let mut contract = build_contract(3, MissionTxState::Prepared);
    contract.receipts.push(TxReceipt {
        seq: 42,
        state: MissionTxState::Prepared,
        emitted_at_ms: 500,
        reason_code: Some("prior".into()),
        error_code: None,
    });

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
    assert!(report.receipts[0].seq > 42);
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
    let mut record = TxExecutionRecord {
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
    terminal_contract.receipts.push(TxReceipt {
        seq: 1,
        state: MissionTxState::Committed,
        emitted_at_ms: 11_000,
        reason_code: None,
        error_code: None,
    });

    let resume = reconstruct_tx_resume_state(
        &terminal_contract,
        Some(&commit_report),
        None,
        15_000,
    );
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
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();

    // Resume with terminal receipt
    let mut final_contract = build_contract(3, MissionTxState::Prepared);
    final_contract.receipts.push(TxReceipt {
        seq: 1,
        state: MissionTxState::RolledBack,
        emitted_at_ms: 21_000,
        reason_code: None,
        error_code: None,
    });

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

    let c1 = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();

    let c2 = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();

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
    assert_eq!(commit_report, cr_restored);

    // Compensation
    let comp_contract = build_contract(4, MissionTxState::Compensating);
    let comp_inputs = success_comp_inputs(4);
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();
    let comp_json = serde_json::to_string(&comp_report).unwrap();
    let comp_restored: frankenterm_core::plan::TxCompensationReport =
        serde_json::from_str(&comp_json).unwrap();
    assert_eq!(comp_report, comp_restored);

    // Resume state
    let resume = reconstruct_tx_resume_state(&contract, Some(&commit_report), None, 25_000);
    let rs_json = serde_json::to_string(&resume).unwrap();
    let rs_restored: TxResumeState = serde_json::from_str(&rs_json).unwrap();
    assert_eq!(resume, rs_restored);

    // Idempotency check
    let check = validate_tx_idempotency(&contract, TxPhase::Commit, None);
    let ic_json = serde_json::to_string(&check).unwrap();
    let ic_restored: frankenterm_core::plan::TxIdempotencyCheckResult =
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
        assert_eq!(sr.ordinal, (i + 1) as u32);
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
    let comp_report = execute_compensation_phase(
        &comp_contract,
        &commit_report,
        &comp_inputs,
        20_000,
    )
    .unwrap();

    // Should be in descending ordinal order
    for i in 1..comp_report.step_results.len() {
        assert!(
            comp_report.step_results[i].forward_ordinal
                < comp_report.step_results[i - 1].forward_ordinal
        );
    }
}

// ── Empty Plan Edge Cases ───────────────────────────────────────────────────

#[test]
fn empty_plan_commit_rejected() {
    let contract = build_contract(0, MissionTxState::Prepared);
    let inputs: Vec<TxCommitStepInput> = vec![];
    let result = execute_commit_phase(
        &contract,
        &inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    );
    assert!(result.is_err());
}

// ── Outcome Target State Consistency ────────────────────────────────────────

#[test]
fn commit_outcome_target_states() {
    // FullyCommitted → Committed
    assert_eq!(
        TxCommitOutcome::FullyCommitted.target_tx_state(),
        MissionTxState::Committed
    );
    // PartialFailure → Compensating
    assert_eq!(
        TxCommitOutcome::PartialFailure.target_tx_state(),
        MissionTxState::Compensating
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
        MissionTxState::Failed
    );
}
