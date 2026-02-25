//! Property-based tests for commit-phase executor (H5).
//!
//! Covers:
//! - Barrier semantics: steps after first failure are always skipped
//! - committed + failed + skipped = total steps
//! - Kill-switch blocks all steps (committed_count = 0)
//! - Pause suspends all steps (committed_count = 0)
//! - Full success implies FullyCommitted outcome
//! - First-step failure implies ImmediateFailure
//! - Partial failure implies PartialFailure with correct boundary
//! - Receipt sequence monotonicity
//! - TxCommitReport serde roundtrip
//! - TxCommitStepResult serde roundtrip
//! - TxCommitStepOutcome tag_name determinism
//! - TxCommitOutcome target_tx_state consistency
//! - Canonical string determinism
//! - Step results ordering matches plan ordinal

use frankenterm_core::plan::{
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction,
    TxCommitOutcome, TxCommitReport, TxCommitStepInput, TxCommitStepOutcome, TxCommitStepResult,
    TxId, TxIntent, TxOutcome, TxPlan, TxPlanId, TxStep, TxStepId, execute_commit_phase,
};
use proptest::prelude::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn make_contract(num_steps: usize) -> MissionTxContract {
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

    MissionTxContract {
        tx_version: 1,
        intent: TxIntent {
            tx_id: TxId("tx:prop".into()),
            requested_by: MissionActorRole::Dispatcher,
            summary: "proptest".into(),
            correlation_id: "pt-1".into(),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan:prop".into()),
            tx_id: TxId("tx:prop".into()),
            steps,
            preconditions: vec![],
            compensations: vec![],
        },
        lifecycle_state: MissionTxState::Prepared,
        outcome: TxOutcome::Pending,
        receipts: vec![],
    }
}

fn all_success_inputs(num_steps: usize) -> Vec<TxCommitStepInput> {
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

fn inputs_with_failure_at(num_steps: usize, fail_at: usize) -> Vec<TxCommitStepInput> {
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

// ── Properties ──────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn barrier_skips_after_failure(
        num_steps in 2usize..8,
        fail_at in 1usize..8,
    ) {
        let fail_at = fail_at.min(num_steps); // clamp to valid range
        let contract = make_contract(num_steps);
        let inputs = inputs_with_failure_at(num_steps, fail_at);

        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        // All steps after fail_at should be skipped
        for result in &report.step_results {
            if result.ordinal > fail_at as u32 {
                prop_assert!(result.outcome.is_skipped(),
                    "step ordinal {} should be skipped after failure at {}",
                    result.ordinal, fail_at);
            }
        }
    }

    #[test]
    fn counts_sum_to_total_steps(
        num_steps in 1usize..10,
        fail_at_opt in 0usize..10,
    ) {
        let contract = make_contract(num_steps);
        let inputs = if fail_at_opt > 0 && fail_at_opt <= num_steps {
            inputs_with_failure_at(num_steps, fail_at_opt)
        } else {
            all_success_inputs(num_steps)
        };

        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let total = report.committed_count + report.failed_count + report.skipped_count;
        prop_assert_eq!(total, num_steps, "counts must sum to total steps");
    }

    #[test]
    fn kill_switch_blocks_all(
        num_steps in 1usize..6,
        level in prop_oneof![
            Just(MissionKillSwitchLevel::SafeMode),
            Just(MissionKillSwitchLevel::HardStop),
        ],
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            level,
            false,
            10_000,
        )
        .unwrap();

        prop_assert_eq!(report.committed_count, 0);
        let is_blocked = matches!(report.outcome, TxCommitOutcome::KillSwitchBlocked);
        prop_assert!(is_blocked);
    }

    #[test]
    fn pause_suspends_all(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            true,
            10_000,
        )
        .unwrap();

        prop_assert_eq!(report.committed_count, 0);
        let is_paused = matches!(report.outcome, TxCommitOutcome::PauseSuspended);
        prop_assert!(is_paused);
    }

    #[test]
    fn all_success_implies_fully_committed(
        num_steps in 1usize..10,
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        prop_assert!(report.is_fully_committed());
        prop_assert_eq!(report.committed_count, num_steps);
        prop_assert!(!report.has_failures());
    }

    #[test]
    fn first_step_failure_is_immediate(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract(num_steps);
        let inputs = inputs_with_failure_at(num_steps, 1);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let is_immediate = matches!(report.outcome, TxCommitOutcome::ImmediateFailure);
        prop_assert!(is_immediate);
        prop_assert_eq!(report.failure_boundary, Some(1));
    }

    #[test]
    fn mid_failure_is_partial(
        num_steps in 3usize..8,
        fail_at in 2usize..8,
    ) {
        let fail_at = fail_at.min(num_steps); // clamp
        if fail_at <= 1 { return Ok(()); } // skip if clamped to 1
        let contract = make_contract(num_steps);
        let inputs = inputs_with_failure_at(num_steps, fail_at);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let is_partial = matches!(report.outcome, TxCommitOutcome::PartialFailure);
        prop_assert!(is_partial);
        prop_assert_eq!(report.failure_boundary, Some(fail_at as u32));
        prop_assert_eq!(report.committed_count, fail_at - 1);
    }

    #[test]
    fn receipt_sequence_monotonic(
        num_steps in 1usize..6,
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let mut prev = 0u64;
        for receipt in &report.receipts {
            prop_assert!(receipt.seq > prev, "receipt seq {} must be > prev {}", receipt.seq, prev);
            prev = receipt.seq;
        }
    }

    #[test]
    fn report_serde_roundtrip(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let json = serde_json::to_string(&report).unwrap();
        let restored: TxCommitReport = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&report, &restored);
    }

    #[test]
    fn step_result_serde_roundtrip(
        ordinal in 1u32..100,
        success in any::<bool>(),
    ) {
        let result = TxCommitStepResult {
            step_id: TxStepId(format!("s{ordinal}")),
            ordinal,
            outcome: if success {
                TxCommitStepOutcome::Committed { reason_code: "ok".into() }
            } else {
                TxCommitStepOutcome::Failed { reason_code: "err".into(), error_code: "E1".into() }
            },
            decision_path: "test".into(),
            completed_at_ms: ordinal as i64 * 1000,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: TxCommitStepResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&result, &restored);
    }

    #[test]
    fn canonical_string_deterministic(
        num_steps in 1usize..5,
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        let s1 = report.canonical_string();
        let s2 = report.canonical_string();
        prop_assert_eq!(s1, s2);
    }

    #[test]
    fn step_results_match_plan_ordinals(
        num_steps in 1usize..8,
    ) {
        let contract = make_contract(num_steps);
        let inputs = all_success_inputs(num_steps);
        let report = execute_commit_phase(
            &contract,
            &inputs,
            MissionKillSwitchLevel::Off,
            false,
            10_000,
        )
        .unwrap();

        prop_assert_eq!(report.step_results.len(), num_steps);
        for (i, result) in report.step_results.iter().enumerate() {
            prop_assert_eq!(result.ordinal, (i + 1) as u32);
        }
    }

    #[test]
    fn outcome_target_state_consistent(
        outcome in prop_oneof![
            Just(TxCommitOutcome::FullyCommitted),
            Just(TxCommitOutcome::PartialFailure),
            Just(TxCommitOutcome::ImmediateFailure),
            Just(TxCommitOutcome::KillSwitchBlocked),
            Just(TxCommitOutcome::PauseSuspended),
        ],
    ) {
        let state = outcome.target_tx_state();
        // All outcomes must map to a valid MissionTxState
        let is_valid = matches!(
            state,
            MissionTxState::Committed | MissionTxState::Compensating
            | MissionTxState::Failed | MissionTxState::Committing
        );
        prop_assert!(is_valid);
    }
}
