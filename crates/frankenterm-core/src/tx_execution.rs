//! Transaction execution engine (ft-1i2ge.8).
//!
//! Orchestrates the full tx lifecycle: plan → prepare → commit → compensate,
//! tying together the plan compiler, idempotency ledger, and observability pipeline.
//!
//! # Architecture
//!
//! ```text
//! TxPlan ──────┐
//!              ├──> TxExecutionEngine::execute() ──> TxExecutionResult
//! StepExecutor ┤                                     ├─ ledger
//!              │                                     ├─ events
//! Config ──────┘                                     └─ forensic bundle
//! ```
//!
//! Safety doctrine: no commit before prepare; no prepare bypass of policy gates;
//! every transition emits observability events with reason codes.

use crate::plan::{
    MissionKillSwitchLevel, MissionTxContract, MissionTxState, TxCommitReport, TxCommitStepInput,
    TxCompensationReport, TxCompensationStepInput, TxOutcome, TxPrepareGateInput, TxPrepareOutcome,
    TxPrepareReport, evaluate_prepare_phase, execute_commit_phase, execute_compensation_phase,
};
use crate::tx_idempotency::{
    IdempotencyKey, IdempotencyStore, ResumeRecommendation, StepOutcome, TxExecutionLedger, TxPhase,
};
use crate::tx_observability::{
    TxEventKind, TxForensicBundle, TxObservabilityConfig, TxObservabilityEvent,
    TxObservabilityPhase,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ── Configuration ────────────────────────────────────────────────────────────

/// Configuration for the tx execution engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxExecutionConfig {
    /// Whether to auto-trigger compensation on partial failure.
    pub auto_compensate: bool,
    /// Whether to produce a forensic bundle after execution.
    pub produce_forensic_bundle: bool,
    /// Maximum number of steps to execute before pausing for safety.
    pub max_steps_per_batch: usize,
    /// Kill switch level for the entire execution.
    pub kill_switch: MissionKillSwitchLevel,
    /// Whether execution is paused (commit phase suspended).
    pub paused: bool,
    /// Optional step ID to inject a failure at (for testing/chaos).
    pub fail_step: Option<String>,
    /// Optional step ID to inject a compensation failure at (for testing/chaos).
    pub fail_compensation_for_step: Option<String>,
    /// Observability configuration.
    pub observability: TxObservabilityConfig,
}

impl Default for TxExecutionConfig {
    fn default() -> Self {
        Self {
            auto_compensate: true,
            produce_forensic_bundle: true,
            max_steps_per_batch: 1000,
            kill_switch: MissionKillSwitchLevel::Off,
            paused: false,
            fail_step: None,
            fail_compensation_for_step: None,
            observability: TxObservabilityConfig::default(),
        }
    }
}

// ── Step Executor Trait ──────────────────────────────────────────────────────

/// Trait for executing individual tx steps.
///
/// The engine calls this to perform actual work (e.g., sending commands to panes,
/// acquiring reservations, evaluating policies). The default synthetic implementation
/// uses deterministic inputs for testing.
pub trait StepExecutor {
    /// Evaluate prepare-phase gates for all steps.
    fn evaluate_gates(&self, contract: &MissionTxContract) -> Vec<TxPrepareGateInput>;

    /// Execute commit-phase steps and return inputs.
    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput>;

    /// Execute compensation steps and return inputs.
    fn execute_compensations(
        &self,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput>;
}

/// Synthetic step executor that produces deterministic results for testing.
pub struct SyntheticStepExecutor;

impl StepExecutor for SyntheticStepExecutor {
    fn evaluate_gates(&self, contract: &MissionTxContract) -> Vec<TxPrepareGateInput> {
        crate::plan::mission_tx_prepare_gate_inputs(contract)
    }

    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput> {
        crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
    }

    fn execute_compensations(
        &self,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput> {
        crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
    }
}

// ── Execution Result ─────────────────────────────────────────────────────────

/// Complete result from a tx execution run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxExecutionResult {
    /// Final lifecycle state of the contract.
    pub final_state: MissionTxState,
    /// Final transaction outcome.
    pub outcome: TxOutcome,
    /// Prepare phase report.
    pub prepare_report: TxPrepareReport,
    /// Commit phase report (None if prepare was denied/deferred).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_report: Option<TxCommitReport>,
    /// Compensation report (None if no compensation was needed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compensation_report: Option<TxCompensationReport>,
    /// Observability events emitted during execution.
    pub events: Vec<TxObservabilityEvent>,
    /// The execution ledger.
    pub ledger: TxExecutionLedger,
    /// Forensic bundle (None if not requested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forensic_bundle: Option<TxForensicBundle>,
    /// Decision path trace for the overall execution.
    pub decision_path: String,
    /// Reason code summarizing the execution.
    pub reason_code: String,
}

// ── Engine ───────────────────────────────────────────────────────────────────

/// The tx execution engine orchestrates the full lifecycle of a mission transaction.
///
/// Given a `MissionTxContract` and a `StepExecutor`, it runs:
/// 1. **Prepare**: Evaluate gates (policy, reservation, approval, liveness)
/// 2. **Commit**: Execute steps in plan order with failure boundary semantics
/// 3. **Compensate**: Roll back committed steps on partial failure
///
/// Each phase transition is recorded in the idempotency ledger and emits
/// structured observability events.
pub struct TxExecutionEngine<E: StepExecutor> {
    executor: E,
    config: TxExecutionConfig,
    event_seq: std::cell::Cell<u64>,
}

impl<E: StepExecutor> TxExecutionEngine<E> {
    /// Create a new execution engine.
    #[must_use]
    pub fn new(executor: E, config: TxExecutionConfig) -> Self {
        Self {
            executor,
            config,
            event_seq: std::cell::Cell::new(0),
        }
    }

    /// Execute the full tx lifecycle on the given contract.
    ///
    /// # Errors
    ///
    /// Returns an error if the contract is invalid or a phase transition fails.
    pub fn execute(
        &self,
        contract: &mut MissionTxContract,
        now_ms: i64,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        contract
            .validate()
            .map_err(TxExecutionError::InvalidContract)?;

        let execution_id = format!("txe-{now_ms}");
        let plan_id = contract.plan.plan_id.0.clone();
        let mut ledger = TxExecutionLedger::new(&execution_id, &plan_id, 0);
        let mut events: Vec<TxObservabilityEvent> = Vec::new();
        let mut decision_path = String::new();

        // Phase 1: Prepare
        let prepare_report = self.run_prepare_phase(
            contract,
            &execution_id,
            &mut events,
            &mut decision_path,
            now_ms,
        )?;

        if !prepare_report.outcome.commit_eligible() {
            let final_state = match prepare_report.outcome {
                TxPrepareOutcome::Denied => MissionTxState::Failed,
                _ => MissionTxState::Planned,
            };
            contract.lifecycle_state = final_state;
            contract.outcome = match final_state {
                MissionTxState::Failed => TxOutcome::Failed,
                _ => TxOutcome::Pending,
            };
            decision_path.push_str("->prepare_not_eligible");

            return Ok(TxExecutionResult {
                final_state,
                outcome: contract.outcome.clone(),
                prepare_report,
                commit_report: None,
                compensation_report: None,
                events,
                ledger,
                forensic_bundle: None,
                decision_path,
                reason_code: "prepare_not_eligible".to_string(),
            });
        }

        // Transition: Planned → Prepared → Committing
        contract.lifecycle_state = MissionTxState::Prepared;
        ledger
            .transition_phase(TxPhase::Preparing)
            .map_err(|e| TxExecutionError::PhaseTransition(e.to_string()))?;
        ledger
            .transition_phase(TxPhase::Committing)
            .map_err(|e| TxExecutionError::PhaseTransition(e.to_string()))?;

        // Phase 2: Commit
        contract.lifecycle_state = MissionTxState::Committing;
        let commit_report = self.run_commit_phase(
            contract,
            &execution_id,
            &mut events,
            &mut decision_path,
            now_ms,
        )?;

        let commit_outcome_state = commit_report.outcome.target_tx_state();
        contract.lifecycle_state = commit_outcome_state;

        // Record commit step results in the ledger
        self.record_commit_results_to_ledger(
            contract,
            &commit_report,
            &execution_id,
            &mut ledger,
            &mut events,
            now_ms,
        );

        // Phase 3: Compensate (if needed)
        let compensation_report = if commit_report.has_failures() && self.config.auto_compensate {
            contract.lifecycle_state = MissionTxState::Compensating;
            ledger
                .transition_phase(TxPhase::Compensating)
                .map_err(|e| TxExecutionError::PhaseTransition(e.to_string()))?;

            let comp = self.run_compensation_phase(
                contract,
                &commit_report,
                &execution_id,
                &mut events,
                &mut decision_path,
                now_ms,
            )?;

            let comp_state = comp.outcome.target_tx_state();
            contract.lifecycle_state = comp_state;

            self.record_compensation_results_to_ledger(
                contract,
                &comp,
                &execution_id,
                &mut ledger,
                &mut events,
                now_ms,
            );

            Some(comp)
        } else {
            None
        };

        // Determine final outcome
        let (final_state, outcome) = self.determine_final_outcome(
            contract.lifecycle_state,
            &commit_report,
            compensation_report.as_ref(),
        );
        contract.lifecycle_state = final_state;
        contract.outcome = outcome.clone();
        decision_path.push_str(&format!("->final:{final_state}"));

        // Transition ledger to terminal phase
        let terminal_phase = if final_state == MissionTxState::Committed
            || final_state == MissionTxState::Compensated
            || final_state == MissionTxState::RolledBack
        {
            TxPhase::Completed
        } else {
            TxPhase::Aborted
        };
        let _ = ledger.transition_phase(terminal_phase);

        // Emit completion event
        events.push(self.make_event(
            TxEventKind::CommitCompleted,
            TxObservabilityPhase::Commit,
            &format!("tx.execution.{}", reason_code_for_outcome(&outcome)),
            &execution_id,
            &plan_id,
            ledger.phase(),
            now_ms,
        ));

        Ok(TxExecutionResult {
            final_state,
            outcome,
            prepare_report,
            commit_report: Some(commit_report),
            compensation_report,
            events,
            ledger,
            forensic_bundle: None,
            decision_path,
            reason_code: format!("execution_{final_state}"),
        })
    }

    /// Resume execution from a persisted ledger.
    pub fn resume(
        &self,
        contract: &mut MissionTxContract,
        store: &IdempotencyStore,
        execution_id: &str,
        now_ms: i64,
    ) -> Result<TxExecutionResult, TxExecutionError> {
        let ledger = store
            .get_ledger(execution_id)
            .ok_or_else(|| TxExecutionError::LedgerNotFound(execution_id.to_string()))?;

        let compiled_plan = crate::tx_plan_compiler::TxPlan {
            plan_id: contract.plan.plan_id.0.clone(),
            plan_hash: 0,
            steps: Vec::new(),
            execution_order: Vec::new(),
            parallel_levels: Vec::new(),
            risk_summary: crate::tx_plan_compiler::TxRiskSummary {
                total_steps: contract.plan.steps.len(),
                high_risk_count: 0,
                critical_risk_count: 0,
                uncompensated_steps: 0,
                overall_risk: crate::tx_plan_compiler::StepRisk::Low,
            },
            rejected_edges: Vec::new(),
        };

        let resume_ctx = crate::tx_idempotency::ResumeContext::from_ledger(ledger, &compiled_plan);
        let mut events = Vec::new();

        events.push(self.make_event(
            TxEventKind::ResumeContextBuilt,
            TxObservabilityPhase::Resume,
            "tx.resume.context_built",
            execution_id,
            &contract.plan.plan_id.0,
            ledger.phase(),
            now_ms,
        ));

        match resume_ctx.recommendation {
            ResumeRecommendation::AlreadyComplete => Ok(TxExecutionResult {
                final_state: contract.lifecycle_state,
                outcome: contract.outcome.clone(),
                prepare_report: TxPrepareReport {
                    outcome: TxPrepareOutcome::AllReady,
                },
                commit_report: None,
                compensation_report: None,
                events,
                ledger: ledger.clone(),
                forensic_bundle: None,
                decision_path: "resume->already_complete".to_string(),
                reason_code: "already_complete".to_string(),
            }),
            ResumeRecommendation::RestartFresh => {
                contract.lifecycle_state = MissionTxState::Planned;
                contract.outcome = TxOutcome::Pending;
                events.push(self.make_event(
                    TxEventKind::ResumeExecuted,
                    TxObservabilityPhase::Resume,
                    "tx.resume.restart_fresh",
                    execution_id,
                    &contract.plan.plan_id.0,
                    ledger.phase(),
                    now_ms,
                ));
                self.execute(contract, now_ms)
            }
            ResumeRecommendation::CompensateAndAbort => {
                contract.lifecycle_state = MissionTxState::Compensating;
                contract.outcome = TxOutcome::Pending;
                self.execute(contract, now_ms)
            }
            ResumeRecommendation::ContinueFromCheckpoint => self.execute(contract, now_ms),
        }
    }

    // ── Phase Runners ────────────────────────────────────────────────────────

    fn run_prepare_phase(
        &self,
        contract: &MissionTxContract,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        decision_path: &mut String,
        now_ms: i64,
    ) -> Result<TxPrepareReport, TxExecutionError> {
        events.push(self.make_event(
            TxEventKind::PrepareStarted,
            TxObservabilityPhase::Prepare,
            "tx.prepare.started",
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Preparing,
            now_ms,
        ));

        let gate_inputs = self.executor.evaluate_gates(contract);

        let report = evaluate_prepare_phase(
            &contract.intent.tx_id,
            &contract.plan,
            &gate_inputs,
            self.config.kill_switch,
            now_ms,
        )
        .map_err(TxExecutionError::PreparePhase)?;

        let reason = match &report.outcome {
            TxPrepareOutcome::AllReady => "tx.prepare.all_ready",
            TxPrepareOutcome::Denied => "tx.prepare.denied",
            TxPrepareOutcome::Deferred => "tx.prepare.deferred",
        };

        events.push(self.make_event(
            TxEventKind::PrepareCompleted,
            TxObservabilityPhase::Prepare,
            reason,
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Preparing,
            now_ms,
        ));

        decision_path.push_str(&format!("prepare({:?})", report.outcome));
        Ok(report)
    }

    fn run_commit_phase(
        &self,
        contract: &MissionTxContract,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        decision_path: &mut String,
        now_ms: i64,
    ) -> Result<TxCommitReport, TxExecutionError> {
        events.push(self.make_event(
            TxEventKind::CommitStarted,
            TxObservabilityPhase::Commit,
            "tx.commit.started",
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Committing,
            now_ms,
        ));

        let commit_inputs =
            self.executor
                .execute_steps(contract, self.config.fail_step.as_deref(), now_ms);

        let report = execute_commit_phase(
            contract,
            &commit_inputs,
            self.config.kill_switch,
            self.config.paused,
            now_ms,
        )
        .map_err(TxExecutionError::CommitPhase)?;

        decision_path.push_str(&format!("->commit({:?})", report.outcome));
        Ok(report)
    }

    fn run_compensation_phase(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        decision_path: &mut String,
        now_ms: i64,
    ) -> Result<TxCompensationReport, TxExecutionError> {
        events.push(self.make_event(
            TxEventKind::CompensationStarted,
            TxObservabilityPhase::Compensate,
            "tx.compensation.started",
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Compensating,
            now_ms,
        ));

        let comp_inputs = self.executor.execute_compensations(
            commit_report,
            self.config.fail_compensation_for_step.as_deref(),
            now_ms,
        );

        let report = execute_compensation_phase(contract, commit_report, &comp_inputs, now_ms)
            .map_err(TxExecutionError::CompensationPhase)?;

        let reason = match &report.outcome {
            crate::plan::TxCompensationOutcome::FullyRolledBack => {
                "tx.compensation.fully_rolled_back"
            }
            crate::plan::TxCompensationOutcome::CompensationFailed => "tx.compensation.failed",
            crate::plan::TxCompensationOutcome::NothingToCompensate => {
                "tx.compensation.nothing_to_compensate"
            }
        };

        events.push(self.make_event(
            TxEventKind::CompensationCompleted,
            TxObservabilityPhase::Compensate,
            reason,
            execution_id,
            &contract.plan.plan_id.0,
            TxPhase::Compensating,
            now_ms,
        ));

        decision_path.push_str(&format!("->compensate({:?})", report.outcome));
        Ok(report)
    }

    // ── Ledger Recording ─────────────────────────────────────────────────────

    fn record_commit_results_to_ledger(
        &self,
        contract: &MissionTxContract,
        commit_report: &TxCommitReport,
        execution_id: &str,
        ledger: &mut TxExecutionLedger,
        events: &mut Vec<TxObservabilityEvent>,
        now_ms: i64,
    ) {
        for step_result in &commit_report.step_results {
            let idem_key =
                IdempotencyKey::new(&contract.plan.plan_id.0, &step_result.step_id.0, "commit");

            if ledger.is_executed(&idem_key) {
                continue;
            }

            let outcome = match &step_result.outcome {
                crate::plan::TxCommitStepOutcome::Committed { reason_code } => {
                    StepOutcome::Success {
                        result: Some(reason_code.clone()),
                    }
                }
                crate::plan::TxCommitStepOutcome::Failed { reason_code } => StepOutcome::Failed {
                    error_code: reason_code.clone(),
                    error_message: format!("Step {} failed", step_result.step_id.0),
                    compensated: false,
                },
                crate::plan::TxCommitStepOutcome::Skipped { reason_code } => StepOutcome::Skipped {
                    reason: reason_code.clone(),
                },
            };

            let _ = ledger.append(
                idem_key,
                outcome,
                crate::tx_plan_compiler::StepRisk::Low,
                &format!("agent-{}", step_result.step_id.0),
                now_ms as u64,
            );

            let event_kind = if step_result.outcome.is_committed() {
                TxEventKind::StepCommitted
            } else {
                TxEventKind::StepFailed
            };

            events.push(self.make_event(
                event_kind,
                TxObservabilityPhase::Commit,
                &format!(
                    "tx.commit.step_{}",
                    if step_result.outcome.is_committed() {
                        "committed"
                    } else {
                        "failed"
                    }
                ),
                execution_id,
                &contract.plan.plan_id.0,
                TxPhase::Committing,
                now_ms,
            ));
        }
    }

    fn record_compensation_results_to_ledger(
        &self,
        contract: &MissionTxContract,
        comp_report: &TxCompensationReport,
        execution_id: &str,
        ledger: &mut TxExecutionLedger,
        events: &mut Vec<TxObservabilityEvent>,
        now_ms: i64,
    ) {
        for receipt in &comp_report.receipts {
            if let Some(step_id) = receipt.get("step_id").and_then(|v| v.as_str()) {
                let idem_key =
                    IdempotencyKey::for_compensation(&contract.plan.plan_id.0, step_id, "rollback");

                if ledger.is_executed(&idem_key) {
                    continue;
                }

                let outcome_str = receipt
                    .get("outcome")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let outcome = if outcome_str == "compensated" {
                    StepOutcome::Compensated {
                        original_outcome: Box::new(StepOutcome::Failed {
                            error_code: "compensated".to_string(),
                            error_message: "Compensated after failure".to_string(),
                            compensated: true,
                        }),
                        compensation_result: "rollback_complete".to_string(),
                    }
                } else {
                    StepOutcome::Failed {
                        error_code: "compensation_failed".to_string(),
                        error_message: format!("Compensation for step {step_id} failed"),
                        compensated: false,
                    }
                };

                let _ = ledger.append(
                    idem_key,
                    outcome,
                    crate::tx_plan_compiler::StepRisk::Low,
                    &format!("agent-{step_id}"),
                    now_ms as u64,
                );

                events.push(self.make_event(
                    TxEventKind::StepCompensated,
                    TxObservabilityPhase::Compensate,
                    &format!("tx.compensate.step_{outcome_str}"),
                    execution_id,
                    &contract.plan.plan_id.0,
                    TxPhase::Compensating,
                    now_ms,
                ));
            }
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    #[allow(clippy::unused_self)]
    fn determine_final_outcome(
        &self,
        current_state: MissionTxState,
        commit_report: &TxCommitReport,
        compensation_report: Option<&TxCompensationReport>,
    ) -> (MissionTxState, TxOutcome) {
        if commit_report.is_fully_committed() {
            return (MissionTxState::Committed, TxOutcome::Committed);
        }

        if let Some(comp) = compensation_report {
            if comp.is_fully_rolled_back() {
                return (MissionTxState::RolledBack, TxOutcome::Compensated);
            }
            if comp.has_residual_risk() {
                return (MissionTxState::Failed, TxOutcome::Failed);
            }
        }

        (current_state, TxOutcome::Failed)
    }

    fn make_event(
        &self,
        kind: TxEventKind,
        phase: TxObservabilityPhase,
        reason_code: &str,
        execution_id: &str,
        plan_id: &str,
        tx_phase: TxPhase,
        timestamp_ms: i64,
    ) -> TxObservabilityEvent {
        let seq = self.event_seq.get();
        self.event_seq.set(seq + 1);
        TxObservabilityEvent {
            sequence: seq,
            timestamp_ms: timestamp_ms as u64,
            kind,
            reason_code: reason_code.to_string(),
            phase,
            execution_id: execution_id.to_string(),
            plan_id: plan_id.to_string(),
            plan_hash: 0,
            step_id: String::new(),
            idem_key: String::new(),
            tx_phase,
            chain_hash: String::new(),
            agent_id: String::new(),
            details: HashMap::new(),
        }
    }
}

fn reason_code_for_outcome(outcome: &TxOutcome) -> &'static str {
    match outcome {
        TxOutcome::Pending => "pending",
        TxOutcome::Committed => "committed",
        TxOutcome::Failed => "failed",
        TxOutcome::Compensated => "compensated",
    }
}

// ── Errors ───────────────────────────────────────────────────────────────────

/// Errors from the tx execution engine.
#[derive(Debug, Clone)]
pub enum TxExecutionError {
    /// Contract validation failed.
    InvalidContract(String),
    /// Phase transition failed.
    PhaseTransition(String),
    /// Prepare phase error.
    PreparePhase(String),
    /// Commit phase error.
    CommitPhase(String),
    /// Compensation phase error.
    CompensationPhase(String),
    /// Ledger not found for resume.
    LedgerNotFound(String),
}

impl std::fmt::Display for TxExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidContract(msg) => write!(f, "Invalid contract: {msg}"),
            Self::PhaseTransition(msg) => write!(f, "Phase transition error: {msg}"),
            Self::PreparePhase(msg) => write!(f, "Prepare phase error: {msg}"),
            Self::CommitPhase(msg) => write!(f, "Commit phase error: {msg}"),
            Self::CompensationPhase(msg) => write!(f, "Compensation phase error: {msg}"),
            Self::LedgerNotFound(id) => write!(f, "Ledger not found: {id}"),
        }
    }
}

impl std::error::Error for TxExecutionError {}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        MissionActorRole, MissionTxContract, MissionTxState, StepAction, TxId, TxIntent, TxOutcome,
        TxPlan as ContractTxPlan, TxPlanId, TxStep, TxStepId,
    };

    fn make_test_contract(num_steps: usize) -> MissionTxContract {
        let steps: Vec<TxStep> = (0..num_steps)
            .map(|i| TxStep {
                step_id: TxStepId(format!("step-{i}")),
                ordinal: i,
                action: StepAction::SendText {
                    pane_id: i as u64,
                    text: format!("action-{i}"),
                    paste_mode: None,
                },
                description: format!("Test step {i}"),
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-test-1".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Test transaction".to_string(),
                correlation_id: "corr-1".to_string(),
                created_at_ms: 1000,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-1".to_string()),
                tx_id: TxId("tx-test-1".to_string()),
                steps,
                preconditions: Vec::new(),
                compensations: Vec::new(),
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    #[test]
    fn execute_happy_path_single_step() {
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        assert!(result.commit_report.is_some());
        assert!(result.compensation_report.is_none());
        assert!(result.prepare_report.outcome.commit_eligible());
        assert!(!result.events.is_empty());
    }

    #[test]
    fn execute_happy_path_multiple_steps() {
        let mut contract = make_test_contract(5);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 5);
        assert_eq!(commit.failed_count, 0);
        assert_eq!(commit.skipped_count, 0);
    }

    #[test]
    fn execute_with_failure_injection_triggers_compensation() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-1".to_string()),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::RolledBack);
        assert_eq!(result.outcome, TxOutcome::Compensated);
        assert!(result.compensation_report.is_some());
        let commit = result.commit_report.unwrap();
        assert!(commit.has_failures());
        assert_eq!(commit.committed_count, 1);
        assert_eq!(commit.failed_count, 1);
        assert_eq!(commit.skipped_count, 1);
    }

    #[test]
    fn execute_with_failure_at_first_step() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-0".to_string()),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        let comp = result.compensation_report.unwrap();
        assert_eq!(
            comp.outcome,
            crate::plan::TxCompensationOutcome::NothingToCompensate
        );
    }

    #[test]
    fn execute_with_compensation_failure() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-2".to_string()),
            fail_compensation_for_step: Some("step-0".to_string()),
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        let comp = result.compensation_report.unwrap();
        assert!(comp.has_residual_risk());
    }

    #[test]
    fn execute_without_auto_compensate() {
        let mut contract = make_test_contract(3);
        let config = TxExecutionConfig {
            fail_step: Some("step-1".to_string()),
            auto_compensate: false,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
        assert!(result.compensation_report.is_none());
    }

    #[test]
    fn execute_with_kill_switch_blocks_at_prepare() {
        let mut contract = make_test_contract(2);
        let config = TxExecutionConfig {
            kill_switch: MissionKillSwitchLevel::HardStop,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(!result.prepare_report.outcome.commit_eligible());
        assert!(result.commit_report.is_none());
    }

    #[test]
    fn execute_with_pause_suspends_commit() {
        let mut contract = make_test_contract(2);
        let config = TxExecutionConfig {
            paused: true,
            ..TxExecutionConfig::default()
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, config);
        let result = engine.execute(&mut contract, 5000).unwrap();

        let commit = result.commit_report.unwrap();
        assert_eq!(commit.outcome, crate::plan::TxCommitOutcome::PauseSuspended);
        assert_eq!(commit.skipped_count, 2);
    }

    #[test]
    fn execute_empty_contract_is_error() {
        let mut contract = MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-empty".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Empty".to_string(),
                correlation_id: "corr-0".to_string(),
                created_at_ms: 0,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-empty".to_string()),
                tx_id: TxId("tx-empty".to_string()),
                steps: Vec::new(),
                preconditions: Vec::new(),
                compensations: Vec::new(),
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        };
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let err = engine.execute(&mut contract, 5000).unwrap_err();
        assert!(matches!(err, TxExecutionError::InvalidContract(_)));
    }

    #[test]
    fn events_emitted_for_all_phases() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let event_kinds: Vec<_> = result.events.iter().map(|e| &e.kind).collect();
        assert!(event_kinds.contains(&&TxEventKind::PrepareStarted));
        assert!(event_kinds.contains(&&TxEventKind::PrepareCompleted));
        assert!(event_kinds.contains(&&TxEventKind::CommitStarted));
        assert!(event_kinds.contains(&&TxEventKind::CommitCompleted));
    }

    #[test]
    fn ledger_records_commit_steps() {
        let mut contract = make_test_contract(3);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(result.ledger.record_count() >= 3);
    }

    #[test]
    fn ledger_reaches_terminal_phase_on_success() {
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(result.ledger.phase().is_terminal());
    }

    #[test]
    fn decision_path_traces_execution() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert!(result.decision_path.contains("prepare"));
        assert!(result.decision_path.contains("commit"));
        assert!(result.decision_path.contains("final"));
    }

    #[test]
    fn execution_config_serde_roundtrip() {
        let config = TxExecutionConfig {
            auto_compensate: false,
            produce_forensic_bundle: false,
            max_steps_per_batch: 50,
            kill_switch: MissionKillSwitchLevel::SafeMode,
            paused: true,
            fail_step: Some("s1".to_string()),
            fail_compensation_for_step: Some("s2".to_string()),
            observability: TxObservabilityConfig::default(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: TxExecutionConfig = serde_json::from_str(&json).unwrap();
        assert!(!back.auto_compensate);
        assert!(back.paused);
        assert_eq!(back.fail_step, Some("s1".to_string()));
    }

    #[test]
    fn execution_result_serde_roundtrip() {
        let mut contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let json = serde_json::to_string(&result).unwrap();
        let back: TxExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.final_state, MissionTxState::Committed);
        assert_eq!(back.outcome, TxOutcome::Committed);
    }

    #[test]
    fn error_display_formats() {
        let errors = vec![
            TxExecutionError::InvalidContract("bad".to_string()),
            TxExecutionError::PhaseTransition("bad transition".to_string()),
            TxExecutionError::PreparePhase("failed".to_string()),
            TxExecutionError::CommitPhase("failed".to_string()),
            TxExecutionError::CompensationPhase("failed".to_string()),
            TxExecutionError::LedgerNotFound("id-1".to_string()),
        ];
        for err in &errors {
            let msg = err.to_string();
            assert!(!msg.is_empty());
        }
    }

    struct DenyingExecutor;

    impl StepExecutor for DenyingExecutor {
        fn evaluate_gates(&self, contract: &MissionTxContract) -> Vec<TxPrepareGateInput> {
            contract
                .plan
                .steps
                .iter()
                .map(|step| TxPrepareGateInput {
                    step_id: step.step_id.clone(),
                    policy_passed: false,
                    policy_reason_code: Some("policy.denied".to_string()),
                    reservation_available: true,
                    reservation_reason_code: None,
                    approval_satisfied: true,
                    approval_reason_code: None,
                    target_liveness: true,
                    liveness_reason_code: None,
                })
                .collect()
        }

        fn execute_steps(
            &self,
            contract: &MissionTxContract,
            fail_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCommitStepInput> {
            crate::plan::mission_tx_commit_step_inputs(contract, fail_step, now_ms)
        }

        fn execute_compensations(
            &self,
            commit_report: &TxCommitReport,
            fail_for_step: Option<&str>,
            now_ms: i64,
        ) -> Vec<TxCompensationStepInput> {
            crate::plan::mission_tx_compensation_inputs(commit_report, fail_for_step, now_ms)
        }
    }

    #[test]
    fn custom_executor_policy_denial_blocks_commit() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(DenyingExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(result.prepare_report.outcome, TxPrepareOutcome::Denied);
        assert!(result.commit_report.is_none());
        assert_eq!(result.final_state, MissionTxState::Failed);
    }

    #[test]
    fn reason_code_mapping() {
        assert_eq!(reason_code_for_outcome(&TxOutcome::Pending), "pending");
        assert_eq!(reason_code_for_outcome(&TxOutcome::Committed), "committed");
        assert_eq!(reason_code_for_outcome(&TxOutcome::Failed), "failed");
        assert_eq!(
            reason_code_for_outcome(&TxOutcome::Compensated),
            "compensated"
        );
    }

    #[test]
    fn synthetic_executor_implements_trait() {
        let executor = SyntheticStepExecutor;
        let contract = make_test_contract(2);
        let gates = executor.evaluate_gates(&contract);
        assert_eq!(gates.len(), 2);
        assert!(gates[0].policy_passed);
        assert!(gates[0].target_liveness);
    }

    #[test]
    fn event_sequence_numbers_are_monotonic() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        for (i, event) in result.events.iter().enumerate() {
            if i > 0 {
                assert!(event.sequence > result.events[i - 1].sequence);
            }
        }
    }

    #[test]
    fn contract_state_updates_after_execution() {
        let mut contract = make_test_contract(2);
        assert_eq!(contract.lifecycle_state, MissionTxState::Planned);
        assert_eq!(contract.outcome, TxOutcome::Pending);

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let _ = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(contract.lifecycle_state, MissionTxState::Committed);
        assert_eq!(contract.outcome, TxOutcome::Committed);
    }
}
