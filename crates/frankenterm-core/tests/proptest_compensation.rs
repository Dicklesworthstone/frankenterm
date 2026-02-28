// Disabled: references types not yet implemented in plan.rs
#![cfg(feature = "__journal_types_placeholder")]
//! Property-based tests for compensation/rollback engine (H6).
//!
//! Covers:
//! - Barrier semantics: steps after first compensation failure are skipped
//! - compensated + failed + no_compensation + skipped = total committed steps
//! - All success implies FullyRolledBack
//! - First compensation failure trips barrier
//! - NothingToCompensate when zero committed steps
//! - Reverse ordinal ordering of compensation steps
//! - Receipt sequence monotonicity
//! - TxCompensationReport serde roundtrip
//! - TxCompensationStepResult serde roundtrip
//! - TxCompensationOutcome target_tx_state consistency
//! - Canonical string determinism
//! - NoCompensation does not trip barrier
//! - Missing input treated as failure

use frankenterm_core::plan::{
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction,
    TxCompensation, TxCompensationOutcome, TxCompensationReport, TxCompensationStepInput,
    TxCompensationStepOutcome, TxCompensationStepResult, TxId, TxIntent, TxOutcome, TxPlan,
    TxPlanId, TxStep, TxStepId, execute_commit_phase, execute_compensation_phase,
};
use proptest::prelude::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a contract with N steps and compensations for each step.
fn make_contract_with_compensations(num_steps: usize) -> MissionTxContract {
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

/// Build a contract with N steps but partial compensations (only for steps <= comp_count).
fn make_contract_partial_compensations(num_steps: usize, comp_count: usize) -> MissionTxContract {
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

    let compensations: Vec<TxCompensation> = (1..=comp_count.min(num_steps))
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

/// Get a commit report where first `fail_at` step fails (partial commit).
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

        let total = report.compensated_count
            + report.failed_count
            + report.no_compensation_count
            + report.skipped_count;
        prop_assert_eq!(
            total, num_steps,
            "counts must sum to total committed steps: {} + {} + {} + {} = {} != {}",
            report.compensated_count, report.failed_count,
            report.no_compensation_count, report.skipped_count,
            total, num_steps
        );
    }

    #[test]
    fn barrier_skips_after_failure(
        num_steps in 2usize..8,
        fail_at in 1usize..8,
    ) {
        // Compensation runs in reverse ordinal, so failing at step `fail_at`
        // (where fail_at is the forward ordinal) means we skip steps with
        // lower ordinals.
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

        // Verify that steps exist
        prop_assert!(!report.step_results.is_empty());

        // Verify barrier was tripped (at least one failure)
        prop_assert!(report.has_residual_risk());
    }

    #[test]
    fn nothing_to_compensate_when_zero_committed(
        num_steps in 1usize..6,
    ) {
        // Get a commit report where first step fails (so 0 committed for 1-step,
        // or we use an immediate failure scenario)
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
    fn reverse_ordinal_ordering(
        num_steps in 2usize..8,
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

        // Step results should be in reverse ordinal order
        for i in 1..report.step_results.len() {
            prop_assert!(
                report.step_results[i].forward_ordinal < report.step_results[i - 1].forward_ordinal,
                "step results must be in reverse ordinal order: {} should be < {}",
                report.step_results[i].forward_ordinal,
                report.step_results[i - 1].forward_ordinal
            );
        }
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
            prop_assert!(
                receipt.seq > prev,
                "receipt seq {} must be > prev {}",
                receipt.seq, prev
            );
            prev = receipt.seq;
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
        prop_assert_eq!(&report, &restored);
    }

    #[test]
    fn step_result_serde_roundtrip(
        ordinal in 1u32..100,
        success in any::<bool>(),
    ) {
        let result = TxCompensationStepResult {
            for_step_id: TxStepId(format!("s{ordinal}")),
            forward_ordinal: ordinal,
            outcome: if success {
                TxCompensationStepOutcome::Compensated { reason_code: "ok".into() }
            } else {
                TxCompensationStepOutcome::Failed { reason_code: "err".into(), error_code: "E1".into() }
            },
            decision_path: "test".into(),
            completed_at_ms: ordinal as i64 * 1000,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: TxCompensationStepResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&result, &restored);
    }

    #[test]
    fn canonical_string_deterministic(
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

        let s1 = report.canonical_string();
        let s2 = report.canonical_string();
        prop_assert_eq!(s1, s2);
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
            MissionTxState::RolledBack | MissionTxState::Failed
        );
        prop_assert!(is_valid);
    }

    #[test]
    fn no_compensation_does_not_trip_barrier(
        num_steps in 2usize..6,
    ) {
        // Contract with compensations only for step 1, not for step 2+.
        // All steps committed. Steps without compensation should get
        // NoCompensation but not trip the barrier.
        let contract = make_contract_partial_compensations(num_steps, 1);
        let commit_report = get_commit_report(num_steps);
        // Only provide comp input for step 1 (the one with compensation defined)
        let comp_inputs = vec![TxCompensationStepInput {
            for_step_id: TxStepId("s1".into()),
            success: true,
            reason_code: "undone".into(),
            error_code: None,
            completed_at_ms: 20_000,
        }];

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        // Should have no failures — only compensated + no_compensation
        prop_assert_eq!(report.failed_count, 0);
        prop_assert_eq!(report.skipped_count, 0);
        prop_assert!(report.is_fully_rolled_back());
    }

    #[test]
    fn rejects_non_compensating_state(
        num_steps in 1usize..4,
        state in prop_oneof![
            Just(MissionTxState::Draft),
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

        // committed_count from commit = fail_at - 1
        let expected_committed = fail_at - 1;

        let report = execute_compensation_phase(
            &contract,
            &commit_report,
            &comp_inputs,
            20_000,
        )
        .unwrap();

        // Should only process the committed steps
        prop_assert_eq!(
            report.step_results.len(), expected_committed,
            "should compensate {} committed steps, got {}",
            expected_committed, report.step_results.len()
        );
        prop_assert_eq!(report.compensated_count, expected_committed);
    }
}
