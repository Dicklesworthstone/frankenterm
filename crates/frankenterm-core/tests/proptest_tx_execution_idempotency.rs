//! Fuzz and metamorphic guards for tx execution idempotency at the connector
//! dispatch boundary.
//!
//! These tests model connector work as an external side effect recorded by the
//! `StepExecutor`. Replaying the same planned transaction with a populated
//! `IdempotencyStore` must not dispatch commit or compensation side effects
//! again, even when the replay returns a fail-closed dedup conflict.

use std::cell::RefCell;
use std::rc::Rc;

use frankenterm_core::plan::{
    MissionActorRole, MissionTxContract, MissionTxState, StepAction, TxCommitReport,
    TxCommitStepInput, TxCompensation, TxCompensationStepInput, TxId, TxIntent, TxOutcome, TxPlan,
    TxPlanId, TxPrepareGateInput, TxStep, TxStepId, mission_tx_commit_step_inputs,
    mission_tx_compensation_inputs, tx_prepare_gate_inputs_allow_all,
};
use frankenterm_core::tx_execution::{StepExecutor, TxExecutionConfig, TxExecutionEngine};
use frankenterm_core::tx_idempotency::{IdempotencyPolicy, IdempotencyStore};
use proptest::prelude::*;
use serde_json::json;

#[derive(Clone, Copy, Debug)]
enum StepKind {
    ConnectorDispatch,
    PaneSend,
    StoreData,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DispatchPhase {
    Commit,
    Compensation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DispatchEvent {
    phase: DispatchPhase,
    step_id: String,
    action_kind: String,
}

#[derive(Clone)]
struct RecordingConnectorExecutor {
    events: Rc<RefCell<Vec<DispatchEvent>>>,
}

impl RecordingConnectorExecutor {
    fn new() -> (Self, Rc<RefCell<Vec<DispatchEvent>>>) {
        let events = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                events: Rc::clone(&events),
            },
            events,
        )
    }
}

impl StepExecutor for RecordingConnectorExecutor {
    fn evaluate_gates(
        &self,
        contract: &MissionTxContract,
        _now_ms: i64,
    ) -> Vec<TxPrepareGateInput> {
        tx_prepare_gate_inputs_allow_all(contract)
    }

    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput> {
        for step in &contract.plan.steps {
            self.events.borrow_mut().push(DispatchEvent {
                phase: DispatchPhase::Commit,
                step_id: step.step_id.0.clone(),
                action_kind: action_kind(&step.action),
            });
            if fail_step == Some(step.step_id.0.as_str()) {
                break;
            }
        }
        mission_tx_commit_step_inputs(contract, fail_step, now_ms)
    }

    fn execute_compensations(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput> {
        for committed_step in commit_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_committed())
            .rev()
        {
            self.events.borrow_mut().push(DispatchEvent {
                phase: DispatchPhase::Compensation,
                step_id: committed_step.step_id.0.clone(),
                action_kind: compensation_action_kind(contract, &committed_step.step_id),
            });
            if fail_for_step == Some(committed_step.step_id.0.as_str()) {
                break;
            }
        }
        mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
    }
}

fn step_kind_strategy() -> impl Strategy<Value = StepKind> {
    prop_oneof![
        Just(StepKind::ConnectorDispatch),
        Just(StepKind::PaneSend),
        Just(StepKind::StoreData),
    ]
}

fn replay_plan_strategy(min_len: usize) -> impl Strategy<Value = Vec<StepKind>> {
    (min_len..7usize).prop_flat_map(|len| {
        (
            0usize..len,
            prop::collection::vec(step_kind_strategy(), len),
        )
            .prop_map(|(connector_index, mut kinds)| {
                if let Some(kind) = kinds.get_mut(connector_index) {
                    *kind = StepKind::ConnectorDispatch;
                }
                kinds
            })
    })
}

fn compensating_plan_strategy() -> impl Strategy<Value = (Vec<StepKind>, usize)> {
    (2usize..7usize).prop_flat_map(|len| {
        (
            1usize..len,
            prop::collection::vec(step_kind_strategy(), len),
        )
            .prop_map(|(fail_index, mut kinds)| {
                if let Some(kind) = kinds.first_mut() {
                    *kind = StepKind::ConnectorDispatch;
                }
                (kinds, fail_index)
            })
    })
}

fn contract_from_kinds(name: &str, kinds: &[StepKind]) -> MissionTxContract {
    let tx_id = TxId(format!("tx-{name}"));
    let steps = kinds
        .iter()
        .enumerate()
        .map(|(idx, kind)| TxStep {
            step_id: TxStepId(format!("step-{idx}")),
            ordinal: idx,
            action: action_for_kind(*kind, idx),
            description: format!("{name} step {idx}"),
        })
        .collect::<Vec<_>>();
    let compensations = steps
        .iter()
        .map(|step| TxCompensation {
            for_step_id: step.step_id.clone(),
            action: StepAction::Custom {
                action_type: "connector.compensate".to_string(),
                payload: json!({
                    "for_step_id": step.step_id.0.clone(),
                    "plan": name,
                }),
            },
        })
        .collect();

    MissionTxContract {
        tx_version: 1,
        intent: TxIntent {
            tx_id: tx_id.clone(),
            requested_by: MissionActorRole::Operator,
            summary: format!("{name} connector idempotency contract"),
            correlation_id: format!("corr-{name}"),
            created_at_ms: 1_000,
        },
        plan: TxPlan {
            plan_id: TxPlanId(format!("plan-{name}")),
            tx_id,
            steps,
            preconditions: Vec::new(),
            compensations,
        },
        lifecycle_state: MissionTxState::Planned,
        outcome: TxOutcome::Pending,
        receipts: Vec::new(),
    }
}

fn action_for_kind(kind: StepKind, idx: usize) -> StepAction {
    match kind {
        StepKind::ConnectorDispatch => StepAction::Custom {
            action_type: "connector.dispatch".to_string(),
            payload: json!({
                "connector": "metamorphic-test",
                "operation": format!("op-{idx}"),
            }),
        },
        StepKind::PaneSend => StepAction::SendText {
            pane_id: u64::try_from(idx).unwrap_or(u64::MAX),
            text: format!("send-{idx}"),
            paste_mode: None,
        },
        StepKind::StoreData => StepAction::StoreData {
            key: format!("key-{idx}"),
            value: json!({
                "idx": idx,
            }),
        },
    }
}

fn action_kind(action: &StepAction) -> String {
    match action {
        StepAction::Custom { action_type, .. } => action_type.clone(),
        StepAction::SendText { .. } => "pane.send_text".to_string(),
        StepAction::StoreData { .. } => "store_data".to_string(),
        StepAction::WaitFor { .. } => "wait_for".to_string(),
        StepAction::AcquireLock { .. } => "acquire_lock".to_string(),
        StepAction::ReleaseLock { .. } => "release_lock".to_string(),
        StepAction::RunWorkflow { .. } => "run_workflow".to_string(),
        StepAction::MarkEventHandled { .. } => "mark_event_handled".to_string(),
        StepAction::ValidateApproval { .. } => "validate_approval".to_string(),
        StepAction::NestedPlan { .. } => "nested_plan".to_string(),
    }
}

fn compensation_action_kind(contract: &MissionTxContract, step_id: &TxStepId) -> String {
    contract
        .plan
        .compensations
        .iter()
        .find(|compensation| compensation.for_step_id == *step_id)
        .map(|compensation| action_kind(&compensation.action))
        .unwrap_or_else(|| "missing_compensation".to_string())
}

fn tx_config(fail_step: Option<String>) -> TxExecutionConfig {
    TxExecutionConfig {
        produce_forensic_bundle: false,
        fail_step,
        ..TxExecutionConfig::default()
    }
}

fn engine_error(err: impl std::fmt::Display) -> TestCaseError {
    TestCaseError::fail(err.to_string())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn completed_commit_replay_does_not_redispatch_connector_side_effects(
        kinds in replay_plan_strategy(1),
    ) {
        let (executor, events) = RecordingConnectorExecutor::new();
        let engine = TxExecutionEngine::new(executor, tx_config(None));
        let store_dir = tempfile::tempdir().map_err(engine_error)?;
        let mut store = IdempotencyStore::open(store_dir.path(), IdempotencyPolicy::default())
            .map_err(engine_error)?;
        let mut first_contract = contract_from_kinds("commit-replay", &kinds);

        let first = engine
            .execute_with_store(&mut first_contract, &mut store, 10_000)
            .map_err(engine_error)?;
        prop_assert_eq!(first.final_state, MissionTxState::Committed);
        prop_assert_eq!(first.outcome, TxOutcome::Committed);

        let first_events = events.borrow().clone();
        prop_assert_eq!(
            first_events
                .iter()
                .filter(|event| event.phase == DispatchPhase::Commit)
                .count(),
            kinds.len()
        );
        prop_assert!(
            first_events
                .iter()
                .any(|event| event.action_kind == "connector.dispatch"),
            "generated plan must exercise connector dispatch"
        );

        let before_replay = first_events.len();
        let mut replay_contract = contract_from_kinds("commit-replay", &kinds);
        let replay = engine
            .execute_with_store(&mut replay_contract, &mut store, 10_100)
            .map_err(engine_error)?;
        prop_assert_eq!(replay.final_state, MissionTxState::Committed);
        prop_assert_eq!(replay.outcome, TxOutcome::Committed);

        let replay_events = events.borrow();
        let new_events = replay_events[before_replay..].to_vec();
        prop_assert!(
            new_events.is_empty(),
            "durable replay must not redispatch commit side effects: {new_events:?}"
        );
    }

    #[test]
    fn compensated_partial_replay_is_side_effect_safe_for_connector_dispatch(
        (kinds, fail_index) in compensating_plan_strategy(),
    ) {
        let (executor, events) = RecordingConnectorExecutor::new();
        let fail_step = format!("step-{fail_index}");
        let engine = TxExecutionEngine::new(executor, tx_config(Some(fail_step)));
        let store_dir = tempfile::tempdir().map_err(engine_error)?;
        let mut store = IdempotencyStore::open(store_dir.path(), IdempotencyPolicy::default())
            .map_err(engine_error)?;
        let mut first_contract = contract_from_kinds("compensating-replay", &kinds);

        let first = engine
            .execute_with_store(&mut first_contract, &mut store, 20_000)
            .map_err(engine_error)?;
        prop_assert_eq!(first.final_state, MissionTxState::RolledBack);
        prop_assert_eq!(first.outcome, TxOutcome::Compensated);
        let compensation_report = first
            .compensation_report
            .as_ref()
            .ok_or_else(|| TestCaseError::fail("missing compensation report"))?;
        prop_assert_eq!(compensation_report.compensated_count, fail_index);

        let first_events = events.borrow().clone();
        prop_assert_eq!(
            first_events
                .iter()
                .filter(|event| event.phase == DispatchPhase::Compensation)
                .count(),
            fail_index
        );
        prop_assert!(
            first_events
                .iter()
                .any(|event| event.action_kind == "connector.dispatch"),
            "generated plan must exercise connector dispatch before the failure boundary"
        );

        let before_replay = first_events.len();
        let mut replay_contract = contract_from_kinds("compensating-replay", &kinds);
        let replay = engine.execute_with_store(&mut replay_contract, &mut store, 20_100);
        if let Ok(result) = replay.as_ref() {
            prop_assert!(
                matches!(
                    result.final_state,
                    MissionTxState::RolledBack | MissionTxState::Committed
                ),
                "successful compensated replay must settle without a fresh side effect"
            );
        }

        let replay_events = events.borrow();
        let new_events = replay_events[before_replay..].to_vec();
        prop_assert!(
            new_events.is_empty(),
            "partial replay must fail closed or settle without redispatch: {new_events:?}"
        );
    }
}
