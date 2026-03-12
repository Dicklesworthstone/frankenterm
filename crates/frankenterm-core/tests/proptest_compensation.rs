//! Property-based tests for compensation/rollback engine (H6).
//!
//! Covers:
//! - All success implies FullyRolledBack
//! - compensated + failed + skipped = total committed steps
//! - Compensation failure implies CompensationFailed + has_residual_risk
//! - NothingToCompensate when zero committed steps
//! - Receipt sequence monotonicity
//! - TxCompensationReport serde roundtrip
//! - TxCompensationOutcome target_tx_state consistency
//! - Rejects non-compensating lifecycle state
//! - Partial commit compensates only committed steps
//! - Missing input treated as failure

use frankenterm_core::plan::{
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction,
    TxCompensation, TxCompensationOutcome, TxCompensationReport, TxCompensationStepInput,
    TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxStep, TxStepId,
    execute_commit_phase, execute_compensation_phase,
};
use proptest::prelude::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a contract with N steps and compensations for each step.
fn make_contract_with_compensations(num_steps: usize) -> MissionTxContract {
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
            tx_id: TxId("tx:comp-prop".into()),
            requested_by: MissionActorRole::Dispatcher,
            summary: "proptest-comp".into(),
            correlation_id: "ptc-1".into(),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan:comp-prop".into()),
            tx_id: TxId("tx:comp-prop".into()),
            steps,
            preconditions: vec![],
            compensations,
        },
        lifecycle_state: MissionTxState::Compensating,
        outcome: TxOutcome::Pending,
        receipts: vec![],
    }
}

/// Get a commit report where all steps succeed (needs Prepared contract).
fn get_commit_report(num_steps: usize) -> frankenterm_core::plan::TxCommitReport {
    let mut contract = make_contract_with_compensations(num_steps);
    contract.lifecycle_state = MissionTxState::Prepared;
    let commit_inputs: Vec<_> = (1..=num_steps)
        .map(|i| frankenterm_core::plan::TxCommitStepInput {
            step_id: TxStepId(format!("s{i}")),
            success: true,
            reason_code: "ok".into(),
            error_code: None,
            completed_at_ms: (i as i64 + 1) * 1000,
        })
        .collect();
    execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap()
}

/// Get a commit report where step at `fail_at` fails (partial commit).
fn get_partial_commit_report(
    num_steps: usize,
    fail_at: usize,
) -> frankenterm_core::plan::TxCommitReport {
    let mut contract = make_contract_with_compensations(num_steps);
    contract.lifecycle_state = MissionTxState::Prepared;
    let commit_inputs: Vec<_> = (1..=num_steps)
        .map(|i| frankenterm_core::plan::TxCommitStepInput {
            step_id: TxStepId(format!("s{i}")),
            success: i != fail_at,
            reason_code: if i == fail_at {
                "err".into()
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
        .collect();
    execute_commit_phase(
        &contract,
        &commit_inputs,
        MissionKillSwitchLevel::Off,
        false,
        10_000,
    )
    .unwrap()
}

fn all_success_comp_inputs(num_steps: usize) -> Vec<TxCompensationStepInput> {
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

fn comp_inputs_with_failure_at(num_steps: usize, fail_at: usize) -> Vec<TxCompensationStepInput> {
    (1..=num_steps)
        .map(|i| TxCompensationStepInput {
            for_step_id: TxStepId(format!("s{i}")),
            success: i != fail_at,
            reason_code: if i == fail_at {
                "undo_error".into()
            } else {
                "undone".into()
            },
            error_code: if i == fail_at {
                Some("FTX2099".into())
            } else {
                None
            },
            completed_at_ms: (i as i64 + 10) * 1000,
        })
        .collect()
}

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn all_success_implies_fully_rolled_back(
        num_steps in 1usize..8,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_commit_report(num_steps);
        let comp_inputs = all_success_comp_inputs(num_steps);

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        prop_assert!(report.is_fully_rolled_back());
        prop_assert_eq!(report.compensated_count, num_steps);
        prop_assert!(!report.has_residual_risk());
    }

    #[test]
    fn counts_sum_to_committed_steps(
        num_steps in 1usize..8,
        fail_at_opt in 0usize..8,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_commit_report(num_steps);
        let comp_inputs = if fail_at_opt > 0 && fail_at_opt <= num_steps {
            comp_inputs_with_failure_at(num_steps, fail_at_opt)
        } else {
            all_success_comp_inputs(num_steps)
        };

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        let total = report.compensated_count + report.failed_count + report.skipped_count;
        prop_assert_eq!(
            total, num_steps,
            "counts must sum to total committed steps: {} + {} + {} = {} != {}",
            report.compensated_count, report.failed_count,
            report.skipped_count, total, num_steps
        );
    }

    #[test]
    fn failure_implies_compensation_failed(
        num_steps in 2usize..8,
        fail_at in 1usize..8,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_commit_report(num_steps);
        let fail_at = fail_at.min(num_steps);
        let comp_inputs = comp_inputs_with_failure_at(num_steps, fail_at);

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        prop_assert!(report.has_residual_risk());
        let is_failed = matches!(report.outcome, TxCompensationOutcome::CompensationFailed);
        prop_assert!(is_failed);
    }

    #[test]
    fn nothing_to_compensate_when_zero_committed(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_partial_commit_report(num_steps, 1);
        let comp_inputs = all_success_comp_inputs(num_steps);

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        let is_nothing = matches!(report.outcome, TxCompensationOutcome::NothingToCompensate);
        prop_assert!(is_nothing);
        prop_assert_eq!(report.compensated_count, 0);
    }

    #[test]
    fn receipt_sequence_monotonic(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_commit_report(num_steps);
        let comp_inputs = all_success_comp_inputs(num_steps);

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        let mut prev = 0u64;
        for receipt in &report.receipts {
            if let Some(seq) = receipt.get("seq").and_then(|v| v.as_u64()) {
                prop_assert!(seq > prev, "receipt seq {} must be > prev {}", seq, prev);
                prev = seq;
            }
        }
    }

    #[test]
    fn report_serde_roundtrip(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_commit_report(num_steps);
        let comp_inputs = all_success_comp_inputs(num_steps);

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        let json = serde_json::to_string(&report).unwrap();
        let restored: TxCompensationReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(report.outcome, restored.outcome);
        prop_assert_eq!(report.compensated_count, restored.compensated_count);
        prop_assert_eq!(report.failed_count, restored.failed_count);
        prop_assert_eq!(report.skipped_count, restored.skipped_count);
        prop_assert_eq!(report.reason_code, restored.reason_code);
        prop_assert_eq!(report.error_code, restored.error_code);
    }

    #[test]
    fn outcome_target_state_consistent(
        outcome in prop_oneof![
            Just(TxCompensationOutcome::FullyRolledBack),
            Just(TxCompensationOutcome::CompensationFailed),
            Just(TxCompensationOutcome::NothingToCompensate),
        ],
    ) {
        let state = outcome.target_tx_state();
        let is_valid = matches!(
            state,
            MissionTxState::Compensated | MissionTxState::Failed
        );
        prop_assert!(is_valid);
    }

    #[test]
    fn rejects_non_compensating_state(
        num_steps in 1usize..4,
        state in prop_oneof![
            Just(MissionTxState::Planned),
            Just(MissionTxState::Prepared),
            Just(MissionTxState::Committing),
            Just(MissionTxState::Committed),
        ],
    ) {
        let mut contract = make_contract_with_compensations(num_steps);
        contract.lifecycle_state = state;
        let commit_report = get_commit_report(num_steps);
        let comp_inputs = all_success_comp_inputs(num_steps);

        let result = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        );

        prop_assert!(result.is_err());
    }

    #[test]
    fn partial_commit_compensates_only_committed(
        num_steps in 3usize..8,
        fail_at in 2usize..8,
    ) {
        let fail_at = fail_at.min(num_steps);
        if fail_at <= 1 { return Ok(()); }
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_partial_commit_report(num_steps, fail_at);
        let comp_inputs = all_success_comp_inputs(num_steps);

        let expected_committed = fail_at - 1;

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        prop_assert_eq!(report.compensated_count, expected_committed);
        prop_assert!(report.is_fully_rolled_back());
    }

    #[test]
    fn missing_comp_input_treated_as_failure(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract_with_compensations(num_steps);
        let commit_report = get_commit_report(num_steps);
        // Provide no compensation inputs at all
        let comp_inputs: Vec<TxCompensationStepInput> = vec![];

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        // First missing input triggers failure, barrier skips the rest
        prop_assert_eq!(report.failed_count, 1);
        prop_assert_eq!(report.skipped_count, num_steps.saturating_sub(1));
        prop_assert!(report.has_residual_risk());
    }
}
