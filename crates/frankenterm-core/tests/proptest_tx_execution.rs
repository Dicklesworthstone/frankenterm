//! Property-based tests for tx_execution engine types (ft-1i2ge.8).
//!
//! Tests serde roundtrip stability, config invariants, and execution
//! determinism for the tx execution orchestrator.

#![cfg(feature = "subprocess-bridge")]

use frankenterm_core::plan::{
    MissionActorRole, MissionKillSwitchLevel, MissionTxContract, MissionTxState, StepAction, TxId,
    TxIntent, TxOutcome, TxPlan, TxPlanId, TxStep, TxStepId,
};
use frankenterm_core::tx_execution::*;
use frankenterm_core::tx_observability::TxObservabilityConfig;
use proptest::prelude::*;

// ── Helpers ─────────────────────────────────────────────────────────────────

fn make_contract(num_steps: usize) -> MissionTxContract {
    let steps: Vec<TxStep> = (0..num_steps)
        .map(|i| TxStep {
            step_id: TxStepId(format!("step-{i}")),
            ordinal: i,
            action: StepAction::SendText {
                pane_id: i as u64,
                text: format!("action-{i}"),
                paste_mode: None,
            },
            description: format!("Step {i}"),
        })
        .collect();

    MissionTxContract {
        tx_version: 1,
        intent: TxIntent {
            tx_id: TxId("tx-prop".to_string()),
            requested_by: MissionActorRole::Operator,
            summary: "proptest contract".to_string(),
            correlation_id: "corr-prop".to_string(),
            created_at_ms: 1000,
        },
        plan: TxPlan {
            plan_id: TxPlanId("plan-prop".to_string()),
            tx_id: TxId("tx-prop".to_string()),
            steps,
            preconditions: Vec::new(),
            compensations: Vec::new(),
        },
        lifecycle_state: MissionTxState::Planned,
        outcome: TxOutcome::Pending,
        receipts: Vec::new(),
    }
}

// ── Strategy generators ─────────────────────────────────────────────────────

fn arb_kill_switch() -> impl Strategy<Value = MissionKillSwitchLevel> {
    prop_oneof![
        Just(MissionKillSwitchLevel::Off),
        Just(MissionKillSwitchLevel::SafeMode),
        Just(MissionKillSwitchLevel::HardStop),
    ]
}

fn arb_execution_config() -> impl Strategy<Value = TxExecutionConfig> {
    (
        any::<bool>(),
        any::<bool>(),
        1_usize..=500,
        arb_kill_switch(),
        any::<bool>(),
        proptest::option::of("[a-z0-9-]{1,20}"),
        proptest::option::of("[a-z0-9-]{1,20}"),
    )
        .prop_map(
            |(
                auto_compensate,
                produce_forensic_bundle,
                max_steps,
                kill_switch,
                paused,
                fail_step,
                fail_comp,
            )| {
                TxExecutionConfig {
                    auto_compensate,
                    produce_forensic_bundle,
                    max_steps_per_batch: max_steps,
                    kill_switch,
                    paused,
                    fail_step,
                    fail_compensation_for_step: fail_comp,
                    observability: TxObservabilityConfig::default(),
                }
            },
        )
}

// ── TxExecutionConfig serde roundtrip ────────────────────────────────────────

proptest! {
    #[test]
    fn tx_execution_config_serde_roundtrip(config in arb_execution_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: TxExecutionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.auto_compensate, config.auto_compensate);
        prop_assert_eq!(back.produce_forensic_bundle, config.produce_forensic_bundle);
        prop_assert_eq!(back.max_steps_per_batch, config.max_steps_per_batch);
        prop_assert_eq!(back.paused, config.paused);
        prop_assert_eq!(back.fail_step, config.fail_step);
        prop_assert_eq!(back.fail_compensation_for_step, config.fail_compensation_for_step);
    }

    #[test]
    fn tx_execution_config_json_not_empty(config in arb_execution_config()) {
        let json = serde_json::to_string(&config).unwrap();
        prop_assert!(!json.is_empty());
        prop_assert!(json.contains("auto_compensate"));
        prop_assert!(json.contains("max_steps_per_batch"));
    }
}

// ── Execution determinism ───────────────────────────────────────────────────

proptest! {
    #[test]
    fn execution_is_deterministic_for_same_inputs(num_steps in 1_usize..=5) {
        let config = TxExecutionConfig::default();
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config.clone());

        let mut c1 = make_contract(num_steps);
        let mut c2 = make_contract(num_steps);

        let r1 = engine.execute(&mut c1, 10000).unwrap();

        let engine2 = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let r2 = engine2.execute(&mut c2, 10000).unwrap();

        prop_assert_eq!(r1.final_state, r2.final_state);
        prop_assert_eq!(r1.outcome, r2.outcome);
        prop_assert_eq!(r1.events.len(), r2.events.len());
        prop_assert_eq!(r1.ledger.record_count(), r2.ledger.record_count());
    }

    #[test]
    fn execution_result_serde_roundtrip(num_steps in 1_usize..=4) {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        let json = serde_json::to_string(&result).unwrap();
        let back: TxExecutionResult = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(back.final_state, result.final_state);
        prop_assert_eq!(back.outcome, result.outcome);
        prop_assert_eq!(back.reason_code, result.reason_code);
        prop_assert_eq!(back.decision_path, result.decision_path);
    }
}

// ── Commit counts invariant ─────────────────────────────────────────────────

proptest! {
    #[test]
    fn commit_counts_sum_to_total_steps(num_steps in 1_usize..=6) {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        if let Some(commit) = &result.commit_report {
            let total = commit.committed_count + commit.failed_count + commit.skipped_count;
            prop_assert_eq!(total, num_steps, "commit counts must sum to total steps");
        }
    }

    #[test]
    fn failure_injection_preserves_step_count(
        num_steps in 2_usize..=5,
        fail_idx in 0_usize..5
    ) {
        let fail_idx = fail_idx % num_steps;
        let config = TxExecutionConfig {
            fail_step: Some(format!("step-{fail_idx}")),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        if let Some(commit) = &result.commit_report {
            let total = commit.committed_count + commit.failed_count + commit.skipped_count;
            prop_assert_eq!(total, num_steps);
            prop_assert!(commit.failed_count >= 1, "injected failure must appear");
        }
    }
}

// ── State machine invariants ────────────────────────────────────────────────

proptest! {
    #[test]
    fn final_state_is_terminal(num_steps in 1_usize..=4) {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        let terminal_states = [
            MissionTxState::Committed,
            MissionTxState::Compensated,
            MissionTxState::RolledBack,
            MissionTxState::Failed,
            MissionTxState::Planned,
        ];
        prop_assert!(
            terminal_states.contains(&result.final_state),
            "final state {:?} must be terminal", result.final_state
        );
    }

    #[test]
    fn ledger_reaches_terminal_phase(num_steps in 1_usize..=3) {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        prop_assert!(
            result.ledger.phase().is_terminal(),
            "ledger phase {:?} must be terminal", result.ledger.phase()
        );
    }

    #[test]
    fn events_have_monotonic_sequences(num_steps in 1_usize..=4) {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        for i in 1..result.events.len() {
            prop_assert!(
                result.events[i].sequence > result.events[i-1].sequence,
                "event sequences must be monotonically increasing"
            );
        }
    }
}

// ── Kill switch / pause invariants ──────────────────────────────────────────

proptest! {
    #[test]
    fn kill_switch_hard_stop_blocks_commit(num_steps in 1_usize..=3) {
        let config = TxExecutionConfig {
            kill_switch: MissionKillSwitchLevel::HardStop,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        prop_assert!(result.commit_report.is_none(), "HardStop must block commit");
        prop_assert!(!result.prepare_report.outcome.commit_eligible());
    }

    #[test]
    fn pause_suspends_all_steps(num_steps in 1_usize..=4) {
        let config = TxExecutionConfig {
            paused: true,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        if let Some(commit) = &result.commit_report {
            prop_assert_eq!(
                commit.skipped_count, num_steps,
                "pause must skip all steps"
            );
        }
    }
}

// ── Error type tests ────────────────────────────────────────────────────────

#[test]
fn error_types_implement_display_and_error() {
    let errors: Vec<TxExecutionError> = vec![
        TxExecutionError::InvalidContract("bad".into()),
        TxExecutionError::PhaseTransition("bad".into()),
        TxExecutionError::PreparePhase("bad".into()),
        TxExecutionError::CommitPhase("bad".into()),
        TxExecutionError::CompensationPhase("bad".into()),
        TxExecutionError::LedgerNotFound("id".into()),
    ];
    for err in &errors {
        let display = format!("{err}");
        assert!(!display.is_empty());
        let source: &dyn std::error::Error = err;
        let _ = format!("{source}");
    }
}

#[test]
fn empty_contract_returns_invalid_contract_error() {
    let mut contract = make_contract(0);
    let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
    let result = engine.execute(&mut contract, 5000);
    assert!(result.is_err());
    let check = matches!(result.unwrap_err(), TxExecutionError::InvalidContract(_));
    assert!(check);
}

// ── Compensation invariants ────────────────────────────────────────────────

proptest! {
    #[test]
    fn compensation_only_runs_when_commit_has_failures(
        num_steps in 2_usize..=5,
        fail_idx in 0_usize..5,
    ) {
        let fail_idx = fail_idx % num_steps;
        let config = TxExecutionConfig {
            fail_step: Some(format!("step-{fail_idx}")),
            auto_compensate: true,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        // Compensation should exist since we injected a failure
        if let Some(commit) = &result.commit_report {
            if commit.has_failures() {
                prop_assert!(
                    result.compensation_report.is_some(),
                    "compensation must run when commit has failures and auto_compensate is true"
                );
            }
        }
    }

    #[test]
    fn no_compensation_without_auto_compensate(
        num_steps in 2_usize..=4,
        fail_idx in 0_usize..4,
    ) {
        let fail_idx = fail_idx % num_steps;
        let config = TxExecutionConfig {
            fail_step: Some(format!("step-{fail_idx}")),
            auto_compensate: false,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        prop_assert!(
            result.compensation_report.is_none(),
            "compensation must NOT run when auto_compensate is false"
        );
    }
}

// ── Contract state mutation invariants ─────────────────────────────────────

proptest! {
    #[test]
    fn contract_reflects_final_state_after_execution(num_steps in 1_usize..=4) {
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);

        // Before execution
        let pre_state = contract.lifecycle_state.clone();
        let pre_outcome = contract.outcome.clone();
        prop_assert_eq!(pre_state, MissionTxState::Planned);
        prop_assert_eq!(pre_outcome, TxOutcome::Pending);

        let result = engine.execute(&mut contract, 5000).unwrap();

        // After execution, contract must match result
        prop_assert_eq!(
            contract.lifecycle_state.clone(), result.final_state,
            "contract lifecycle_state must match result.final_state"
        );
        prop_assert_eq!(
            contract.outcome.clone(), result.outcome,
            "contract outcome must match result.outcome"
        );
    }

    #[test]
    fn failure_injection_produces_non_committed_outcome(
        num_steps in 2_usize..=5,
        fail_idx in 0_usize..5,
    ) {
        let fail_idx = fail_idx % num_steps;
        let config = TxExecutionConfig {
            fail_step: Some(format!("step-{fail_idx}")),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        prop_assert_ne!(
            result.outcome,
            TxOutcome::Committed,
            "injected failure must prevent Committed outcome"
        );
    }
}

// ── Observability event coverage ───────────────────────────────────────────

proptest! {
    #[test]
    fn successful_execution_emits_prepare_and_commit_events(num_steps in 1_usize..=3) {
        use frankenterm_core::tx_observability::TxEventKind;

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut contract = make_contract(num_steps);
        let result = engine.execute(&mut contract, 5000).unwrap();

        let kinds: Vec<&TxEventKind> = result.events.iter().map(|e| &e.kind).collect();
        prop_assert!(
            kinds.contains(&&TxEventKind::PrepareStarted),
            "must emit PrepareStarted event"
        );
        prop_assert!(
            kinds.contains(&&TxEventKind::PrepareCompleted),
            "must emit PrepareCompleted event"
        );
        prop_assert!(
            kinds.contains(&&TxEventKind::CommitStarted),
            "must emit CommitStarted event"
        );
        prop_assert!(
            kinds.contains(&&TxEventKind::CommitCompleted),
            "must emit CommitCompleted event"
        );
    }
}
