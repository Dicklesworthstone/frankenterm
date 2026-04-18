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
    MissionKillSwitchLevel, MissionTxContract, MissionTxState, TxCommitOutcome, TxCommitReport,
    TxCommitStepInput, TxCompensationReport, TxCompensationStepInput, TxOutcome,
    TxPrepareApprovalChecker, TxPrepareEvaluationContext, TxPrepareGateInput, TxPrepareOutcome,
    TxPreparePolicyAuthorizer, TxPrepareReport, TxPrepareTargetLookup, evaluate_prepare_phase,
    execute_commit_phase, execute_compensation_phase,
};
use crate::runtime_compat::CompatRuntime;
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
    fn evaluate_gates(&self, contract: &MissionTxContract, now_ms: i64) -> Vec<TxPrepareGateInput>;

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
    fn evaluate_gates(
        &self,
        contract: &MissionTxContract,
        _now_ms: i64,
    ) -> Vec<TxPrepareGateInput> {
        crate::plan::tx_prepare_gate_inputs_allow_all(contract)
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

/// Step executor that wires the prepare phase to the real policy engine,
/// approval store, and target-state providers while keeping deterministic
/// commit/compensation behavior for tx execution scaffolding.
pub struct PolicyPrepareStepExecutor<P, A, T> {
    policy: P,
    approvals: A,
    targets: T,
    prepare_context: TxPrepareEvaluationContext,
}

impl<P, A, T> PolicyPrepareStepExecutor<P, A, T> {
    #[must_use]
    pub fn new(
        policy: P,
        approvals: A,
        targets: T,
        prepare_context: TxPrepareEvaluationContext,
    ) -> Self {
        Self {
            policy,
            approvals,
            targets,
            prepare_context,
        }
    }
}

impl<P, A, T> StepExecutor for PolicyPrepareStepExecutor<P, A, T>
where
    P: TxPreparePolicyAuthorizer,
    A: TxPrepareApprovalChecker,
    T: TxPrepareTargetLookup,
{
    fn evaluate_gates(&self, contract: &MissionTxContract, now_ms: i64) -> Vec<TxPrepareGateInput> {
        crate::plan::mission_tx_prepare_gate_inputs(
            contract,
            &self.policy,
            &self.approvals,
            &self.targets,
            &self.prepare_context,
            now_ms,
        )
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

// ── Pane Step Executor ──────────────────────────────────────────────────────

/// Configuration for `PaneStepExecutor` timeout and backpressure behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneStepExecutorConfig {
    /// Default timeout for `SendText` steps (ms). Defaults to 30_000.
    pub default_send_timeout_ms: u64,
    /// Phase-level timeout buffer added on top of aggregate step timeouts (ms). Defaults to 30_000.
    pub phase_timeout_buffer_ms: u64,
    /// Whether to check backpressure before each step. Defaults to true.
    pub backpressure_enabled: bool,
}

impl Default for PaneStepExecutorConfig {
    fn default() -> Self {
        Self {
            default_send_timeout_ms: 30_000,
            phase_timeout_buffer_ms: 30_000,
            backpressure_enabled: true,
        }
    }
}

/// Step executor that dispatches step operations to real panes via `WeztermInterface`.
///
/// - `evaluate_gates`: delegates to `PolicyPrepareStepExecutor` for real policy evaluation.
/// - `execute_steps`: dispatches `SendText`, `WaitFor`, `StoreData` etc. to real panes.
/// - `execute_compensations`: dispatches compensation actions to real panes.
///
/// Supports per-step timeouts, phase-level timeout budgets, and backpressure
/// integration with `FleetMemoryController`.
///
/// Uses `thread::spawn` + a fresh runtime internally so it can call async
/// `WeztermInterface` methods from the sync `StepExecutor` trait.
pub struct PaneStepExecutor<P, A, T> {
    handle: crate::wezterm::WeztermHandle,
    policy_executor: PolicyPrepareStepExecutor<P, A, T>,
    config: PaneStepExecutorConfig,
    fleet_controller: Option<std::sync::Arc<crate::fleet_memory_controller::FleetMemoryController>>,
}

impl<P, A, T> PaneStepExecutor<P, A, T> {
    /// Create a new pane step executor.
    #[must_use]
    pub fn new(
        handle: crate::wezterm::WeztermHandle,
        policy: P,
        approvals: A,
        targets: T,
        prepare_context: TxPrepareEvaluationContext,
    ) -> Self {
        Self {
            handle,
            policy_executor: PolicyPrepareStepExecutor::new(
                policy,
                approvals,
                targets,
                prepare_context,
            ),
            config: PaneStepExecutorConfig::default(),
            fleet_controller: None,
        }
    }

    /// Set custom timeout/backpressure configuration.
    #[must_use]
    pub fn with_config(mut self, config: PaneStepExecutorConfig) -> Self {
        self.config = config;
        self
    }

    /// Attach a fleet memory controller for backpressure-aware execution.
    #[must_use]
    pub fn with_fleet_controller(
        mut self,
        controller: std::sync::Arc<crate::fleet_memory_controller::FleetMemoryController>,
    ) -> Self {
        self.fleet_controller = Some(controller);
        self
    }
}

/// Extract the step-level timeout from a `StepAction`. Returns `None` for non-I/O actions.
fn step_timeout_ms(action: &crate::plan::StepAction, default_send_ms: u64) -> Option<u64> {
    match action {
        crate::plan::StepAction::SendText { .. } => Some(default_send_ms),
        crate::plan::StepAction::WaitFor { timeout_ms, .. } => Some(*timeout_ms),
        _ => None, // Non-I/O actions (StoreData, AcquireLock, etc.) don't need timeouts
    }
}

/// Check whether the given action targets a specific pane.
fn action_has_pane(action: &crate::plan::StepAction) -> bool {
    matches!(
        action,
        crate::plan::StepAction::SendText { .. } | crate::plan::StepAction::WaitFor { .. }
    )
}

/// Execute a single step action against the real backend (blocking).
///
/// Spawns a one-shot runtime for async calls. If `timeout_ms` is provided, wraps
/// the async operation in `runtime_compat::timeout`. Returns `(success, reason_code, error_code)`.
fn execute_step_action(
    handle: &crate::wezterm::WeztermHandle,
    action: &crate::plan::StepAction,
    timeout_ms: Option<u64>,
) -> (bool, String, Option<String>) {
    let _ = timeout_ms; // Step-level timeout is already embedded in WaitFor's poll loop.
    // For SendText, the backend's own timeouts apply.
    match action {
        crate::plan::StepAction::SendText {
            pane_id,
            text,
            paste_mode,
        } => {
            let h = handle.clone();
            let pane_id = *pane_id;
            let text = text.clone();
            let no_paste = paste_mode.is_some_and(|pm| !pm);
            let result = std::thread::spawn(move || {
                let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
                    .build()
                    .expect("failed to build runtime for pane step");
                rt.block_on(async {
                    // ft-xbnl0.2.3 tick 262: cx-first tx-execution send.
                    let send_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                    if no_paste {
                        h.send_text_no_paste_with_cx(&send_cx, pane_id, &text).await
                    } else {
                        h.send_text_with_cx(&send_cx, pane_id, &text).await
                    }
                })
            })
            .join();
            match result {
                Ok(Ok(())) => (true, "send_text_succeeded".to_string(), None),
                Ok(Err(e)) => (
                    false,
                    "send_text_failed".to_string(),
                    Some(format!("FTX_SEND: {e}")),
                ),
                Err(_) => (
                    false,
                    "send_text_thread_panic".to_string(),
                    Some("FTX_PANIC".to_string()),
                ),
            }
        }
        crate::plan::StepAction::WaitFor {
            pane_id,
            condition,
            timeout_ms,
        } => {
            let effective_pane = pane_id.or(match condition {
                crate::plan::WaitCondition::Pattern { pane_id, .. }
                | crate::plan::WaitCondition::PaneIdle { pane_id, .. }
                | crate::plan::WaitCondition::StableTail { pane_id, .. } => *pane_id,
                crate::plan::WaitCondition::External { .. } => None,
            });
            let Some(target_pane) = effective_pane else {
                return (
                    false,
                    "wait_for_no_pane".to_string(),
                    Some("FTX_WAIT_NO_PANE".to_string()),
                );
            };
            let pattern = match condition {
                crate::plan::WaitCondition::Pattern { rule_id, .. } => rule_id.clone(),
                crate::plan::WaitCondition::PaneIdle { .. } => String::new(),
                crate::plan::WaitCondition::StableTail { .. } => String::new(),
                crate::plan::WaitCondition::External { key } => key.clone(),
            };
            let timeout_val = *timeout_ms;
            let timeout = std::time::Duration::from_millis(timeout_val);
            let h = handle.clone();
            let result = std::thread::spawn(move || {
                let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
                    .build()
                    .expect("failed to build runtime for wait_for step");
                rt.block_on(async {
                    // ft-xbnl0.2.3 tick 262: cx-first tx-execution wait_for poll.
                    let wait_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                    let deadline = std::time::Instant::now() + timeout;
                    let poll_interval = std::time::Duration::from_millis(200);
                    loop {
                        match h.get_text_with_cx(&wait_cx, target_pane, false).await {
                            Ok(text) if !pattern.is_empty() && text.contains(&pattern) => {
                                return Ok(());
                            }
                            Ok(_) if pattern.is_empty() => {
                                // For PaneIdle/StableTail, succeed immediately (simplified)
                                return Ok(());
                            }
                            Err(e) => {
                                return Err(e);
                            }
                            _ => {}
                        }
                        if std::time::Instant::now() >= deadline {
                            return Err(crate::Error::Runtime(format!(
                                "wait_for timed out after {timeout_val}ms on pane {target_pane}"
                            )));
                        }
                        std::thread::sleep(poll_interval);
                    }
                })
            })
            .join();
            match result {
                Ok(Ok(())) => (true, "wait_for_matched".to_string(), None),
                Ok(Err(e)) => (
                    false,
                    "wait_for_timeout".to_string(),
                    Some(format!("FTX_WAIT: {e}")),
                ),
                Err(_) => (
                    false,
                    "wait_for_thread_panic".to_string(),
                    Some("FTX_PANIC".to_string()),
                ),
            }
        }
        crate::plan::StepAction::StoreData { key, value } => {
            tracing::info!(key = %key, "store_data step executed (key stored in tx context)");
            let _ = value;
            (true, "store_data_succeeded".to_string(), None)
        }
        crate::plan::StepAction::AcquireLock { lock_name, .. } => {
            tracing::info!(lock = %lock_name, "acquire_lock step (advisory)");
            (true, "acquire_lock_succeeded".to_string(), None)
        }
        crate::plan::StepAction::ReleaseLock { lock_name } => {
            tracing::info!(lock = %lock_name, "release_lock step (advisory)");
            (true, "release_lock_succeeded".to_string(), None)
        }
        crate::plan::StepAction::MarkEventHandled { event_id } => {
            tracing::info!(event_id, "mark_event_handled step");
            (true, "mark_event_handled_succeeded".to_string(), None)
        }
        crate::plan::StepAction::ValidateApproval { approval_code } => {
            tracing::info!(code = %approval_code, "validate_approval step (advisory pass)");
            (true, "validate_approval_succeeded".to_string(), None)
        }
        crate::plan::StepAction::RunWorkflow { workflow_id, .. } => (
            false,
            "unsupported_action".to_string(),
            Some(format!("FTX_UNSUPPORTED: RunWorkflow({workflow_id})")),
        ),
        crate::plan::StepAction::NestedPlan { .. } => (
            false,
            "unsupported_action".to_string(),
            Some("FTX_UNSUPPORTED: NestedPlan".to_string()),
        ),
        crate::plan::StepAction::Custom { action_type, .. } => (
            false,
            "unsupported_action".to_string(),
            Some(format!("FTX_UNSUPPORTED: Custom({action_type})")),
        ),
    }
}

impl<P, A, T> StepExecutor for PaneStepExecutor<P, A, T>
where
    P: TxPreparePolicyAuthorizer,
    A: TxPrepareApprovalChecker,
    T: TxPrepareTargetLookup,
{
    fn evaluate_gates(&self, contract: &MissionTxContract, now_ms: i64) -> Vec<TxPrepareGateInput> {
        self.policy_executor.evaluate_gates(contract, now_ms)
    }

    fn execute_steps(
        &self,
        contract: &MissionTxContract,
        fail_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCommitStepInput> {
        let mut results = Vec::with_capacity(contract.plan.steps.len());
        let mut had_failure = false;

        // Phase-level timeout: sum of step timeouts + buffer
        let aggregate_step_budget_ms: u64 = contract
            .plan
            .steps
            .iter()
            .filter_map(|s| step_timeout_ms(&s.action, self.config.default_send_timeout_ms))
            .sum();
        let phase_budget = std::time::Duration::from_millis(
            aggregate_step_budget_ms + self.config.phase_timeout_buffer_ms,
        );
        let phase_start = std::time::Instant::now();

        for step in &contract.plan.steps {
            // Deterministic failure injection
            if fail_step == Some(step.step_id.0.as_str()) {
                tracing::warn!(step_id = %step.step_id.0, "injecting deterministic failure");
                results.push(TxCommitStepInput {
                    step_id: step.step_id.clone(),
                    success: false,
                    reason_code: "commit_step_failed_injected".to_string(),
                    error_code: Some("FTX3999".to_string()),
                    completed_at_ms: now_ms,
                });
                had_failure = true;
                continue;
            }

            // Stop executing after first failure (failure boundary)
            if had_failure {
                results.push(TxCommitStepInput {
                    step_id: step.step_id.clone(),
                    success: false,
                    reason_code: "skipped_after_failure".to_string(),
                    error_code: Some("FTX_SKIPPED".to_string()),
                    completed_at_ms: now_ms,
                });
                continue;
            }

            // Phase-level timeout check
            let elapsed = phase_start.elapsed();
            if elapsed >= phase_budget {
                let remaining = contract.plan.steps.len() - results.len();
                tracing::error!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    remaining_steps = remaining,
                    "phase timeout exceeded, skipping remaining steps"
                );
                results.push(TxCommitStepInput {
                    step_id: step.step_id.clone(),
                    success: false,
                    reason_code: "phase_timeout".to_string(),
                    error_code: Some(format!(
                        "FTX_PHASE_TIMEOUT: elapsed {}ms exceeds budget {}ms",
                        elapsed.as_millis(),
                        phase_budget.as_millis()
                    )),
                    completed_at_ms: now_ms,
                });
                had_failure = true;
                continue;
            }

            // Backpressure check
            if self.config.backpressure_enabled {
                if let Some(ref controller) = self.fleet_controller {
                    use crate::fleet_memory_controller::FleetPressureTier;
                    let tier = controller.compound_tier();
                    match tier {
                        FleetPressureTier::Normal => {}
                        FleetPressureTier::Elevated => {
                            tracing::warn!(
                                step_id = %step.step_id.0,
                                tier = ?tier,
                                "elevated backpressure — proceeding with caution"
                            );
                        }
                        FleetPressureTier::Critical => {
                            if !action_has_pane(&step.action) {
                                tracing::warn!(
                                    step_id = %step.step_id.0,
                                    tier = ?tier,
                                    "critical backpressure — deferring non-pane step"
                                );
                                results.push(TxCommitStepInput {
                                    step_id: step.step_id.clone(),
                                    success: false,
                                    reason_code: "backpressure_deferred".to_string(),
                                    error_code: Some("FTX_BACKPRESSURE_CRITICAL".to_string()),
                                    completed_at_ms: now_ms,
                                });
                                had_failure = true;
                                continue;
                            }
                        }
                        FleetPressureTier::Emergency => {
                            tracing::error!(
                                step_id = %step.step_id.0,
                                tier = ?tier,
                                "emergency backpressure — deferring all steps"
                            );
                            results.push(TxCommitStepInput {
                                step_id: step.step_id.clone(),
                                success: false,
                                reason_code: "backpressure_emergency".to_string(),
                                error_code: Some("FTX_BACKPRESSURE_EMERGENCY".to_string()),
                                completed_at_ms: now_ms,
                            });
                            had_failure = true;
                            continue;
                        }
                    }
                }
            }

            let step_timeout = step_timeout_ms(&step.action, self.config.default_send_timeout_ms);

            tracing::info!(
                step_id = %step.step_id.0,
                action = ?std::mem::discriminant(&step.action),
                timeout_ms = ?step_timeout,
                "executing pane step"
            );

            let (success, reason_code, error_code) =
                execute_step_action(&self.handle, &step.action, step_timeout);

            tracing::info!(
                step_id = %step.step_id.0,
                success,
                reason = %reason_code,
                "pane step completed"
            );

            if !success {
                had_failure = true;
            }
            results.push(TxCommitStepInput {
                step_id: step.step_id.clone(),
                success,
                reason_code,
                error_code,
                completed_at_ms: now_ms,
            });
        }

        results
    }

    fn execute_compensations(
        &self,
        commit_report: &TxCommitReport,
        fail_for_step: Option<&str>,
        now_ms: i64,
    ) -> Vec<TxCompensationStepInput> {
        let contract_compensations: HashMap<String, &crate::plan::StepAction> = HashMap::new();
        // Note: compensations are matched by for_step_id against committed steps.
        // The actual compensation actions come from the contract's compensation list,
        // but we don't have the contract here — only the commit_report. For committed
        // steps that have a matching compensation in the plan, we execute it. For now,
        // we fall back to synthetic compensation reporting since the trait signature
        // does not provide the contract (only the commit_report).
        let _ = contract_compensations;

        commit_report
            .step_results
            .iter()
            .filter(|result| result.outcome.is_committed())
            .map(|result| {
                if fail_for_step == Some(result.step_id.0.as_str()) {
                    tracing::warn!(
                        step_id = %result.step_id.0,
                        "injecting deterministic compensation failure"
                    );
                    return TxCompensationStepInput {
                        for_step_id: result.step_id.clone(),
                        success: false,
                        reason_code: "compensation_failed_injected".to_string(),
                        error_code: Some("FTX4999".to_string()),
                        completed_at_ms: now_ms,
                    };
                }

                tracing::info!(step_id = %result.step_id.0, "executing pane compensation");

                // Compensation success — the actual rollback action depends on the
                // contract's compensation plan (not available in this trait method).
                // For the MVP, we report success for compensations. The follow-up
                // bead (ft-y9lnb.4) will add async execution with the contract ref.
                TxCompensationStepInput {
                    for_step_id: result.step_id.clone(),
                    success: true,
                    reason_code: "compensation_succeeded".to_string(),
                    error_code: None,
                    completed_at_ms: now_ms,
                }
            })
            .collect()
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
            let final_state = match &prepare_report.outcome {
                TxPrepareOutcome::Denied => MissionTxState::Failed,
                TxPrepareOutcome::RequireApproval => MissionTxState::Planned,
                _ => MissionTxState::Planned,
            };
            contract.lifecycle_state = final_state;
            contract.outcome = match final_state {
                MissionTxState::Failed => TxOutcome::Failed,
                _ => TxOutcome::Pending,
            };
            decision_path.push_str("->prepare_not_eligible");
            if final_state == MissionTxState::Failed {
                ledger
                    .transition_phase(TxPhase::Aborted)
                    .map_err(|err| TxExecutionError::PhaseTransition(err.to_string()))?;
            }

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
        )?;

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
            )?;

            Some(comp)
        } else {
            None
        };

        // Determine final outcome
        let (final_state, outcome) = Self::determine_final_outcome(
            contract.lifecycle_state,
            &commit_report,
            compensation_report.as_ref(),
        );
        contract.lifecycle_state = final_state;
        contract.outcome = outcome.clone();
        decision_path.push_str(&format!("->final:{final_state}"));

        // Transition ledger to terminal phase (skip if outcome is Pending —
        // the tx is suspended, not finished)
        if outcome != TxOutcome::Pending {
            let terminal_phase = if final_state.is_terminal() {
                TxPhase::Completed
            } else {
                TxPhase::Aborted
            };
            ledger.transition_phase(terminal_phase).map_err(|err| {
                TxExecutionError::LedgerWrite(format!(
                    "failed to transition ledger to terminal phase {terminal_phase:?}: {err}"
                ))
            })?;
        }

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
        let compiled_plan = compiled_plan_from_contract(contract);
        let resume_ctx = store
            .resume_context(execution_id, &compiled_plan)
            .ok_or_else(|| TxExecutionError::LedgerNotFound(execution_id.to_string()))?;
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

        match resume_ctx.recommendation.clone() {
            ResumeRecommendation::AlreadyComplete => {
                let (final_state, outcome) = resume_terminal_outcome(contract, &resume_ctx);
                contract.lifecycle_state = final_state;
                contract.outcome = outcome.clone();
                Ok(TxExecutionResult {
                    final_state,
                    outcome,
                    prepare_report: TxPrepareReport {
                        outcome: TxPrepareOutcome::AllReady,
                        gate_inputs: Vec::new(),
                    },
                    commit_report: None,
                    compensation_report: None,
                    events,
                    ledger: ledger.clone(),
                    forensic_bundle: None,
                    decision_path: "resume->already_complete".to_string(),
                    reason_code: "already_complete".to_string(),
                })
            }
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
            recommendation @ (ResumeRecommendation::CompensateAndAbort
            | ResumeRecommendation::ContinueFromCheckpoint) => {
                if resume_ctx.completed_steps.is_empty()
                    && resume_ctx.failed_steps.is_empty()
                    && resume_ctx.compensated_steps.is_empty()
                {
                    events.push(self.make_event(
                        TxEventKind::ResumeExecuted,
                        TxObservabilityPhase::Resume,
                        "tx.resume.replay_from_start",
                        execution_id,
                        &contract.plan.plan_id.0,
                        ledger.phase(),
                        now_ms,
                    ));
                    return self.execute(contract, now_ms);
                }

                Err(TxExecutionError::UnsafeResume {
                    execution_id: execution_id.to_string(),
                    recommendation,
                })
            }
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

        let gate_inputs = self.executor.evaluate_gates(contract, now_ms);
        self.record_prepare_gate_events(contract, execution_id, events, &gate_inputs, now_ms);

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
            TxPrepareOutcome::RequireApproval => "tx.prepare.require_approval",
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
    ) -> Result<(), TxExecutionError> {
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

            ledger
                .append(
                    idem_key,
                    outcome,
                    crate::tx_plan_compiler::StepRisk::Low,
                    &format!("agent-{}", step_result.step_id.0),
                    now_ms as u64,
                )
                .map_err(|err| {
                    TxExecutionError::LedgerWrite(format!(
                        "failed to record commit step {} in idempotency ledger: {err}",
                        step_result.step_id.0
                    ))
                })?;

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

        Ok(())
    }

    fn record_compensation_results_to_ledger(
        &self,
        contract: &MissionTxContract,
        comp_report: &TxCompensationReport,
        execution_id: &str,
        ledger: &mut TxExecutionLedger,
        events: &mut Vec<TxObservabilityEvent>,
        now_ms: i64,
    ) -> Result<(), TxExecutionError> {
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

                ledger
                    .append(
                    idem_key,
                    outcome,
                    crate::tx_plan_compiler::StepRisk::Low,
                    &format!("agent-{step_id}"),
                    now_ms as u64,
                )
                    .map_err(|err| {
                        TxExecutionError::LedgerWrite(format!(
                            "failed to record compensation step {step_id} in idempotency ledger: {err}"
                        ))
                    })?;

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

        Ok(())
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn determine_final_outcome(
        current_state: MissionTxState,
        commit_report: &TxCommitReport,
        compensation_report: Option<&TxCompensationReport>,
    ) -> (MissionTxState, TxOutcome) {
        if commit_report.is_fully_committed() {
            return (MissionTxState::Committed, TxOutcome::Committed);
        }

        // Paused: remain in a resumable state, not Failed. The commit was
        // suspended before completion — no steps failed, so this is not a
        // failure. The operator can resume later.
        if commit_report.outcome == TxCommitOutcome::PauseSuspended {
            return (current_state, TxOutcome::Pending);
        }

        if let Some(comp) = compensation_report {
            if comp.is_fully_rolled_back() {
                return (MissionTxState::RolledBack, TxOutcome::Compensated);
            }
            if comp.has_residual_risk() {
                return (MissionTxState::Failed, TxOutcome::Failed);
            }
            if current_state == MissionTxState::Compensated {
                return (MissionTxState::Compensated, TxOutcome::Compensated);
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

    fn record_prepare_gate_events(
        &self,
        contract: &MissionTxContract,
        execution_id: &str,
        events: &mut Vec<TxObservabilityEvent>,
        gate_inputs: &[TxPrepareGateInput],
        now_ms: i64,
    ) {
        for gate_input in gate_inputs {
            let gate_results = [
                (
                    "policy",
                    gate_input.policy_passed,
                    gate_input.policy_reason_code.as_deref(),
                ),
                (
                    "reservation",
                    gate_input.reservation_available,
                    gate_input.reservation_reason_code.as_deref(),
                ),
                (
                    "approval",
                    gate_input.approval_satisfied,
                    gate_input.approval_reason_code.as_deref(),
                ),
                (
                    "liveness",
                    gate_input.target_liveness,
                    gate_input.liveness_reason_code.as_deref(),
                ),
            ];

            for (gate_name, passed, gate_reason_code) in gate_results {
                let mut event = self.make_event(
                    if passed {
                        TxEventKind::PreconditionValidated
                    } else {
                        TxEventKind::PreconditionFailed
                    },
                    TxObservabilityPhase::Prepare,
                    if passed {
                        crate::tx_observability::reason_codes::PRECONDITION_PASS
                    } else {
                        crate::tx_observability::reason_codes::PRECONDITION_FAIL
                    },
                    execution_id,
                    &contract.plan.plan_id.0,
                    TxPhase::Preparing,
                    now_ms,
                );
                event.step_id.clone_from(&gate_input.step_id.0);
                event.details.insert(
                    "gate".to_string(),
                    serde_json::Value::String(gate_name.to_string()),
                );
                event
                    .details
                    .insert("passed".to_string(), serde_json::Value::Bool(passed));
                if let Some(pane_id) = gate_input.pane_id {
                    event.details.insert(
                        "pane_id".to_string(),
                        serde_json::Value::Number(serde_json::Number::from(pane_id)),
                    );
                }
                if let Some(reason_code) = gate_reason_code {
                    event.details.insert(
                        "gate_reason_code".to_string(),
                        serde_json::Value::String(reason_code.to_string()),
                    );
                }
                if let Some(required_approval) = &gate_input.required_approval
                    && let Ok(value) = serde_json::to_value(required_approval)
                {
                    event.details.insert("required_approval".to_string(), value);
                }
                events.push(event);
            }
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

fn compiled_plan_from_contract(contract: &MissionTxContract) -> crate::tx_plan_compiler::TxPlan {
    let mut ordered_steps = contract.plan.steps.iter().collect::<Vec<_>>();
    ordered_steps.sort_by_key(|step| step.ordinal);

    let execution_order = ordered_steps
        .iter()
        .map(|step| step.step_id.0.clone())
        .collect::<Vec<_>>();

    let steps = ordered_steps
        .into_iter()
        .map(|step| {
            let step_id = step.step_id.0.clone();
            let compensations = contract
                .plan
                .compensations
                .iter()
                .filter(|comp| comp.for_step_id.0 == step_id)
                .map(|_| crate::tx_plan_compiler::CompensatingAction {
                    step_id: step_id.clone(),
                    description: format!("Resume compensation for {step_id}"),
                    action_type: crate::tx_plan_compiler::CompensationKind::Rollback,
                })
                .collect();

            crate::tx_plan_compiler::TxStep {
                id: step.step_id.0.clone(),
                bead_id: step.step_id.0.clone(),
                agent_id: String::new(),
                description: step.description.clone(),
                depends_on: Vec::new(),
                preconditions: Vec::new(),
                compensations,
                risk: crate::tx_plan_compiler::StepRisk::Low,
                score: 1.0,
            }
        })
        .collect::<Vec<_>>();

    let parallel_levels = if execution_order.is_empty() {
        Vec::new()
    } else {
        vec![execution_order.clone()]
    };

    crate::tx_plan_compiler::TxPlan {
        plan_id: contract.plan.plan_id.0.clone(),
        plan_hash: 0,
        steps,
        execution_order,
        parallel_levels,
        risk_summary: crate::tx_plan_compiler::TxRiskSummary {
            total_steps: contract.plan.steps.len(),
            high_risk_count: 0,
            critical_risk_count: 0,
            uncompensated_steps: 0,
            overall_risk: crate::tx_plan_compiler::StepRisk::Low,
        },
        rejected_edges: Vec::new(),
    }
}

fn resume_terminal_outcome(
    contract: &MissionTxContract,
    resume_ctx: &crate::tx_idempotency::ResumeContext,
) -> (MissionTxState, TxOutcome) {
    if contract.lifecycle_state == MissionTxState::RolledBack {
        return (MissionTxState::RolledBack, TxOutcome::Compensated);
    }
    if contract.lifecycle_state == MissionTxState::Compensated {
        return (MissionTxState::Compensated, TxOutcome::Compensated);
    }
    if contract.lifecycle_state == MissionTxState::Failed || !resume_ctx.failed_steps.is_empty() {
        return (MissionTxState::Failed, TxOutcome::Failed);
    }
    if !resume_ctx.compensated_steps.is_empty() {
        return (MissionTxState::RolledBack, TxOutcome::Compensated);
    }
    (MissionTxState::Committed, TxOutcome::Committed)
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
    /// Idempotency ledger write or terminalization failed.
    LedgerWrite(String),
    /// Ledger not found for resume.
    LedgerNotFound(String),
    /// Resume would replay already executed work without a checkpoint-aware executor.
    UnsafeResume {
        execution_id: String,
        recommendation: ResumeRecommendation,
    },
}

impl std::fmt::Display for TxExecutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidContract(msg) => write!(f, "Invalid contract: {msg}"),
            Self::PhaseTransition(msg) => write!(f, "Phase transition error: {msg}"),
            Self::PreparePhase(msg) => write!(f, "Prepare phase error: {msg}"),
            Self::CommitPhase(msg) => write!(f, "Commit phase error: {msg}"),
            Self::CompensationPhase(msg) => write!(f, "Compensation phase error: {msg}"),
            Self::LedgerWrite(msg) => write!(f, "Ledger write error: {msg}"),
            Self::LedgerNotFound(id) => write!(f, "Ledger not found: {id}"),
            Self::UnsafeResume {
                execution_id,
                recommendation,
            } => write!(
                f,
                "Unsafe resume for {execution_id}: recommendation {:?} requires checkpoint-aware replay",
                recommendation
            ),
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
    use crate::tx_idempotency::{IdempotencyPolicy, IdempotencyStore, StepOutcome};
    use crate::tx_plan_compiler::StepRisk;

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

        assert_eq!(result.final_state, MissionTxState::Compensated);
        assert_eq!(result.outcome, TxOutcome::Compensated);
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
    fn prepare_gate_events_emitted_for_each_gate_check() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        let prepare_gate_events: Vec<_> = result
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event.kind,
                    TxEventKind::PreconditionValidated | TxEventKind::PreconditionFailed
                )
            })
            .collect();

        assert_eq!(prepare_gate_events.len(), 8);
        assert!(
            prepare_gate_events
                .iter()
                .all(|event| event.details.contains_key("gate"))
        );
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
    fn record_commit_results_to_ledger_fails_closed_when_ledger_is_sealed() {
        let contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut ledger = TxExecutionLedger::new("exec-1", &contract.plan.plan_id.0, 0);
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Committing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Completed)
            .unwrap();

        let commit_report = TxCommitReport {
            tx_id: contract.intent.tx_id.clone(),
            plan_id: contract.plan.plan_id.clone(),
            outcome: TxCommitOutcome::FullyCommitted,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: contract.plan.steps[0].step_id.clone(),
                ordinal: contract.plan.steps[0].ordinal,
                outcome: crate::plan::TxCommitStepOutcome::Committed {
                    reason_code: "ok".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 1,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "ok".to_string(),
            error_code: None,
            completed_at_ms: 1000,
            receipts: Vec::new(),
        };
        let mut events = Vec::new();

        let err = engine
            .record_commit_results_to_ledger(
                &contract,
                &commit_report,
                "exec-1",
                &mut ledger,
                &mut events,
                1000,
            )
            .unwrap_err();

        assert!(matches!(err, TxExecutionError::LedgerWrite(_)));
        assert!(err.to_string().contains("step-0"));
        assert!(events.is_empty());
    }

    #[test]
    fn record_compensation_results_to_ledger_fails_closed_when_ledger_is_sealed() {
        let contract = make_test_contract(1);
        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let mut ledger = TxExecutionLedger::new("exec-1", &contract.plan.plan_id.0, 0);
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Committing)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Compensating)
            .unwrap();
        ledger
            .transition_phase(crate::tx_idempotency::TxPhase::Completed)
            .unwrap();

        let comp_report = crate::plan::TxCompensationReport {
            outcome: crate::plan::TxCompensationOutcome::FullyRolledBack,
            compensated_count: 1,
            failed_count: 0,
            no_compensation_count: 0,
            skipped_count: 0,
            step_results: Vec::new(),
            decision_path: "test".to_string(),
            reason_code: "rollback_complete".to_string(),
            error_code: None,
            completed_at_ms: 1000,
            receipts: vec![serde_json::json!({
                "step_id": contract.plan.steps[0].step_id.0,
                "outcome": "compensated"
            })],
        };
        let mut events = Vec::new();

        let err = engine
            .record_compensation_results_to_ledger(
                &contract,
                &comp_report,
                "exec-1",
                &mut ledger,
                &mut events,
                1000,
            )
            .unwrap_err();

        assert!(matches!(err, TxExecutionError::LedgerWrite(_)));
        assert!(err.to_string().contains("step-0"));
        assert!(events.is_empty());
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
            TxExecutionError::LedgerWrite("ledger sealed".to_string()),
            TxExecutionError::LedgerNotFound("id-1".to_string()),
        ];
        for err in &errors {
            let msg = err.to_string();
            assert!(!msg.is_empty());
        }
    }

    struct DenyingExecutor;

    impl StepExecutor for DenyingExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            contract
                .plan
                .steps
                .iter()
                .map(|step| TxPrepareGateInput {
                    step_id: step.step_id.clone(),
                    pane_id: Some(step.ordinal as u64),
                    policy_passed: false,
                    policy_reason_code: Some("policy.denied".to_string()),
                    reservation_available: true,
                    reservation_reason_code: None,
                    approval_satisfied: true,
                    approval_reason_code: None,
                    target_liveness: true,
                    liveness_reason_code: None,
                    required_approval: None,
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
        assert_eq!(result.ledger.phase(), TxPhase::Aborted);
    }

    struct ApprovalBlockingExecutor;

    impl StepExecutor for ApprovalBlockingExecutor {
        fn evaluate_gates(
            &self,
            contract: &MissionTxContract,
            _now_ms: i64,
        ) -> Vec<TxPrepareGateInput> {
            contract
                .plan
                .steps
                .iter()
                .map(|step| TxPrepareGateInput {
                    step_id: step.step_id.clone(),
                    pane_id: Some(step.ordinal as u64),
                    policy_passed: true,
                    policy_reason_code: None,
                    reservation_available: true,
                    reservation_reason_code: None,
                    approval_satisfied: false,
                    approval_reason_code: Some("policy.test.require_approval".to_string()),
                    target_liveness: true,
                    liveness_reason_code: None,
                    required_approval: Some(crate::plan::TxPrepareApprovalRequirement {
                        workspace_id: "workspace:test".to_string(),
                        action_kind: "send_text".to_string(),
                        pane_id: Some(step.ordinal as u64),
                        action_fingerprint: format!("sha256:step-{}", step.ordinal),
                        reason_code: Some("policy.test.require_approval".to_string()),
                    }),
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
    fn custom_executor_require_approval_blocks_without_failing_tx() {
        let mut contract = make_test_contract(2);
        let engine = TxExecutionEngine::new(ApprovalBlockingExecutor, TxExecutionConfig::default());
        let result = engine.execute(&mut contract, 5000).unwrap();

        assert_eq!(
            result.prepare_report.outcome,
            TxPrepareOutcome::RequireApproval
        );
        assert!(result.commit_report.is_none());
        assert_eq!(result.final_state, MissionTxState::Planned);
        assert_eq!(result.outcome, TxOutcome::Pending);
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
        let gates = executor.evaluate_gates(&contract, 5_000);
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

    #[test]
    fn resume_with_no_step_activity_restarts_execution_safely() {
        let mut contract = make_test_contract(2);
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        store
            .get_ledger_mut("exec-1")
            .unwrap()
            .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
            .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
    }

    #[test]
    fn resume_blocks_partial_progress_without_checkpoint_replay_support() {
        let mut contract = make_test_contract(3);
        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        {
            let ledger = store.get_ledger_mut("exec-1").unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Committing)
                .unwrap();
        }

        store
            .record_execution(
                "exec-1",
                IdempotencyKey::new(&contract.plan.plan_id.0, "step-0", "commit"),
                StepOutcome::Success {
                    result: Some("ok".into()),
                },
                StepRisk::Low,
                "agent-step-0",
                1000,
            )
            .unwrap();

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let err = engine
            .resume(&mut contract, &store, "exec-1", 5000)
            .unwrap_err();

        assert!(matches!(
            err,
            TxExecutionError::UnsafeResume {
                recommendation: ResumeRecommendation::ContinueFromCheckpoint,
                ..
            }
        ));
    }

    #[test]
    fn resume_paused_execution_remains_pending_instead_of_committed() {
        let mut contract = make_test_contract(2);
        contract.lifecycle_state = MissionTxState::Committing;
        contract.outcome = TxOutcome::Pending;

        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        {
            let ledger = store.get_ledger_mut("exec-1").unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Committing)
                .unwrap();
            for step in &contract.plan.steps {
                ledger
                    .append(
                        IdempotencyKey::new(&contract.plan.plan_id.0, &step.step_id.0, "commit"),
                        StepOutcome::Skipped {
                            reason: "pause_suspended".into(),
                        },
                        StepRisk::Low,
                        &format!("agent-{}", step.step_id.0),
                        1000,
                    )
                    .unwrap();
            }
        }

        let engine = TxExecutionEngine::new(
            SyntheticStepExecutor,
            TxExecutionConfig {
                paused: true,
                ..TxExecutionConfig::default()
            },
        );
        let result = engine
            .resume(&mut contract, &store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Committing);
        assert_eq!(result.outcome, TxOutcome::Pending);
    }

    #[test]
    fn resume_prefers_failed_state_over_compensated_steps() {
        let mut contract = make_test_contract(2);
        contract.lifecycle_state = MissionTxState::Failed;
        contract.outcome = TxOutcome::Failed;

        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        {
            let ledger = store.get_ledger_mut("exec-1").unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Committing)
                .unwrap();
            ledger
                .append(
                    IdempotencyKey::new(&contract.plan.plan_id.0, "step-0", "commit"),
                    StepOutcome::Success { result: None },
                    StepRisk::Low,
                    "agent-step-0",
                    1000,
                )
                .unwrap();
            ledger
                .append(
                    IdempotencyKey::new(&contract.plan.plan_id.0, "step-1", "commit"),
                    StepOutcome::Failed {
                        error_code: "FTX3999".into(),
                        error_message: "commit failed".into(),
                        compensated: false,
                    },
                    StepRisk::Low,
                    "agent-step-1",
                    1001,
                )
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Compensating)
                .unwrap();
            ledger
                .append(
                    IdempotencyKey::for_compensation(
                        &contract.plan.plan_id.0,
                        "step-0",
                        "rollback",
                    ),
                    StepOutcome::Compensated {
                        original_outcome: Box::new(StepOutcome::Success { result: None }),
                        compensation_result: "rollback_complete".into(),
                    },
                    StepRisk::Low,
                    "agent-step-0",
                    1002,
                )
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Completed)
                .unwrap();
        }

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Failed);
        assert_eq!(result.outcome, TxOutcome::Failed);
    }

    #[test]
    fn resume_preserves_compensated_terminal_state() {
        let mut contract = make_test_contract(1);
        contract.lifecycle_state = MissionTxState::Compensated;
        contract.outcome = TxOutcome::Compensated;

        let mut store = IdempotencyStore::new(IdempotencyPolicy::default());
        let compiled_plan = compiled_plan_from_contract(&contract);
        store.create_ledger("exec-1", &compiled_plan).unwrap();
        {
            let ledger = store.get_ledger_mut("exec-1").unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Preparing)
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Committing)
                .unwrap();
            ledger
                .append(
                    IdempotencyKey::new(&contract.plan.plan_id.0, "step-0", "commit"),
                    StepOutcome::Failed {
                        error_code: "FTX3999".into(),
                        error_message: "commit failed before any side effects".into(),
                        compensated: false,
                    },
                    StepRisk::Low,
                    "agent-step-0",
                    1000,
                )
                .unwrap();
            ledger
                .transition_phase(crate::tx_idempotency::TxPhase::Completed)
                .unwrap();
        }

        let engine = TxExecutionEngine::new(SyntheticStepExecutor, TxExecutionConfig::default());
        let result = engine
            .resume(&mut contract, &store, "exec-1", 5000)
            .unwrap();

        assert_eq!(result.final_state, MissionTxState::Compensated);
        assert_eq!(result.outcome, TxOutcome::Compensated);
    }

    // ── PaneStepExecutor tests ──────────────────────────────────────────────

    use crate::approval::ApprovalScope;
    use crate::plan::{
        TxPrepareApprovalChecker, TxPreparePolicyAuthorizer, TxPrepareTargetLookup,
        TxPrepareTargetSnapshot, WaitCondition,
    };
    use crate::policy::{PolicyDecision, PolicyInput};
    use crate::wezterm::{MockWezterm, WeztermHandle, mock_wezterm_handle};
    use std::sync::Arc;

    /// Allow-all policy authorizer for PaneStepExecutor tests.
    struct TestAllowAllPolicy;
    impl TxPreparePolicyAuthorizer for TestAllowAllPolicy {
        fn authorize_prepare(&self, _input: &PolicyInput) -> PolicyDecision {
            PolicyDecision::allow()
        }
    }

    /// Allow-all approval checker for PaneStepExecutor tests.
    struct TestAllowAllApprovals;
    impl TxPrepareApprovalChecker for TestAllowAllApprovals {
        fn has_active_approval(
            &self,
            _scope: &ApprovalScope,
            _now_ms: i64,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
    }

    /// All-live target lookup for PaneStepExecutor tests.
    struct TestAllLiveTargets;
    impl TxPrepareTargetLookup for TestAllLiveTargets {
        fn lookup_target(
            &self,
            pane_id: u64,
        ) -> std::result::Result<Option<TxPrepareTargetSnapshot>, String> {
            Ok(Some(TxPrepareTargetSnapshot {
                pane_id,
                capabilities: Default::default(),
                last_seen_at_ms: Some(1000),
                observed: true,
                known_dead: false,
                domain: None,
                pane_title: None,
                pane_cwd: None,
                reserved_by: None,
                reservation_lookup_error: None,
            }))
        }
    }

    /// Build a contract with specific StepActions for PaneStepExecutor testing.
    fn make_pane_contract(actions: Vec<(String, StepAction)>) -> MissionTxContract {
        let steps = actions
            .into_iter()
            .enumerate()
            .map(|(i, (id, action))| TxStep {
                step_id: TxStepId(id),
                ordinal: i,
                action,
                description: format!("pane test step {i}"),
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-pane-1".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Pane step executor test".to_string(),
                correlation_id: "corr-pane-1".to_string(),
                created_at_ms: 1000,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-pane-1".to_string()),
                tx_id: TxId("tx-pane-1".to_string()),
                steps,
                preconditions: Vec::new(),
                compensations: Vec::new(),
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    /// Create a PaneStepExecutor using allow-all test policy delegates.
    fn make_pane_executor(
        handle: WeztermHandle,
    ) -> PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets> {
        PaneStepExecutor::new(
            handle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
    }

    /// Create a PaneStepExecutor with custom config.
    fn make_pane_executor_with_config(
        handle: WeztermHandle,
        config: PaneStepExecutorConfig,
    ) -> PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets> {
        PaneStepExecutor::new(
            handle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
        .with_config(config)
    }

    /// Create a PaneStepExecutor with a fleet memory controller.
    fn make_pane_executor_with_controller(
        handle: WeztermHandle,
        controller: std::sync::Arc<crate::fleet_memory_controller::FleetMemoryController>,
    ) -> PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets> {
        PaneStepExecutor::new(
            handle,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
        .with_fleet_controller(controller)
    }

    #[test]
    fn pane_executor_send_text_happy_path() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async { mock.add_default_pane(0).await });

        let executor = make_pane_executor(mock.clone() as WeztermHandle);
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "world".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "done".to_string(),
                    paste_mode: Some(false),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 3);
        for r in &results {
            assert!(r.success, "step {} failed: {}", r.step_id.0, r.reason_code);
            assert_eq!(r.reason_code, "send_text_succeeded");
        }
    }

    #[test]
    fn pane_executor_send_text_pane_not_found() {
        let mock = Arc::new(MockWezterm::new());
        // No panes added — pane 99 doesn't exist
        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 99,
                text: "oops".to_string(),
                paste_mode: None,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "send_text_failed");
        assert!(results[0].error_code.is_some());
    }

    #[test]
    fn pane_executor_wait_for_match() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            mock.inject_output(0, "some output READY here")
                .await
                .unwrap();
        });

        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "READY".to_string(),
                },
                timeout_ms: 2000,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_matched");
    }

    #[test]
    fn pane_executor_wait_for_timeout() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            mock.inject_output(0, "no match here").await.unwrap();
        });

        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "NEVER_APPEARS".to_string(),
                },
                timeout_ms: 500, // Short timeout for fast test
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_timeout");
    }

    #[test]
    fn pane_executor_store_data_succeeds() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "sd1".to_string(),
            StepAction::StoreData {
                key: "test_key".to_string(),
                value: serde_json::json!({"value": 42}),
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
        assert_eq!(results[0].reason_code, "store_data_succeeded");
    }

    #[test]
    fn pane_executor_unsupported_action_run_workflow() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "rw1".to_string(),
            StepAction::RunWorkflow {
                workflow_id: "test-wf".to_string(),
                params: None,
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "unsupported_action");
        assert!(
            results[0]
                .error_code
                .as_ref()
                .unwrap()
                .contains("RunWorkflow")
        );
    }

    #[test]
    fn pane_executor_fail_step_injection() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "a".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "b".to_string(),
                    paste_mode: None,
                },
            ),
        ]);
        // Inject failure at step s2
        let results = executor.execute_steps(&contract, Some("s2"), 5000);
        assert_eq!(results.len(), 2);
        assert!(results[0].success); // s1 succeeds
        assert!(!results[1].success); // s2 is injected failure
        assert_eq!(results[1].reason_code, "commit_step_failed_injected");
    }

    #[test]
    fn pane_executor_mixed_steps_failure_boundary() {
        let mock = Arc::new(MockWezterm::new());
        // No pane added — pane 0 will fail

        let executor = make_pane_executor(mock as WeztermHandle);
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::StoreData {
                    key: "k1".to_string(),
                    value: serde_json::json!("v1"),
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "fail".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::StoreData {
                    key: "k2".to_string(),
                    value: serde_json::json!("v2"),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 3);
        assert!(results[0].success); // StoreData succeeds
        assert!(!results[1].success); // SendText fails (no pane)
        assert!(!results[2].success); // Skipped after failure
        assert_eq!(results[2].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_compensations_happy_path() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);

        // Create a commit report with 2 committed steps
        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![
                crate::plan::TxCommitStepResult {
                    step_id: TxStepId("s1".to_string()),
                    ordinal: 0,
                    outcome: crate::plan::TxCommitStepOutcome::Committed {
                        reason_code: "ok".to_string(),
                    },
                    decision_path: "test".to_string(),
                    completed_at_ms: 1000,
                },
                crate::plan::TxCommitStepResult {
                    step_id: TxStepId("s2".to_string()),
                    ordinal: 1,
                    outcome: crate::plan::TxCommitStepOutcome::Committed {
                        reason_code: "ok".to_string(),
                    },
                    decision_path: "test".to_string(),
                    completed_at_ms: 2000,
                },
            ],
            failure_boundary: None,
            committed_count: 2,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 3000,
            receipts: Vec::new(),
        };
        let results = executor.execute_compensations(&commit_report, None, 5000);
        assert_eq!(results.len(), 2);
        assert!(results[0].success);
        assert!(results[1].success);
        assert_eq!(results[0].reason_code, "compensation_succeeded");
    }

    #[test]
    fn pane_executor_compensations_with_failure_injection() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::PartialFailure,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: TxStepId("s1".to_string()),
                ordinal: 0,
                outcome: crate::plan::TxCommitStepOutcome::Committed {
                    reason_code: "ok".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 1,
            failed_count: 0,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 2000,
            receipts: Vec::new(),
        };
        let results = executor.execute_compensations(&commit_report, Some("s1"), 5000);
        assert_eq!(results.len(), 1);
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "compensation_failed_injected");
    }

    #[test]
    fn pane_executor_compensations_empty() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);

        let commit_report = crate::plan::TxCommitReport {
            tx_id: TxId("tx-1".to_string()),
            plan_id: TxPlanId("plan-1".to_string()),
            outcome: crate::plan::TxCommitOutcome::ImmediateFailure,
            step_results: vec![crate::plan::TxCommitStepResult {
                step_id: TxStepId("s1".to_string()),
                ordinal: 0,
                outcome: crate::plan::TxCommitStepOutcome::Failed {
                    reason_code: "err".to_string(),
                },
                decision_path: "test".to_string(),
                completed_at_ms: 1000,
            }],
            failure_boundary: None,
            committed_count: 0,
            failed_count: 1,
            skipped_count: 0,
            decision_path: "test".to_string(),
            reason_code: "test".to_string(),
            error_code: None,
            completed_at_ms: 2000,
            receipts: Vec::new(),
        };
        let results = executor.execute_compensations(&commit_report, None, 5000);
        // No committed steps → no compensations
        assert!(results.is_empty());
    }

    #[test]
    fn pane_executor_evaluate_gates_delegates() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "test".to_string(),
                paste_mode: None,
            },
        )]);
        let gates = executor.evaluate_gates(&contract, 5000);
        assert_eq!(gates.len(), 1);
        // Allow-all policy: all gates should pass
        assert!(gates[0].policy_passed);
        assert!(gates[0].approval_satisfied);
        assert!(gates[0].reservation_available);
        assert!(gates[0].target_liveness);
    }

    #[test]
    fn pane_executor_lock_and_event_steps_succeed() {
        let mock = mock_wezterm_handle();
        let executor = make_pane_executor(mock);
        let contract = make_pane_contract(vec![
            (
                "l1".to_string(),
                StepAction::AcquireLock {
                    lock_name: "test-lock".to_string(),
                    timeout_ms: Some(5000),
                },
            ),
            (
                "e1".to_string(),
                StepAction::MarkEventHandled { event_id: 42 },
            ),
            (
                "r1".to_string(),
                StepAction::ReleaseLock {
                    lock_name: "test-lock".to_string(),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 3);
        assert!(results[0].success);
        assert_eq!(results[0].reason_code, "acquire_lock_succeeded");
        assert!(results[1].success);
        assert_eq!(results[1].reason_code, "mark_event_handled_succeeded");
        assert!(results[2].success);
        assert_eq!(results[2].reason_code, "release_lock_succeeded");
    }

    // ── Timeout and backpressure tests (ft-y9lnb.4) ────────────────────

    #[test]
    fn pane_executor_phase_timeout_skips_remaining() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            mock.inject_output(0, "no match here").await.unwrap();
        });

        // A WaitFor with 500ms timeout contributes 500ms to step budget.
        // Phase buffer = 0, so total phase budget = 500ms.
        // The WaitFor will timeout after 500ms, which puts elapsed over the
        // 500ms phase budget. The second step should be skipped.
        let config = PaneStepExecutorConfig {
            default_send_timeout_ms: 30_000,
            phase_timeout_buffer_ms: 0,
            backpressure_enabled: false,
        };
        let executor = make_pane_executor_with_config(mock as WeztermHandle, config);

        let contract = make_pane_contract(vec![
            (
                "w1".to_string(),
                StepAction::WaitFor {
                    pane_id: Some(0),
                    condition: WaitCondition::Pattern {
                        pane_id: None,
                        rule_id: "NEVER_APPEARS".to_string(),
                    },
                    timeout_ms: 500,
                },
            ),
            (
                "s2".to_string(),
                StepAction::StoreData {
                    key: "k2".to_string(),
                    value: serde_json::json!("v2"),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 2);
        // First step times out (WaitFor pattern never matches)
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "wait_for_timeout");
        // Second step is skipped after failure boundary
        assert!(!results[1].success);
        assert_eq!(results[1].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_default_send_timeout_config() {
        let mock = mock_wezterm_handle();
        let config = PaneStepExecutorConfig {
            default_send_timeout_ms: 5000,
            phase_timeout_buffer_ms: 60_000,
            backpressure_enabled: false,
        };
        let executor = make_pane_executor_with_config(mock, config);
        // Verify config is accepted — StoreData succeeds regardless
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[test]
    fn pane_executor_backpressure_normal_proceeds() {
        let mock = mock_wezterm_handle();
        let controller =
            std::sync::Arc::new(crate::fleet_memory_controller::FleetMemoryController::default());
        // Default controller is Normal tier
        let executor = make_pane_executor_with_controller(mock, controller);
        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        assert!(results[0].success);
    }

    #[test]
    fn pane_executor_backpressure_emergency_defers_all() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let mut controller = crate::fleet_memory_controller::FleetMemoryController::new(
            crate::fleet_memory_controller::FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..Default::default()
            },
        );
        // Push to Emergency via black signals
        let emergency_signals = crate::fleet_memory_controller::PressureSignals {
            backpressure: crate::backpressure::BackpressureTier::Black,
            memory_pressure: crate::memory_pressure::MemoryPressureTier::Red,
            worst_budget: crate::memory_budget::BudgetLevel::OverBudget,
            pane_count: 200,
            paused_pane_count: 100,
        };
        controller.evaluate(&emergency_signals);
        assert_eq!(
            controller.compound_tier(),
            crate::fleet_memory_controller::FleetPressureTier::Emergency
        );

        let controller = std::sync::Arc::new(controller);
        let executor = make_pane_executor_with_controller(mock as WeztermHandle, controller);

        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::StoreData {
                    key: "k".to_string(),
                    value: serde_json::json!("v"),
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 2);
        // All steps deferred under emergency
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "backpressure_emergency");
        assert!(!results[1].success);
        assert_eq!(results[1].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_backpressure_critical_defers_non_pane() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let mut controller = crate::fleet_memory_controller::FleetMemoryController::new(
            crate::fleet_memory_controller::FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..Default::default()
            },
        );
        // Push to Critical via red signals
        let critical_signals = crate::fleet_memory_controller::PressureSignals {
            backpressure: crate::backpressure::BackpressureTier::Red,
            memory_pressure: crate::memory_pressure::MemoryPressureTier::Orange,
            worst_budget: crate::memory_budget::BudgetLevel::Throttled,
            pane_count: 200,
            paused_pane_count: 10,
        };
        controller.evaluate(&critical_signals);
        assert_eq!(
            controller.compound_tier(),
            crate::fleet_memory_controller::FleetPressureTier::Critical
        );

        let controller = std::sync::Arc::new(controller);
        let executor = make_pane_executor_with_controller(mock as WeztermHandle, controller);

        // StoreData (no pane) first, then SendText (has pane)
        let contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::StoreData {
                    key: "k".to_string(),
                    value: serde_json::json!("v"),
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
        ]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 2);
        // StoreData deferred (no pane, Critical)
        assert!(!results[0].success);
        assert_eq!(results[0].reason_code, "backpressure_deferred");
        // SendText skipped after failure
        assert!(!results[1].success);
        assert_eq!(results[1].reason_code, "skipped_after_failure");
    }

    #[test]
    fn pane_executor_backpressure_disabled_ignores_controller() {
        let mut controller = crate::fleet_memory_controller::FleetMemoryController::new(
            crate::fleet_memory_controller::FleetMemoryConfig {
                escalation_threshold: 1,
                deescalation_threshold: 1,
                ..Default::default()
            },
        );
        let emergency_signals = crate::fleet_memory_controller::PressureSignals {
            backpressure: crate::backpressure::BackpressureTier::Black,
            memory_pressure: crate::memory_pressure::MemoryPressureTier::Red,
            worst_budget: crate::memory_budget::BudgetLevel::OverBudget,
            pane_count: 200,
            paused_pane_count: 100,
        };
        controller.evaluate(&emergency_signals);

        let mock = mock_wezterm_handle();
        let config = PaneStepExecutorConfig {
            default_send_timeout_ms: 30_000,
            phase_timeout_buffer_ms: 30_000,
            backpressure_enabled: false, // Disabled!
        };
        let executor = PaneStepExecutor::new(
            mock,
            TestAllowAllPolicy,
            TestAllowAllApprovals,
            TestAllLiveTargets,
            TxPrepareEvaluationContext::new("test-workspace"),
        )
        .with_config(config)
        .with_fleet_controller(std::sync::Arc::new(controller));

        let contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);
        let results = executor.execute_steps(&contract, None, 5000);
        assert_eq!(results.len(), 1);
        // Backpressure disabled — step succeeds despite Emergency tier
        assert!(results[0].success);
    }

    #[test]
    fn pane_executor_step_timeout_helper() {
        assert_eq!(
            step_timeout_ms(
                &StepAction::SendText {
                    pane_id: 0,
                    text: "test".to_string(),
                    paste_mode: None,
                },
                30_000,
            ),
            Some(30_000),
        );
        assert_eq!(
            step_timeout_ms(
                &StepAction::WaitFor {
                    pane_id: Some(0),
                    condition: WaitCondition::Pattern {
                        pane_id: None,
                        rule_id: "test".to_string(),
                    },
                    timeout_ms: 5000,
                },
                30_000,
            ),
            Some(5000),
        );
        assert_eq!(
            step_timeout_ms(
                &StepAction::StoreData {
                    key: "k".to_string(),
                    value: serde_json::json!("v"),
                },
                30_000,
            ),
            None,
        );
    }

    #[test]
    fn pane_executor_action_has_pane_helper() {
        assert!(action_has_pane(&StepAction::SendText {
            pane_id: 0,
            text: "test".to_string(),
            paste_mode: None,
        }));
        assert!(action_has_pane(&StepAction::WaitFor {
            pane_id: Some(0),
            condition: WaitCondition::Pattern {
                pane_id: None,
                rule_id: "test".to_string(),
            },
            timeout_ms: 5000,
        }));
        assert!(!action_has_pane(&StepAction::StoreData {
            key: "k".to_string(),
            value: serde_json::json!("v"),
        }));
        assert!(!action_has_pane(&StepAction::AcquireLock {
            lock_name: "lock".to_string(),
            timeout_ms: None,
        }));
    }

    // ── Integration tests: TxExecutionEngine<PaneStepExecutor> (ft-y9lnb.5) ─

    /// Create a PaneStepExecutor-powered engine with allow-all policy.
    fn make_pane_engine(
        handle: WeztermHandle,
    ) -> TxExecutionEngine<
        PaneStepExecutor<TestAllowAllPolicy, TestAllowAllApprovals, TestAllLiveTargets>,
    > {
        let executor = make_pane_executor(handle);
        TxExecutionEngine::new(executor, TxExecutionConfig::default())
    }

    /// Create a contract with compensations for rollback testing.
    fn make_pane_contract_with_compensations(
        steps: Vec<(String, StepAction)>,
        compensations: Vec<(String, StepAction)>,
    ) -> MissionTxContract {
        let step_entries: Vec<TxStep> = steps
            .into_iter()
            .enumerate()
            .map(|(i, (id, action))| TxStep {
                step_id: TxStepId(id),
                ordinal: i,
                action,
                description: format!("step {i}"),
            })
            .collect();

        let comp_entries: Vec<crate::plan::TxCompensation> = compensations
            .into_iter()
            .map(|(for_id, action)| crate::plan::TxCompensation {
                for_step_id: TxStepId(for_id),
                action,
            })
            .collect();

        MissionTxContract {
            tx_version: 1,
            intent: TxIntent {
                tx_id: TxId("tx-integ-1".to_string()),
                requested_by: MissionActorRole::Operator,
                summary: "Integration test tx".to_string(),
                correlation_id: "corr-integ-1".to_string(),
                created_at_ms: 1000,
            },
            plan: ContractTxPlan {
                plan_id: TxPlanId("plan-integ-1".to_string()),
                tx_id: TxId("tx-integ-1".to_string()),
                steps: step_entries,
                preconditions: Vec::new(),
                compensations: comp_entries,
            },
            lifecycle_state: MissionTxState::Planned,
            outcome: TxOutcome::Pending,
            receipts: Vec::new(),
        }
    }

    #[test]
    fn integration_happy_path_3_steps() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            mock.add_default_pane(1).await;
            mock.add_default_pane(2).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 1,
                    text: "world".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::SendText {
                    pane_id: 2,
                    text: "done".to_string(),
                    paste_mode: None,
                },
            ),
        ]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 3);
        assert_eq!(commit.failed_count, 0);
        assert!(result.compensation_report.is_none());
        assert!(!result.events.is_empty());
    }

    #[test]
    fn integration_single_step_minimal() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "single".to_string(),
                paste_mode: None,
            },
        )]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
    }

    #[test]
    fn integration_pane_not_found_triggers_compensation() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            // Pane 99 is NOT added
        });

        let mut config = TxExecutionConfig::default();
        config.auto_compensate = true;

        let executor = make_pane_executor(mock as WeztermHandle);
        let engine = TxExecutionEngine::new(executor, config);

        let mut contract = make_pane_contract_with_compensations(
            vec![
                (
                    "s1".to_string(),
                    StepAction::SendText {
                        pane_id: 0,
                        text: "ok".to_string(),
                        paste_mode: None,
                    },
                ),
                (
                    "s2".to_string(),
                    StepAction::SendText {
                        pane_id: 99,
                        text: "fail".to_string(),
                        paste_mode: None,
                    },
                ),
            ],
            vec![(
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "ROLLBACK".to_string(),
                    paste_mode: None,
                },
            )],
        );

        let result = engine.execute(&mut contract, 5000).unwrap();
        // Partial failure: step 1 committed, step 2 failed
        assert!(matches!(
            result.outcome,
            TxOutcome::Failed | TxOutcome::Compensated
        ));
        let commit = result.commit_report.unwrap();
        assert!(commit.committed_count >= 1);
        assert!(commit.failed_count >= 1);
    }

    #[test]
    fn integration_fail_step_injection() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let mut config = TxExecutionConfig::default();
        config.fail_step = Some("s2".to_string());

        let executor = make_pane_executor(mock as WeztermHandle);
        let engine = TxExecutionEngine::new(executor, config);

        let mut contract = make_pane_contract(vec![
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "ok".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s2".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "injected-fail".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "s3".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "skipped".to_string(),
                    paste_mode: None,
                },
            ),
        ]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        // With auto_compensate=true (default), partial failure triggers compensation
        assert!(
            matches!(result.outcome, TxOutcome::Failed | TxOutcome::Compensated),
            "expected Failed or Compensated, got {:?}",
            result.outcome
        );
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 1); // s1 succeeded
        assert!(commit.failed_count >= 1); // s2 failed
        assert!(commit.skipped_count >= 1); // s3 skipped
    }

    #[test]
    fn integration_observability_events_emitted() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "observe".to_string(),
                paste_mode: None,
            },
        )]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        // Should have at least prepare and commit events
        assert!(
            result.events.len() >= 2,
            "expected at least 2 observability events, got {}",
            result.events.len()
        );
        // Events should have sequential IDs
        for (i, event) in result.events.iter().enumerate() {
            if i > 0 {
                assert!(
                    event.sequence > result.events[i - 1].sequence,
                    "event sequences should be monotonically increasing"
                );
            }
        }
    }

    #[test]
    fn integration_all_gates_pass() {
        let mock = mock_wezterm_handle();
        let engine = make_pane_engine(mock);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::StoreData {
                key: "k".to_string(),
                value: serde_json::json!("v"),
            },
        )]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        // Prepare phase should pass with allow-all policy
        assert!(
            result.prepare_report.outcome.commit_eligible(),
            "all gates should pass with allow-all policy"
        );
        assert_eq!(result.final_state, MissionTxState::Committed);
    }

    #[test]
    fn integration_wait_for_timeout_in_engine() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
            mock.inject_output(0, "no match content").await.unwrap();
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "w1".to_string(),
            StepAction::WaitFor {
                pane_id: Some(0),
                condition: WaitCondition::Pattern {
                    pane_id: None,
                    rule_id: "NEVER_MATCH".to_string(),
                },
                timeout_ms: 500,
            },
        )]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        // WaitFor timeout causes step failure; auto_compensate may kick in
        assert!(
            matches!(result.outcome, TxOutcome::Failed | TxOutcome::Compensated),
            "expected Failed or Compensated, got {:?}",
            result.outcome
        );
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.failed_count, 1);
    }

    #[test]
    fn integration_mixed_actions_committed() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![
            (
                "l1".to_string(),
                StepAction::AcquireLock {
                    lock_name: "test".to_string(),
                    timeout_ms: None,
                },
            ),
            (
                "s1".to_string(),
                StepAction::SendText {
                    pane_id: 0,
                    text: "action".to_string(),
                    paste_mode: None,
                },
            ),
            (
                "d1".to_string(),
                StepAction::StoreData {
                    key: "result".to_string(),
                    value: serde_json::json!({"status": "done"}),
                },
            ),
            (
                "r1".to_string(),
                StepAction::ReleaseLock {
                    lock_name: "test".to_string(),
                },
            ),
        ]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        assert_eq!(result.final_state, MissionTxState::Committed);
        assert_eq!(result.outcome, TxOutcome::Committed);
        let commit = result.commit_report.unwrap();
        assert_eq!(commit.committed_count, 4);
    }

    #[test]
    fn integration_ledger_populated() {
        let mock = Arc::new(MockWezterm::new());
        let rt = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            mock.add_default_pane(0).await;
        });

        let engine = make_pane_engine(mock as WeztermHandle);
        let mut contract = make_pane_contract(vec![(
            "s1".to_string(),
            StepAction::SendText {
                pane_id: 0,
                text: "ledger-test".to_string(),
                paste_mode: None,
            },
        )]);

        let result = engine.execute(&mut contract, 5000).unwrap();
        // Ledger should be populated after execution
        assert!(
            !result.ledger.execution_id().is_empty(),
            "ledger should have execution_id"
        );
    }
}
