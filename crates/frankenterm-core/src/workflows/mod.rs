//! Durable workflow execution engine
//!
//! Provides idempotent, recoverable, audited workflow execution.
//!
//! # Architecture
//!
//! Workflows are explicit state machines with a uniform execution model:
//! - **Workflow trait**: Defines the workflow interface (name, steps, execution)
//! - **WorkflowContext**: Runtime context with WezTerm client, storage, pane state
//! - **StepResult**: Step outcomes (continue, done, retry, abort, wait)
//! - **WaitCondition**: Conditions to pause execution (pattern, idle, external)
//!
//! This design enables:
//! - Persistent/resumable workflows
//! - Deterministic step logic testing
//! - Shared runner across agent-specific workflows

mod account_steps;
mod codex_exit;
mod context;
mod descriptors;
mod lock;
mod step_results;
mod traits;
mod wait_execution;
pub use account_steps::*;
pub use codex_exit::*;
pub use context::*;
pub use descriptors::*;
pub use lock::*;
pub use step_results::*;
pub use traits::*;
pub use wait_execution::*;

use crate::cass::{CassAgent, CassClient, CassSearchHit, SearchOptions};
use crate::policy::{InjectionResult, PaneCapabilities, Redactor};
use crate::runtime_compat::sleep;
use crate::storage::StorageHandle;
use crate::wezterm::{
    CodexSummaryWaitResult, PaneTextSource, PaneWaiter, WaitMatcher, WaitOptions, WaitResult,
    WeztermHandleSource, default_wezterm_handle, stable_hash, tail_text,
    wait_for_codex_session_summary,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Type alias for a boxed future used in dyn-compatible traits.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
type PolicyInjector = crate::policy::PolicyGatedInjector<crate::wezterm::WeztermHandle>;
type PolicyInjectorHandle = Arc<crate::runtime_compat::Mutex<PolicyInjector>>;

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// ============================================================================
// Workflow Execution State
// ============================================================================

/// Workflow execution state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    /// Unique execution ID
    pub id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane being operated on
    pub pane_id: u64,
    /// Current step index
    pub current_step: usize,
    /// Status
    pub status: ExecutionStatus,
    /// Started at timestamp
    pub started_at: i64,
    /// Last updated timestamp
    pub updated_at: i64,
}

/// Workflow execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// Running
    Running,
    /// Waiting for condition
    Waiting,
    /// Completed successfully
    Completed,
    /// Aborted with error
    Aborted,
}

/// Workflow engine for managing executions
pub struct WorkflowEngine {
    /// Maximum concurrent workflows
    max_concurrent: usize,
}

impl Default for WorkflowEngine {
    fn default() -> Self {
        Self::new(3)
    }
}

impl WorkflowEngine {
    /// Create a new workflow engine
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self { max_concurrent }
    }

    /// Get the maximum concurrent workflows setting
    #[must_use]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Start a new workflow execution and persist it to storage
    ///
    /// Creates a new execution record with status 'running' and step 0.
    /// Returns the execution which can be used with `DurableWorkflowRunner`.
    pub async fn start(
        &self,
        storage: &crate::storage::StorageHandle,
        workflow_name: &str,
        pane_id: u64,
        trigger_event_id: Option<i64>,
        context: Option<serde_json::Value>,
    ) -> crate::Result<WorkflowExecution> {
        let execution_id = generate_workflow_id(workflow_name);
        self.start_with_id(
            storage,
            execution_id,
            workflow_name,
            pane_id,
            trigger_event_id,
            context,
        )
        .await
    }

    /// Start a workflow execution using a caller-provided execution_id.
    ///
    /// This is used by `WorkflowRunner` so the lock execution_id matches the persisted DB id.
    pub async fn start_with_id(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: String,
        workflow_name: &str,
        pane_id: u64,
        trigger_event_id: Option<i64>,
        context: Option<serde_json::Value>,
    ) -> crate::Result<WorkflowExecution> {
        let now = now_ms();

        let record = crate::storage::WorkflowRecord {
            id: execution_id.clone(),
            workflow_name: workflow_name.to_string(),
            pane_id,
            trigger_event_id,
            current_step: 0,
            status: "running".to_string(),
            wait_condition: None,
            context,
            result: None,
            error: None,
            started_at: now,
            updated_at: now,
            completed_at: None,
        };

        storage.upsert_workflow(record).await?;

        Ok(WorkflowExecution {
            id: execution_id,
            workflow_name: workflow_name.to_string(),
            pane_id,
            current_step: 0,
            status: ExecutionStatus::Running,
            started_at: now,
            updated_at: now,
        })
    }

    /// Resume a workflow execution from storage
    ///
    /// Loads the workflow record and step logs to determine the next step.
    /// Returns None if the workflow doesn't exist or is already completed.
    pub async fn resume(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: &str,
    ) -> crate::Result<Option<(WorkflowExecution, usize)>> {
        // Load the workflow record
        let Some(record) = storage.get_workflow(execution_id).await? else {
            return Ok(None);
        };

        // Check if already completed
        if record.status == "completed" || record.status == "aborted" {
            return Ok(None);
        }

        // Load step logs to find the last completed step
        let step_logs = storage.get_step_logs(execution_id).await?;
        let next_step = compute_next_step(&step_logs);

        let execution = WorkflowExecution {
            id: record.id,
            workflow_name: record.workflow_name,
            pane_id: record.pane_id,
            current_step: next_step,
            status: match record.status.as_str() {
                "waiting" => ExecutionStatus::Waiting,
                _ => ExecutionStatus::Running,
            },
            started_at: record.started_at,
            updated_at: record.updated_at,
        };

        Ok(Some((execution, next_step)))
    }

    /// Find all incomplete workflows for resume on restart
    pub async fn find_incomplete(
        &self,
        storage: &crate::storage::StorageHandle,
    ) -> crate::Result<Vec<crate::storage::WorkflowRecord>> {
        storage.find_incomplete_workflows().await
    }

    /// Update workflow status
    pub async fn update_status(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: &str,
        status: ExecutionStatus,
        current_step: usize,
        wait_condition: Option<&WaitCondition>,
        error: Option<&str>,
    ) -> crate::Result<()> {
        let now = now_ms();
        let status_str = match status {
            ExecutionStatus::Running => "running",
            ExecutionStatus::Waiting => "waiting",
            ExecutionStatus::Completed => "completed",
            ExecutionStatus::Aborted => "aborted",
        };

        // Load existing record to preserve fields
        let Some(existing) = storage.get_workflow(execution_id).await? else {
            return Err(crate::error::WorkflowError::NotFound(execution_id.to_string()).into());
        };

        let record = crate::storage::WorkflowRecord {
            id: existing.id,
            workflow_name: existing.workflow_name,
            pane_id: existing.pane_id,
            trigger_event_id: existing.trigger_event_id,
            current_step,
            status: status_str.to_string(),
            wait_condition: wait_condition.map(|wc| serde_json::to_value(wc).unwrap_or_default()),
            context: existing.context,
            result: existing.result,
            error: error.map(String::from),
            started_at: existing.started_at,
            updated_at: now,
            completed_at: if status == ExecutionStatus::Completed
                || status == ExecutionStatus::Aborted
            {
                Some(now)
            } else {
                None
            },
        };

        storage.upsert_workflow(record).await
    }

    /// Record a step log entry
    pub async fn log_step(
        &self,
        storage: &crate::storage::StorageHandle,
        execution_id: &str,
        step_index: usize,
        step_name: &str,
        result: &StepResult,
        started_at: i64,
    ) -> crate::Result<()> {
        let completed_at = now_ms();
        let result_type = match result {
            StepResult::Continue => "continue",
            StepResult::Done { .. } => "done",
            StepResult::Abort { .. } => "abort",
            StepResult::Retry { .. } => "retry",
            StepResult::WaitFor { .. } => "wait_for",
            StepResult::SendText { .. } => "send_text",
            StepResult::JumpTo { .. } => "jump_to",
        };
        let result_data = serde_json::to_string(result)
            .inspect_err(
                |e| tracing::warn!(error = %e, "workflow step result serialization failed"),
            )
            .ok();
        let verification_refs = build_verification_refs(result, None);
        let error_code = step_error_code_from_result(result);

        storage
            .insert_step_log(
                execution_id,
                None,
                step_index,
                step_name,
                None,
                None,
                result_type,
                result_data,
                None,
                verification_refs,
                error_code,
                started_at,
                completed_at,
            )
            .await
    }
}

/// Compute the next step index from step logs
///
/// Finds the highest completed step index and returns the next one.
/// If no steps are completed, returns 0.
fn compute_next_step(step_logs: &[crate::storage::WorkflowStepLogRecord]) -> usize {
    if step_logs.is_empty() {
        return 0;
    }

    // Find the highest step index with a terminal result (continue or done)
    // Steps with retry or wait_for should be re-executed
    let mut max_completed = None;
    for log in step_logs {
        if log.result_type == "continue" || log.result_type == "done" {
            max_completed =
                Some(max_completed.map_or(log.step_index, |m: usize| m.max(log.step_index)));
        }
    }

    max_completed.map_or(0, |idx| idx + 1)
}

/// Generate a unique workflow execution ID
fn generate_workflow_id(workflow_name: &str) -> String {
    let timestamp = now_ms();
    let random: u32 = rand::random();
    format!("{workflow_name}-{timestamp}-{random:08x}")
}

/// Get current timestamp in milliseconds
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn build_verification_refs(
    step_result: &StepResult,
    step_plan: Option<&crate::plan::StepPlan>,
) -> Option<String> {
    let mut refs: Vec<serde_json::Value> = Vec::new();

    if let Some(step_plan) = step_plan {
        if let Some(verification) = &step_plan.verification {
            refs.push(serde_json::json!({
                "source": "plan",
                "strategy": &verification.strategy,
                "description": verification.description,
                "timeout_ms": verification.timeout_ms,
            }));
        }
    }

    match step_result {
        StepResult::WaitFor {
            condition,
            timeout_ms,
        } => {
            refs.push(serde_json::json!({
                "source": "wait_for",
                "condition": condition,
                "timeout_ms": timeout_ms,
            }));
        }
        StepResult::SendText {
            wait_for: Some(condition),
            wait_timeout_ms,
            ..
        } => {
            refs.push(serde_json::json!({
                "source": "post_send_wait",
                "condition": condition,
                "timeout_ms": wait_timeout_ms,
            }));
        }
        _ => {}
    }

    if refs.is_empty() {
        None
    } else {
        serde_json::to_string(&refs)
            .inspect_err(
                |e| tracing::warn!(error = %e, "workflow verification_refs serialization failed"),
            )
            .ok()
    }
}

fn redact_text_for_log(text: &str, max_len: usize) -> String {
    let redactor = Redactor::new();
    let redacted = redactor.redact(text);
    if redacted.len() <= max_len {
        return redacted;
    }
    let mut truncated = redacted.chars().take(max_len).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn redacted_step_result_for_logging(step_result: &StepResult) -> StepResult {
    match step_result {
        StepResult::SendText {
            text,
            wait_for,
            wait_timeout_ms,
        } => StepResult::SendText {
            text: redact_text_for_log(text, 160),
            wait_for: wait_for.clone(),
            wait_timeout_ms: *wait_timeout_ms,
        },
        _ => step_result.clone(),
    }
}

fn step_error_code_from_result(step_result: &StepResult) -> Option<String> {
    match step_result {
        StepResult::Abort { .. } => Some("FT-5002".to_string()),
        _ => None,
    }
}

fn policy_summary_from_injection(result: &crate::policy::InjectionResult) -> Option<String> {
    use crate::policy::InjectionResult;

    let mut obj = serde_json::Map::new();
    match result {
        InjectionResult::Allowed {
            decision,
            summary,
            action,
            ..
        } => {
            obj.insert("decision".to_string(), serde_json::json!("allow"));
            if let Ok(action_val) = serde_json::to_value(action) {
                obj.insert("action".to_string(), action_val);
            }
            if let Some(rule_id) = decision.rule_id() {
                obj.insert("rule_id".to_string(), serde_json::json!(rule_id));
            }
            obj.insert("summary".to_string(), serde_json::json!(summary));
        }
        InjectionResult::Denied {
            decision,
            summary,
            action,
            ..
        } => {
            obj.insert("decision".to_string(), serde_json::json!("deny"));
            if let Ok(action_val) = serde_json::to_value(action) {
                obj.insert("action".to_string(), action_val);
            }
            if let Some(rule_id) = decision.rule_id() {
                obj.insert("rule_id".to_string(), serde_json::json!(rule_id));
            }
            if let Some(reason) = decision.denial_reason() {
                obj.insert("reason".to_string(), serde_json::json!(reason));
            }
            obj.insert("summary".to_string(), serde_json::json!(summary));
        }
        InjectionResult::RequiresApproval {
            decision,
            summary,
            action,
            ..
        } => {
            obj.insert(
                "decision".to_string(),
                serde_json::json!("require_approval"),
            );
            if let Ok(action_val) = serde_json::to_value(action) {
                obj.insert("action".to_string(), action_val);
            }
            if let Some(rule_id) = decision.rule_id() {
                obj.insert("rule_id".to_string(), serde_json::json!(rule_id));
            }
            if let crate::policy::PolicyDecision::RequireApproval { reason, .. } = decision {
                obj.insert("reason".to_string(), serde_json::json!(reason));
            }
            obj.insert("summary".to_string(), serde_json::json!(summary));
        }
        InjectionResult::Error { error, action, .. } => {
            obj.insert("decision".to_string(), serde_json::json!("error"));
            if let Ok(action_val) = serde_json::to_value(action) {
                obj.insert("action".to_string(), action_val);
            }
            obj.insert("error".to_string(), serde_json::json!(error));
        }
    }

    if obj.is_empty() {
        None
    } else {
        serde_json::to_string(&obj)
            .inspect_err(
                |e| tracing::warn!(error = %e, "workflow decision_context serialization failed"),
            )
            .ok()
    }
}

fn policy_error_code_from_decision(
    decision: &crate::policy::PolicyDecision,
) -> Option<&'static str> {
    if matches!(
        decision,
        crate::policy::PolicyDecision::RequireApproval { .. }
    ) {
        return Some("FT-4010");
    }
    match decision.rule_id() {
        Some("policy.alt_screen" | "policy.alt_screen_unknown") => Some("FT-4001"),
        Some("policy.prompt_required" | "policy.prompt_unknown") => Some("FT-4002"),
        Some("policy.rate_limit") => Some("FT-4003"),
        _ => None,
    }
}

fn policy_error_code_from_injection(result: &crate::policy::InjectionResult) -> Option<String> {
    match result {
        crate::policy::InjectionResult::Denied { decision, .. }
        | crate::policy::InjectionResult::RequiresApproval { decision, .. } => {
            policy_error_code_from_decision(decision).map(str::to_string)
        }
        _ => None,
    }
}

async fn record_workflow_action(
    storage: &crate::storage::StorageHandle,
    action_kind: &str,
    execution_id: &str,
    pane_id: u64,
    _workflow_name: &str,
    input_summary: Option<String>,
    result: &str,
    decision_reason: Option<String>,
) -> Option<i64> {
    let action = crate::storage::AuditActionRecord {
        id: 0,
        ts: now_ms(),
        actor_kind: "workflow".to_string(),
        actor_id: Some(execution_id.to_string()),
        correlation_id: None,
        pane_id: Some(pane_id),
        domain: None,
        action_kind: action_kind.to_string(),
        policy_decision: "allow".to_string(),
        decision_reason,
        rule_id: None,
        input_summary,
        verification_summary: None,
        decision_context: None,
        result: result.to_string(),
    };

    match storage.record_audit_action_redacted(action).await {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                execution_id,
                action_kind,
                error = %e,
                "Failed to record workflow audit action"
            );
            None
        }
    }
}

async fn record_workflow_start_action(
    storage: &crate::storage::StorageHandle,
    workflow_name: &str,
    execution_id: &str,
    pane_id: u64,
    step_count: usize,
    start_step: usize,
) -> Option<i64> {
    let summary = serde_json::json!({
        "workflow_name": workflow_name,
        "execution_id": execution_id,
        "step_count": step_count,
        "start_step": start_step,
    });
    let summary = serde_json::to_string(&summary)
        .inspect_err(|e| tracing::warn!(error = %e, "workflow start summary serialization failed"))
        .ok();
    let action_id = record_workflow_action(
        storage,
        "workflow_start",
        execution_id,
        pane_id,
        workflow_name,
        summary,
        "started",
        None,
    )
    .await?;

    let undo_payload = serde_json::json!({
        "execution_id": execution_id,
        "workflow_name": workflow_name,
    });
    let undo = crate::storage::ActionUndoRecord {
        audit_action_id: action_id,
        undoable: true,
        undo_strategy: "workflow_abort".to_string(),
        undo_hint: Some(format!("ft robot workflow abort {execution_id}")),
        undo_payload: serde_json::to_string(&undo_payload)
            .inspect_err(
                |e| tracing::warn!(error = %e, "workflow undo_payload serialization failed"),
            )
            .ok(),
        undone_at: None,
        undone_by: None,
    };
    if let Err(e) = storage.upsert_action_undo_redacted(undo).await {
        tracing::warn!(
            execution_id,
            error = %e,
            "Failed to record workflow undo metadata"
        );
    }

    Some(action_id)
}

async fn fetch_workflow_start_action_id(
    storage: &crate::storage::StorageHandle,
    execution_id: &str,
) -> Option<i64> {
    let query = crate::storage::AuditQuery {
        limit: Some(1),
        actor_id: Some(execution_id.to_string()),
        action_kind: Some("workflow_start".to_string()),
        ..Default::default()
    };
    storage
        .get_audit_actions(query)
        .await
        .ok()
        .and_then(|mut rows| rows.pop().map(|row| row.id))
}

async fn record_workflow_step_action(
    storage: &crate::storage::StorageHandle,
    workflow_name: &str,
    execution_id: &str,
    pane_id: u64,
    step_index: usize,
    step_name: &str,
    step_id: Option<String>,
    step_kind: Option<String>,
    result_type: &str,
    parent_action_id: Option<i64>,
) -> Option<i64> {
    let summary = serde_json::json!({
        "workflow_name": workflow_name,
        "execution_id": execution_id,
        "step_index": step_index,
        "step_name": step_name,
        "step_id": step_id,
        "step_kind": step_kind,
        "result_type": result_type,
        "parent_action_id": parent_action_id,
    });
    let summary = serde_json::to_string(&summary)
        .inspect_err(|e| tracing::warn!(error = %e, "workflow step summary serialization failed"))
        .ok();
    record_workflow_action(
        storage,
        "workflow_step",
        execution_id,
        pane_id,
        workflow_name,
        summary,
        result_type,
        None,
    )
    .await
}

async fn record_workflow_terminal_action(
    storage: &crate::storage::StorageHandle,
    workflow_name: &str,
    execution_id: &str,
    pane_id: u64,
    action_kind: &str,
    result: &str,
    reason: Option<&str>,
    step_index: Option<usize>,
    steps_executed: Option<usize>,
    start_action_id: Option<i64>,
) {
    let summary = serde_json::json!({
        "workflow_name": workflow_name,
        "execution_id": execution_id,
        "reason": reason,
        "step_index": step_index,
        "steps_executed": steps_executed,
        "parent_action_id": start_action_id,
    });
    let summary = serde_json::to_string(&summary)
        .inspect_err(
            |e| tracing::warn!(error = %e, "workflow terminal summary serialization failed"),
        )
        .ok();
    let _ = record_workflow_action(
        storage,
        action_kind,
        execution_id,
        pane_id,
        workflow_name,
        summary,
        result,
        reason.map(str::to_string),
    )
    .await;

    if let Some(start_action_id) = start_action_id {
        let undo = crate::storage::ActionUndoRecord {
            audit_action_id: start_action_id,
            undoable: false,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: Some("workflow no longer running".to_string()),
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        if let Err(e) = storage.upsert_action_undo_redacted(undo).await {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to update workflow undo metadata"
            );
        }
    }
}

// ============================================================================
// Plan-first Execution Helpers (wa-upg.2.3)
// ============================================================================

/// Generate an ActionPlan from a workflow definition.
///
/// This helper creates a complete ActionPlan using the workflow's step metadata.
/// Workflows can use this as a base and then customize the plan.
///
/// # Arguments
/// * `workflow` - The workflow to generate a plan for
/// * `workspace_id` - The workspace scope for the plan
/// * `pane_id` - Target pane ID
/// * `execution_id` - The workflow execution ID (used in metadata)
pub fn workflow_to_action_plan(
    workflow: &dyn Workflow,
    workspace_id: &str,
    pane_id: u64,
    execution_id: &str,
) -> crate::plan::ActionPlan {
    let steps = workflow.steps_to_plans(pane_id);

    crate::plan::ActionPlan::builder(workflow.description(), workspace_id)
        .add_steps(steps)
        .metadata(serde_json::json!({
            "workflow_name": workflow.name(),
            "execution_id": execution_id,
            "pane_id": pane_id,
            "generated_by": "workflow_to_action_plan",
        }))
        .created_at(now_ms())
        .build()
}

/// Result of checking a step's idempotency.
#[derive(Debug, Clone)]
pub enum IdempotencyCheckResult {
    /// Step has not been executed before - proceed with execution
    NotExecuted,
    /// Step was already executed successfully - skip
    AlreadyCompleted {
        /// When the step was completed
        completed_at: i64,
        /// Result from the previous execution
        previous_result: Option<String>,
    },
    /// Step was started but not completed - may need recovery
    PartiallyExecuted {
        /// When the step was started
        started_at: i64,
    },
}

/// Check if a step has already been executed based on its idempotency key.
///
/// This enables safe replay by checking the step log for previous executions.
pub async fn check_step_idempotency(
    storage: &StorageHandle,
    execution_id: &str,
    idempotency_key: &crate::plan::IdempotencyKey,
    step_index: usize,
) -> IdempotencyCheckResult {
    // Query step logs for this execution
    let Ok(logs) = storage.get_step_logs(execution_id).await else {
        return IdempotencyCheckResult::NotExecuted;
    };

    let mut latest_completed: Option<(i64, Option<String>)> = None;
    let mut latest_started: Option<i64> = None;

    // Find logs for this step by index and idempotency key
    for log in logs {
        if log.step_index != step_index {
            continue;
        }

        let key_matches = if let Some(step_id) = log.step_id.as_deref() {
            step_id == idempotency_key.0.as_str()
        } else if let Some(ref result_data) = log.result_data {
            serde_json::from_str::<serde_json::Value>(result_data)
                .ok()
                .and_then(|data| {
                    data.get("idempotency_key")
                        .and_then(|v| v.as_str())
                        .map(|key| key == idempotency_key.0.as_str())
                })
                .unwrap_or(false)
        } else {
            false
        };

        if !key_matches {
            continue;
        }

        let is_completed = match log.result_type.as_str() {
            "continue" | "done" => true,
            "send_text" => {
                if let Some(ref summary) = log.policy_summary {
                    serde_json::from_str::<serde_json::Value>(summary)
                        .ok()
                        .and_then(|data| {
                            data.get("decision")
                                .and_then(|v| v.as_str())
                                .map(|decision| decision == "allow")
                        })
                        .unwrap_or(true)
                } else {
                    true
                }
            }
            _ => false,
        };

        if is_completed {
            let should_replace = latest_completed
                .as_ref()
                .is_none_or(|(ts, _)| log.completed_at > *ts);
            if should_replace {
                latest_completed = Some((log.completed_at, log.result_data.clone()));
            }
        } else {
            let should_replace = latest_started
                .as_ref()
                .is_none_or(|ts| log.started_at > *ts);
            if should_replace {
                latest_started = Some(log.started_at);
            }
        }
    }

    if let Some((completed_at, previous_result)) = latest_completed {
        return IdempotencyCheckResult::AlreadyCompleted {
            completed_at,
            previous_result,
        };
    }

    if let Some(started_at) = latest_started {
        return IdempotencyCheckResult::PartiallyExecuted { started_at };
    }

    IdempotencyCheckResult::NotExecuted
}

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// ============================================================================
// Multi-Pane Coordination Primitives (wa-nu4.4.4.1)
// ============================================================================

/// Strategy for grouping panes into coordination groups.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneGroupStrategy {
    /// Group by WezTerm domain (e.g., "local", "SSH:host").
    ByDomain,
    /// Group by inferred agent type (codex, claude_code, etc.).
    ByAgent,
    /// Group by project directory (cwd-based).
    ByProject,
    /// Explicit list of pane IDs.
    Explicit { pane_ids: Vec<u64> },
}

/// A named group of panes selected by a grouping strategy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneGroup {
    /// Group name (e.g., domain name, agent type, project path).
    pub name: String,
    /// Pane IDs belonging to this group.
    pub pane_ids: Vec<u64>,
    /// Strategy used to form this group.
    pub strategy: PaneGroupStrategy,
}

impl PaneGroup {
    /// Create a new pane group.
    #[must_use]
    pub fn new(name: impl Into<String>, pane_ids: Vec<u64>, strategy: PaneGroupStrategy) -> Self {
        Self {
            name: name.into(),
            pane_ids,
            strategy,
        }
    }

    /// Number of panes in this group.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pane_ids.len()
    }

    /// Whether this group is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pane_ids.is_empty()
    }
}

/// Build pane groups from a list of pane records using the given strategy.
///
/// Returns groups sorted deterministically by name.
pub fn build_pane_groups(
    panes: &[crate::storage::PaneRecord],
    strategy: &PaneGroupStrategy,
) -> Vec<PaneGroup> {
    use std::collections::BTreeMap;

    match strategy {
        PaneGroupStrategy::ByDomain => {
            let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for pane in panes {
                groups
                    .entry(pane.domain.clone())
                    .or_default()
                    .push(pane.pane_id);
            }
            groups
                .into_iter()
                .map(|(name, mut pane_ids)| {
                    pane_ids.sort_unstable();
                    PaneGroup::new(name, pane_ids, PaneGroupStrategy::ByDomain)
                })
                .collect()
        }
        PaneGroupStrategy::ByAgent => {
            let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for pane in panes {
                let agent = pane
                    .title
                    .as_deref()
                    .and_then(infer_agent_from_title)
                    .unwrap_or("unknown")
                    .to_string();
                groups.entry(agent).or_default().push(pane.pane_id);
            }
            groups
                .into_iter()
                .map(|(name, mut pane_ids)| {
                    pane_ids.sort_unstable();
                    PaneGroup::new(name, pane_ids, PaneGroupStrategy::ByAgent)
                })
                .collect()
        }
        PaneGroupStrategy::ByProject => {
            let mut groups: BTreeMap<String, Vec<u64>> = BTreeMap::new();
            for pane in panes {
                let project = pane.cwd.as_deref().unwrap_or("unknown").to_string();
                groups.entry(project).or_default().push(pane.pane_id);
            }
            groups
                .into_iter()
                .map(|(name, mut pane_ids)| {
                    pane_ids.sort_unstable();
                    PaneGroup::new(name, pane_ids, PaneGroupStrategy::ByProject)
                })
                .collect()
        }
        PaneGroupStrategy::Explicit { pane_ids } => {
            let mut sorted = pane_ids.clone();
            sorted.sort_unstable();
            vec![PaneGroup::new("explicit", sorted, strategy.clone())]
        }
    }
}

/// Simple agent inference from pane title.
fn infer_agent_from_title(title: &str) -> Option<&'static str> {
    let lower = title.to_lowercase();
    if lower.contains("codex") {
        Some("codex")
    } else if lower.contains("claude") {
        Some("claude_code")
    } else if lower.contains("gemini") {
        Some("gemini")
    } else {
        None
    }
}

/// Result of attempting to acquire group locks.
#[derive(Debug, Clone)]
pub enum GroupLockResult {
    /// All pane locks acquired successfully.
    Acquired {
        /// Pane IDs that were locked.
        locked_panes: Vec<u64>,
    },
    /// Some panes were already locked; acquisition was rolled back.
    PartialFailure {
        /// Panes that were successfully locked (then released during rollback).
        would_have_locked: Vec<u64>,
        /// Panes that were already locked by other workflows.
        conflicts: Vec<GroupLockConflict>,
    },
}

impl GroupLockResult {
    /// Whether all locks were acquired.
    #[must_use]
    pub fn is_acquired(&self) -> bool {
        matches!(self, Self::Acquired { .. })
    }
}

/// Information about a lock conflict during group acquisition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupLockConflict {
    /// Pane that couldn't be locked.
    pub pane_id: u64,
    /// Workflow currently holding the lock.
    pub held_by_workflow: String,
    /// Execution ID holding the lock.
    pub held_by_execution: String,
}

impl PaneWorkflowLockManager {
    /// Attempt to acquire locks for all panes in a group (all-or-nothing).
    ///
    /// If any pane is already locked, all acquired locks are rolled back
    /// and `PartialFailure` is returned with conflict details.
    pub fn try_acquire_group(
        &self,
        pane_ids: &[u64],
        workflow_name: &str,
        execution_id: &str,
    ) -> GroupLockResult {
        let mut acquired = Vec::new();
        let mut conflicts = Vec::new();

        for &pane_id in pane_ids {
            match self.try_acquire(pane_id, workflow_name, execution_id) {
                LockAcquisitionResult::Acquired => {
                    acquired.push(pane_id);
                }
                LockAcquisitionResult::AlreadyLocked {
                    held_by_workflow,
                    held_by_execution,
                    ..
                } => {
                    conflicts.push(GroupLockConflict {
                        pane_id,
                        held_by_workflow,
                        held_by_execution,
                    });
                }
            }
        }

        if conflicts.is_empty() {
            GroupLockResult::Acquired {
                locked_panes: acquired,
            }
        } else {
            // Rollback: release all locks we acquired
            for pane_id in &acquired {
                self.release(*pane_id, execution_id);
            }
            GroupLockResult::PartialFailure {
                would_have_locked: acquired,
                conflicts,
            }
        }
    }

    /// Release locks for all panes in a group.
    pub fn release_group(&self, pane_ids: &[u64], execution_id: &str) -> usize {
        let mut released = 0;
        for &pane_id in pane_ids {
            if self.release(pane_id, execution_id) {
                released += 1;
            }
        }
        released
    }
}

/// Precondition that a pane must satisfy before a broadcast action is executed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BroadcastPrecondition {
    /// Pane must have a shell prompt active (from OSC 133).
    PromptActive,
    /// Pane must NOT be in alternate screen mode (vim, less, etc.).
    NotAltScreen,
    /// Pane must NOT have a recent capture gap.
    NoRecentGap,
    /// Pane must NOT be reserved by another workflow.
    NotReserved,
}

impl BroadcastPrecondition {
    /// Check if the pane capabilities satisfy this precondition.
    #[must_use]
    pub fn check(&self, caps: &crate::policy::PaneCapabilities) -> bool {
        match self {
            Self::PromptActive => caps.prompt_active,
            Self::NotAltScreen => !caps.alt_screen.unwrap_or(false),
            Self::NoRecentGap => !caps.has_recent_gap,
            Self::NotReserved => !caps.is_reserved,
        }
    }

    /// Human-readable label for this precondition.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::PromptActive => "prompt_active",
            Self::NotAltScreen => "not_alt_screen",
            Self::NoRecentGap => "no_recent_gap",
            Self::NotReserved => "not_reserved",
        }
    }
}

/// Default safe broadcast preconditions.
///
/// These prevent "spray and pray" broadcasting:
/// - Prompt must be active
/// - Not in alternate screen
/// - No recent capture gap
/// - Not reserved by another workflow
#[must_use]
pub fn default_broadcast_preconditions() -> Vec<BroadcastPrecondition> {
    vec![
        BroadcastPrecondition::PromptActive,
        BroadcastPrecondition::NotAltScreen,
        BroadcastPrecondition::NoRecentGap,
        BroadcastPrecondition::NotReserved,
    ]
}

/// Check all preconditions against pane capabilities.
///
/// Returns a list of failed precondition labels.
pub fn check_preconditions(
    preconditions: &[BroadcastPrecondition],
    caps: &crate::policy::PaneCapabilities,
) -> Vec<&'static str> {
    preconditions
        .iter()
        .filter(|p| !p.check(caps))
        .map(|p| p.label())
        .collect()
}

/// Outcome of a broadcast action on a single pane.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PaneBroadcastOutcome {
    /// Action was allowed and executed.
    Allowed {
        /// Time taken for this pane's action in milliseconds.
        elapsed_ms: u64,
    },
    /// Action was denied by policy.
    Denied {
        /// Reason for denial.
        reason: String,
    },
    /// Preconditions were not met.
    PreconditionFailed {
        /// List of failed precondition labels.
        failed: Vec<String>,
    },
    /// Action was skipped (pane was locked by another workflow).
    Skipped {
        /// Reason for skipping.
        reason: String,
    },
    /// Verification after action failed.
    VerificationFailed {
        /// What went wrong during verification.
        reason: String,
    },
}

/// Full broadcast result across all targeted panes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroadcastResult {
    /// Workflow or action name.
    pub action: String,
    /// Per-pane outcomes, keyed by pane ID.
    pub outcomes: Vec<PaneBroadcastEntry>,
    /// Total elapsed time in milliseconds.
    pub total_elapsed_ms: u64,
}

/// A single entry in a broadcast result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneBroadcastEntry {
    /// Pane ID.
    pub pane_id: u64,
    /// Outcome for this pane.
    pub outcome: PaneBroadcastOutcome,
}

impl BroadcastResult {
    /// Create a new broadcast result.
    #[must_use]
    pub fn new(action: impl Into<String>) -> Self {
        Self {
            action: action.into(),
            outcomes: Vec::new(),
            total_elapsed_ms: 0,
        }
    }

    /// Add a pane outcome.
    pub fn add_outcome(&mut self, pane_id: u64, outcome: PaneBroadcastOutcome) {
        self.outcomes.push(PaneBroadcastEntry { pane_id, outcome });
    }

    /// Count of panes where the action was allowed.
    #[must_use]
    pub fn allowed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::Allowed { .. }))
            .count()
    }

    /// Count of panes where the action was denied.
    #[must_use]
    pub fn denied_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::Denied { .. }))
            .count()
    }

    /// Count of panes where preconditions failed.
    #[must_use]
    pub fn precondition_failed_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::PreconditionFailed { .. }))
            .count()
    }

    /// Count of panes that were skipped.
    #[must_use]
    pub fn skipped_count(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|e| matches!(e.outcome, PaneBroadcastOutcome::Skipped { .. }))
            .count()
    }

    /// Whether all targeted panes were allowed.
    #[must_use]
    pub fn all_allowed(&self) -> bool {
        !self.outcomes.is_empty() && self.allowed_count() == self.outcomes.len()
    }
}

// ============================================================================
// Multi-Pane Coordination Workflows (wa-nu4.4.4.2)
// ============================================================================

/// Configuration for the `coordinate_agents` family of multi-pane workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinateAgentsConfig {
    /// Strategy for selecting which panes to target.
    pub strategy: PaneGroupStrategy,
    /// Preconditions each pane must meet before receiving a broadcast action.
    #[serde(default = "default_broadcast_preconditions")]
    pub preconditions: Vec<BroadcastPrecondition>,
    /// Whether to abort the entire operation if group lock acquisition fails.
    #[serde(default)]
    pub abort_on_lock_failure: bool,
}

impl Default for CoordinateAgentsConfig {
    fn default() -> Self {
        Self {
            strategy: PaneGroupStrategy::ByAgent,
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        }
    }
}

/// Agent-specific text to send for context refresh.
#[must_use]
pub fn agent_reread_prompt(agent_hint: &str) -> &'static str {
    match agent_hint {
        "codex" => "Read the AGENTS.md file and follow the instructions for resuming context.\n",
        "claude_code" => "/read AGENTS.md\n",
        "gemini" => "Please read AGENTS.md and follow any instructions for context recovery.\n",
        _ => "cat AGENTS.md\n",
    }
}

/// Agent-specific safe pause keystrokes.
#[must_use]
pub fn agent_pause_text(agent_hint: &str) -> &'static str {
    match agent_hint {
        // For AI coding agents, Ctrl-C is the safest interrupt
        "codex" | "claude_code" | "gemini" => "\x03",
        // For unknown panes, also Ctrl-C
        _ => "\x03",
    }
}

/// Result of a multi-pane coordination operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationResult {
    /// The operation that was performed.
    pub operation: String,
    /// Per-group results.
    pub groups: Vec<GroupCoordinationEntry>,
    /// Aggregate broadcast result across all groups.
    pub broadcast: BroadcastResult,
}

/// Per-group result within a coordination operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCoordinationEntry {
    /// Group name (e.g., domain name, agent type, project path).
    pub group_name: String,
    /// Number of panes in this group.
    pub pane_count: usize,
    /// Number of panes that received the action.
    pub acted_count: usize,
    /// Number of panes that failed preconditions.
    pub precondition_failed_count: usize,
    /// Number of panes that were skipped (lock conflicts, etc.).
    pub skipped_count: usize,
}

impl CoordinationResult {
    /// Create a new coordination result for the given operation.
    #[must_use]
    pub fn new(operation: impl Into<String>) -> Self {
        let op = operation.into();
        Self {
            operation: op.clone(),
            groups: Vec::new(),
            broadcast: BroadcastResult::new(op),
        }
    }

    /// Total panes that were successfully acted upon.
    #[must_use]
    pub fn total_acted(&self) -> usize {
        self.groups.iter().map(|g| g.acted_count).sum()
    }

    /// Total panes across all groups.
    #[must_use]
    pub fn total_panes(&self) -> usize {
        self.groups.iter().map(|g| g.pane_count).sum()
    }
}

/// Evaluate which panes in a group pass preconditions and return per-pane outcomes.
///
/// This is the core "filter before broadcast" logic that prevents spraying actions
/// to panes that aren't ready. Returns a vec of (pane_id, `Option<outcome>`) where
/// `None` means the pane passed all preconditions.
#[must_use]
pub fn evaluate_pane_preconditions<S: ::std::hash::BuildHasher>(
    pane_ids: &[u64],
    capabilities: &std::collections::HashMap<u64, crate::policy::PaneCapabilities, S>,
    preconditions: &[BroadcastPrecondition],
) -> Vec<(u64, Option<PaneBroadcastOutcome>)> {
    pane_ids
        .iter()
        .map(|&pid| {
            match capabilities.get(&pid) {
                Some(caps) => {
                    let failed = check_preconditions(preconditions, caps);
                    if failed.is_empty() {
                        (pid, None) // passed all preconditions
                    } else {
                        (
                            pid,
                            Some(PaneBroadcastOutcome::PreconditionFailed {
                                failed: failed.iter().map(|s| (*s).to_string()).collect(),
                            }),
                        )
                    }
                }
                None => (
                    pid,
                    Some(PaneBroadcastOutcome::Skipped {
                        reason: "no capabilities available for pane".to_string(),
                    }),
                ),
            }
        })
        .collect()
}

/// Plan a `reread_context` coordination: determine which panes would receive
/// a context refresh prompt and which would be filtered out.
///
/// This is a dry-run / planning function that does not execute any actions.
#[must_use]
pub fn plan_reread_context<S: ::std::hash::BuildHasher>(
    panes: &[crate::storage::PaneRecord],
    capabilities: &std::collections::HashMap<u64, crate::policy::PaneCapabilities, S>,
    config: &CoordinateAgentsConfig,
) -> CoordinationResult {
    let groups = build_pane_groups(panes, &config.strategy);
    let mut result = CoordinationResult::new("reread_context");

    for group in &groups {
        let evals =
            evaluate_pane_preconditions(&group.pane_ids, capabilities, &config.preconditions);
        let mut acted = 0usize;
        let mut precond_failed = 0usize;
        let mut skipped = 0usize;

        for (pid, outcome) in &evals {
            match outcome {
                None => {
                    acted += 1;
                    result
                        .broadcast
                        .add_outcome(*pid, PaneBroadcastOutcome::Allowed { elapsed_ms: 0 });
                }
                Some(o @ PaneBroadcastOutcome::PreconditionFailed { .. }) => {
                    precond_failed += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
                Some(o) => {
                    skipped += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
            }
        }

        result.groups.push(GroupCoordinationEntry {
            group_name: group.name.clone(),
            pane_count: group.pane_ids.len(),
            acted_count: acted,
            precondition_failed_count: precond_failed,
            skipped_count: skipped,
        });
    }

    result
}

/// Plan a `pause_all` coordination: determine which panes would receive
/// a safe pause signal.
#[must_use]
pub fn plan_pause_all<S: ::std::hash::BuildHasher>(
    panes: &[crate::storage::PaneRecord],
    capabilities: &std::collections::HashMap<u64, crate::policy::PaneCapabilities, S>,
    config: &CoordinateAgentsConfig,
) -> CoordinationResult {
    let groups = build_pane_groups(panes, &config.strategy);
    let mut result = CoordinationResult::new("pause_all");

    for group in &groups {
        // For pause_all, we only require NotAltScreen — we deliberately send
        // to panes even if a command is running (that's the point of pausing).
        let pause_preconditions: Vec<BroadcastPrecondition> = config
            .preconditions
            .iter()
            .filter(|p| matches!(p, BroadcastPrecondition::NotAltScreen))
            .cloned()
            .collect();

        let evals =
            evaluate_pane_preconditions(&group.pane_ids, capabilities, &pause_preconditions);
        let mut acted = 0usize;
        let mut precond_failed = 0usize;
        let mut skipped = 0usize;

        for (pid, outcome) in &evals {
            match outcome {
                None => {
                    acted += 1;
                    result
                        .broadcast
                        .add_outcome(*pid, PaneBroadcastOutcome::Allowed { elapsed_ms: 0 });
                }
                Some(o @ PaneBroadcastOutcome::PreconditionFailed { .. }) => {
                    precond_failed += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
                Some(o) => {
                    skipped += 1;
                    result.broadcast.add_outcome(*pid, o.clone());
                }
            }
        }

        result.groups.push(GroupCoordinationEntry {
            group_name: group.name.clone(),
            pane_count: group.pane_ids.len(),
            acted_count: acted,
            precondition_failed_count: precond_failed,
            skipped_count: skipped,
        });
    }

    result
}

/// Resolve the text to send for each pane in a `reread_context` operation.
///
/// Returns a map from pane_id to the prompt text, using agent-specific prompts
/// when the agent type can be inferred from the pane title.
#[must_use]
pub fn resolve_reread_prompts(
    panes: &[crate::storage::PaneRecord],
) -> std::collections::HashMap<u64, &'static str> {
    panes
        .iter()
        .map(|p| {
            let agent = p
                .title
                .as_deref()
                .and_then(infer_agent_from_title)
                .unwrap_or("unknown");
            (p.pane_id, agent_reread_prompt(agent))
        })
        .collect()
}

/// Resolve the text to send for each pane in a `pause_all` operation.
#[must_use]
pub fn resolve_pause_texts(
    panes: &[crate::storage::PaneRecord],
) -> std::collections::HashMap<u64, &'static str> {
    panes
        .iter()
        .map(|p| {
            let agent = p
                .title
                .as_deref()
                .and_then(infer_agent_from_title)
                .unwrap_or("unknown");
            (p.pane_id, agent_pause_text(agent))
        })
        .collect()
}

// ============================================================================
// Unstick Workflow: read-only code scanning (wa-nu4.4.4.4)
// ============================================================================

/// Category of code pattern scanned by the unstick workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnstickFindingKind {
    /// TODO / FIXME / HACK comment.
    TodoComment,
    /// `unwrap()` / `expect()` / `panic!()` call.
    PanicSite,
    /// Suspicious error handling (e.g., `let _ = ...` on Result).
    SuppressedError,
}

impl UnstickFindingKind {
    /// Human-readable label for display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TodoComment => "TODO/FIXME",
            Self::PanicSite => "panic site",
            Self::SuppressedError => "suppressed error",
        }
    }
}

/// A single finding from the unstick scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstickFinding {
    /// Category of the finding.
    pub kind: UnstickFindingKind,
    /// Relative file path from the repo root.
    pub file: String,
    /// One-based line number.
    pub line: u32,
    /// Short snippet of the matched code (bounded to 200 chars).
    pub snippet: String,
    /// Suggested next action for the agent.
    pub suggestion: String,
}

/// Configuration for the unstick scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstickConfig {
    /// Root directory to scan (must be absolute).
    pub root: std::path::PathBuf,
    /// Maximum number of findings per category.
    #[serde(default = "default_max_findings_per_kind")]
    pub max_findings_per_kind: usize,
    /// Maximum total findings across all categories.
    #[serde(default = "default_max_total_findings")]
    pub max_total_findings: usize,
    /// File extensions to scan (e.g., ["rs", "py", "ts"]).
    #[serde(default = "default_scan_extensions")]
    pub extensions: Vec<String>,
}

fn default_max_findings_per_kind() -> usize {
    10
}

fn default_max_total_findings() -> usize {
    25
}

fn default_scan_extensions() -> Vec<String> {
    vec![
        "rs".to_string(),
        "py".to_string(),
        "ts".to_string(),
        "js".to_string(),
        "go".to_string(),
    ]
}

impl Default for UnstickConfig {
    fn default() -> Self {
        Self {
            root: std::path::PathBuf::from("."),
            max_findings_per_kind: default_max_findings_per_kind(),
            max_total_findings: default_max_total_findings(),
            extensions: default_scan_extensions(),
        }
    }
}

/// Result of an unstick scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnstickReport {
    /// Findings grouped by kind.
    pub findings: Vec<UnstickFinding>,
    /// Total files scanned.
    pub files_scanned: usize,
    /// Whether the scan was truncated due to limits.
    pub truncated: bool,
    /// Which scanner was used ("ast-grep" or "text").
    pub scanner: String,
    /// Summary counts by kind.
    pub counts: std::collections::BTreeMap<String, usize>,
}

impl UnstickReport {
    /// Create an empty report.
    #[must_use]
    pub fn empty(scanner: &str) -> Self {
        Self {
            findings: Vec::new(),
            files_scanned: 0,
            truncated: false,
            scanner: scanner.to_string(),
            counts: std::collections::BTreeMap::new(),
        }
    }

    /// Total number of findings.
    #[must_use]
    pub fn total_findings(&self) -> usize {
        self.findings.len()
    }

    /// Format as a concise human-readable summary.
    #[must_use]
    pub fn human_summary(&self) -> String {
        if self.findings.is_empty() {
            return "No actionable findings.".to_string();
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "Found {} items across {} files (scanner: {}):",
            self.findings.len(),
            self.files_scanned,
            self.scanner,
        ));

        for (kind, count) in &self.counts {
            lines.push(format!("  {kind}: {count}"));
        }

        lines.push(String::new());

        // Show top findings (up to 10)
        for (i, f) in self.findings.iter().take(10).enumerate() {
            lines.push(format!(
                "  {}. [{}] {}:{} — {}",
                i + 1,
                f.kind.label(),
                f.file,
                f.line,
                truncate_snippet(&f.snippet, 80),
            ));
            lines.push(format!("     → {}", f.suggestion));
        }

        if self.findings.len() > 10 {
            lines.push(format!(
                "  ... and {} more (use --format json for full list)",
                self.findings.len() - 10
            ));
        }

        if self.truncated {
            lines.push("  (results truncated due to limits)".to_string());
        }

        lines.join("\n")
    }
}

/// Truncate a snippet to a max length, adding "..." if needed.
fn truncate_snippet(s: &str, max_len: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_len {
        trimmed.to_string()
    } else if max_len == 0 {
        String::new()
    } else if max_len <= 3 {
        ".".repeat(max_len)
    } else {
        let prefix_budget = max_len - 3;
        let mut end = 0usize;
        for ch in trimmed.chars() {
            let next = end.saturating_add(ch.len_utf8());
            if next > prefix_budget {
                break;
            }
            end = next;
        }

        let mut out = trimmed[..end].to_string();
        out.push_str("...");
        out
    }
}

/// Regex patterns for text-based scanning (fallback when ast-grep is not available).
///
/// Hoisted into `LazyLock` statics so each regex is compiled exactly once.
struct TextScanPatterns {
    todo: &'static regex::Regex,
    panic_site: &'static regex::Regex,
    suppressed_error: &'static regex::Regex,
}

static TEXT_SCAN_TODO: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\b(TODO|FIXME|HACK|XXX)\b").expect("valid regex"));
static TEXT_SCAN_PANIC_SITE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\b(unwrap|expect|panic!)\s*\(").expect("valid regex"));
static TEXT_SCAN_SUPPRESSED_ERROR: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"let\s+_\s*=.*\?|let\s+_\s*=.*\.unwrap").expect("valid regex")
});

impl TextScanPatterns {
    fn new() -> Self {
        Self {
            todo: &TEXT_SCAN_TODO,
            panic_site: &TEXT_SCAN_PANIC_SITE,
            suppressed_error: &TEXT_SCAN_SUPPRESSED_ERROR,
        }
    }
}

/// Scan a single file for findings using text-based patterns.
fn scan_file_text(
    path: &std::path::Path,
    root: &std::path::Path,
    patterns: &TextScanPatterns,
    max_per_kind: usize,
    kind_counts: &mut std::collections::HashMap<UnstickFindingKind, usize>,
) -> Vec<UnstickFinding> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let rel_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string();

    let mut findings = Vec::new();

    for (line_num, line) in content.lines().enumerate() {
        let line_num = (line_num + 1) as u32;

        // Check TODO/FIXME
        if patterns.todo.is_match(line) {
            let count = kind_counts
                .entry(UnstickFindingKind::TodoComment)
                .or_insert(0);
            if *count < max_per_kind {
                *count += 1;
                findings.push(UnstickFinding {
                    kind: UnstickFindingKind::TodoComment,
                    file: rel_path.clone(),
                    line: line_num,
                    snippet: line.trim().chars().take(200).collect(),
                    suggestion: "Address this TODO item or convert to a tracked issue".to_string(),
                });
            }
        }

        // Check panic sites
        if patterns.panic_site.is_match(line) {
            let count = kind_counts
                .entry(UnstickFindingKind::PanicSite)
                .or_insert(0);
            if *count < max_per_kind {
                *count += 1;
                findings.push(UnstickFinding {
                    kind: UnstickFindingKind::PanicSite,
                    file: rel_path.clone(),
                    line: line_num,
                    snippet: line.trim().chars().take(200).collect(),
                    suggestion: "Replace with proper error handling (? operator or match)"
                        .to_string(),
                });
            }
        }

        // Check suppressed errors
        if patterns.suppressed_error.is_match(line) {
            let count = kind_counts
                .entry(UnstickFindingKind::SuppressedError)
                .or_insert(0);
            if *count < max_per_kind {
                *count += 1;
                findings.push(UnstickFinding {
                    kind: UnstickFindingKind::SuppressedError,
                    file: rel_path.clone(),
                    line: line_num,
                    snippet: line.trim().chars().take(200).collect(),
                    suggestion: "Handle this error explicitly instead of suppressing it"
                        .to_string(),
                });
            }
        }
    }

    findings
}

/// Check whether `sg` (ast-grep) is available on the system.
#[must_use]
pub fn is_ast_grep_available() -> bool {
    std::process::Command::new("sg")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run the unstick scan using text-based patterns (always available).
///
/// This is the fallback scanner when ast-grep is not installed.
/// It scans files matching the configured extensions and returns
/// a bounded set of findings.
#[must_use]
pub fn run_unstick_scan_text(config: &UnstickConfig) -> UnstickReport {
    let patterns = TextScanPatterns::new();
    let mut kind_counts: std::collections::HashMap<UnstickFindingKind, usize> =
        std::collections::HashMap::new();
    let mut all_findings = Vec::new();
    let mut files_scanned = 0usize;
    let mut truncated = false;

    // Walk the directory tree using a stack-based approach (no walkdir dep)
    let mut dir_stack: Vec<(std::path::PathBuf, usize)> = vec![(config.root.clone(), 0)];

    while let Some((dir, depth)) = dir_stack.pop() {
        if depth > 10 || all_findings.len() >= config.max_total_findings {
            if all_findings.len() >= config.max_total_findings {
                truncated = true;
            }
            break;
        }

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            if all_findings.len() >= config.max_total_findings {
                truncated = true;
                break;
            }

            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_dir() {
                // Skip hidden dirs, target, node_modules, vendor
                if !name.starts_with('.')
                    && name != "target"
                    && name != "node_modules"
                    && name != "vendor"
                {
                    dir_stack.push((path, depth + 1));
                }
                continue;
            }

            if !path.is_file() {
                continue;
            }

            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();

            if !config.extensions.contains(&ext) {
                continue;
            }

            files_scanned += 1;

            let file_findings = scan_file_text(
                &path,
                &config.root,
                &patterns,
                config.max_findings_per_kind,
                &mut kind_counts,
            );

            for f in file_findings {
                if all_findings.len() >= config.max_total_findings {
                    truncated = true;
                    break;
                }
                all_findings.push(f);
            }
        }
    }

    // Build counts summary
    let counts: std::collections::BTreeMap<String, usize> = kind_counts
        .iter()
        .map(|(k, v)| (k.label().to_string(), *v))
        .collect();

    UnstickReport {
        findings: all_findings,
        files_scanned,
        truncated,
        scanner: "text".to_string(),
        counts,
    }
}


// ============================================================================
// WorkflowRunner - Event-driven workflow execution
// ============================================================================

/// Result of attempting to start a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStartResult {
    /// Workflow started successfully
    Started {
        /// Unique execution ID
        execution_id: String,
        /// Name of the workflow that was started
        workflow_name: String,
    },
    /// No workflow handles this detection
    NoMatchingWorkflow {
        /// The rule_id from the detection
        rule_id: String,
    },
    /// The pane is already locked by another workflow
    PaneLocked {
        /// The pane that is locked
        pane_id: u64,
        /// Workflow name holding the lock
        held_by_workflow: String,
        /// Execution ID holding the lock
        held_by_execution: String,
    },
    /// An error occurred
    Error {
        /// Error message
        error: String,
    },
}

impl WorkflowStartResult {
    /// Returns true if a workflow was started.
    #[must_use]
    pub fn is_started(&self) -> bool {
        matches!(self, Self::Started { .. })
    }

    /// Returns true if the pane was locked by another workflow.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::PaneLocked { .. })
    }

    /// Returns the execution ID if the workflow was started.
    #[must_use]
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Started { execution_id, .. } => Some(execution_id),
            _ => None,
        }
    }
}

/// Result of workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowExecutionResult {
    /// Workflow completed successfully
    Completed {
        /// Execution ID
        execution_id: String,
        /// Final result value
        result: serde_json::Value,
        /// Total elapsed time in milliseconds
        elapsed_ms: u64,
        /// Number of steps executed
        steps_executed: usize,
    },
    /// Workflow was aborted
    Aborted {
        /// Execution ID
        execution_id: String,
        /// Reason for abort
        reason: String,
        /// Step index where abort occurred
        step_index: usize,
        /// Elapsed time in milliseconds
        elapsed_ms: u64,
    },
    /// Workflow step was denied by policy
    PolicyDenied {
        /// Execution ID
        execution_id: String,
        /// Step index where denial occurred
        step_index: usize,
        /// Reason for denial
        reason: String,
    },
    /// An error occurred during execution
    Error {
        /// Execution ID (if available)
        execution_id: Option<String>,
        /// Error message
        error: String,
    },
}

impl WorkflowExecutionResult {
    /// Returns true if the workflow completed successfully.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// Returns true if the workflow was aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted { .. })
    }

    /// Returns the execution ID.
    #[must_use]
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Completed { execution_id, .. }
            | Self::Aborted { execution_id, .. }
            | Self::PolicyDenied { execution_id, .. } => Some(execution_id),
            Self::Error { execution_id, .. } => execution_id.as_deref(),
        }
    }
}

/// Configuration for the workflow runner.
#[derive(Debug, Clone)]
pub struct WorkflowRunnerConfig {
    /// Maximum concurrent workflow executions
    pub max_concurrent: usize,
    /// Default timeout for step execution (milliseconds)
    pub step_timeout_ms: u64,
    /// Retry delay multiplier for exponential backoff
    pub retry_backoff_multiplier: f64,
    /// Maximum retries per step
    pub max_retries_per_step: usize,
}

impl Default for WorkflowRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            step_timeout_ms: 30_000,
            retry_backoff_multiplier: 2.0,
            max_retries_per_step: 3,
        }
    }
}

/// Event-driven workflow runner that subscribes to detection events
/// and executes matching workflows.
///
/// # Architecture
///
/// ```text
/// EventBus (detections) -> WorkflowRunner -> find_matching_workflow
///                                         -> acquire_pane_lock
///                                         -> WorkflowEngine (persist)
///                                         -> execute_steps
///                                         -> release_pane_lock
/// ```
///
/// # Usage
///
/// ```ignore
/// let runner = WorkflowRunner::new(
///     engine,
///     lock_manager,
///     storage,
///     injector,
///     config,
/// );
///
/// // Register workflows
/// runner.register_workflow(Arc::new(MyWorkflow::new()));
///
/// // Run the event loop
/// runner.run(event_bus).await;
/// ```
pub struct WorkflowRunner {
    /// Registered workflows
    workflows: std::sync::RwLock<Vec<Arc<dyn Workflow>>>,
    /// Workflow engine for persistence
    engine: WorkflowEngine,
    /// Per-pane lock manager
    lock_manager: Arc<PaneWorkflowLockManager>,
    /// Storage handle for persistence
    storage: Arc<crate::storage::StorageHandle>,
    /// Policy-gated injector for terminal input
    injector: PolicyInjectorHandle,
    /// Optional replay capture adapter for decision provenance.
    replay_capture: Option<crate::replay_capture::SharedCaptureAdapter>,
    /// Configuration
    config: WorkflowRunnerConfig,
}

impl WorkflowRunner {
    /// Create a new workflow runner.
    pub fn new(
        engine: WorkflowEngine,
        lock_manager: Arc<PaneWorkflowLockManager>,
        storage: Arc<crate::storage::StorageHandle>,
        injector: PolicyInjectorHandle,
        config: WorkflowRunnerConfig,
    ) -> Self {
        Self {
            workflows: std::sync::RwLock::new(Vec::new()),
            engine,
            lock_manager,
            storage,
            injector,
            replay_capture: None,
            config,
        }
    }

    /// Attach a replay capture adapter for workflow step decision provenance.
    #[must_use]
    pub fn with_replay_capture_adapter(
        mut self,
        replay_capture: crate::replay_capture::SharedCaptureAdapter,
    ) -> Self {
        self.replay_capture = Some(replay_capture);
        self
    }

    /// Get the lock manager.
    pub fn lock_manager(&self) -> &Arc<PaneWorkflowLockManager> {
        &self.lock_manager
    }

    /// Register a workflow.
    pub fn register_workflow(&self, workflow: Arc<dyn Workflow>) {
        let mut workflows = self.workflows.write().unwrap();
        workflows.push(workflow);
    }

    /// Find a workflow that handles the given detection.
    pub fn find_matching_workflow(
        &self,
        detection: &crate::patterns::Detection,
    ) -> Option<Arc<dyn Workflow>> {
        let workflows = self.workflows.read().unwrap();
        workflows.iter().find(|w| w.handles(detection)).cloned()
    }

    /// Find a workflow by name.
    pub fn find_workflow_by_name(&self, name: &str) -> Option<Arc<dyn Workflow>> {
        let workflows = self.workflows.read().unwrap();
        workflows.iter().find(|w| w.name() == name).cloned()
    }

    /// Handle a detection event, potentially starting a workflow.
    ///
    /// Returns immediately with `WorkflowStartResult`. The actual workflow
    /// execution happens asynchronously if started.
    pub async fn handle_detection(
        &self,
        pane_id: u64,
        detection: &crate::patterns::Detection,
        event_id: Option<i64>,
    ) -> WorkflowStartResult {
        // Find matching workflow
        let Some(workflow) = self.find_matching_workflow(detection) else {
            return WorkflowStartResult::NoMatchingWorkflow {
                rule_id: detection.rule_id.clone(),
            };
        };

        let workflow_name = workflow.name().to_string();

        // Try to acquire pane lock
        let execution_id = generate_workflow_id(&workflow_name);
        let lock_result = self
            .lock_manager
            .try_acquire(pane_id, &workflow_name, &execution_id);

        match lock_result {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                ..
            } => {
                return WorkflowStartResult::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                };
            }
            LockAcquisitionResult::Acquired => {
                // Lock acquired, start execution
            }
        }

        // Start workflow execution via engine
        let agent_type_str = match detection.agent_type {
            crate::patterns::AgentType::Codex => "codex",
            crate::patterns::AgentType::ClaudeCode => "claude_code",
            crate::patterns::AgentType::Gemini => "gemini",
            crate::patterns::AgentType::Wezterm => "wezterm",
            crate::patterns::AgentType::Unknown => "unknown",
        };
        let severity_str = format!("{:?}", detection.severity).to_lowercase();

        // IMPORTANT: workflows expect ctx.trigger() to include at least:
        // - agent_type
        // - event_type
        // - extracted
        //
        // Keep the legacy nested "detection" object for backward compatibility.
        let context = serde_json::json!({
            "rule_id": detection.rule_id,
            "agent_type": agent_type_str,
            "event_type": detection.event_type,
            "severity": severity_str,
            "confidence": detection.confidence,
            "extracted": detection.extracted,
            "matched_text": detection.matched_text,
            "span": { "start": detection.span.0, "end": detection.span.1 },
            "detection": {
                "rule_id": detection.rule_id,
                "matched_text": detection.matched_text,
                "severity": format!("{:?}", detection.severity),
            }
        });

        match self
            .engine
            .start_with_id(
                &self.storage,
                execution_id.clone(),
                &workflow_name,
                pane_id,
                event_id,
                Some(context),
            )
            .await
        {
            Ok(_execution) => WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            },
            Err(e) => {
                // Release lock on error
                self.lock_manager.release(pane_id, &execution_id);
                WorkflowStartResult::Error {
                    error: e.to_string(),
                }
            }
        }
    }

    /// Run a workflow execution to completion.
    ///
    /// This method executes all steps of a workflow, handling retries,
    /// wait conditions, and policy gates.
    ///
    /// # Plan-first execution (wa-upg.2.3)
    ///
    /// If the workflow implements `to_action_plan`, the plan is generated and
    /// attached to the context before execution begins. This enables:
    /// - Deterministic step descriptions for audit trails
    /// - Idempotency keys for safe replay
    /// - Structured verification and failure handling
    pub async fn run_workflow(
        &self,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        start_step: usize,
    ) -> WorkflowExecutionResult {
        let start_time = Instant::now();
        let workflow_name = workflow.name().to_string();
        let step_count = workflow.step_count();
        let mut current_step = start_step;
        let mut retries = 0;
        let start_action_id = if start_step == 0 {
            record_workflow_start_action(
                &self.storage,
                &workflow_name,
                execution_id,
                pane_id,
                step_count,
                start_step,
            )
            .await
        } else {
            fetch_workflow_start_action_id(&self.storage, execution_id).await
        };

        // Create workflow context with injector for policy-gated actions.
        // Use prompt() capabilities (alt_screen: Some(false)) as the baseline —
        // workflows are triggered by detections on active panes where normal-screen
        // is the expected state. PaneCapabilities::default() leaves alt_screen as
        // None which causes the policy engine to require approval for SendText.
        let mut ctx = WorkflowContext::new(
            self.storage.clone(),
            pane_id,
            PaneCapabilities::prompt(),
            execution_id,
        )
        .with_injector(Arc::clone(&self.injector));

        // Attach persisted trigger context (if any) so workflows can interpret extracted fields.
        if let Ok(Some(record)) = self.storage.get_workflow(execution_id).await {
            if let Some(trigger) = record.context {
                ctx = ctx.with_trigger(trigger);
            }
        }

        if let Ok(Some(record)) = self.storage.get_pane(pane_id).await {
            ctx.set_pane_meta(PaneMetadata::from_record(&record));
        }

        if let Some(adapter) = self.replay_capture.as_ref() {
            let mut injector = self.injector.lock().await;
            injector.set_decision_capture(adapter.clone());
        }

        // Plan-first execution: generate ActionPlan if workflow supports it (wa-upg.2.3)
        if let Some(plan) = workflow.to_action_plan(&ctx, execution_id) {
            tracing::info!(
                execution_id,
                workflow_name = %workflow_name,
                plan_id = %plan.plan_id,
                step_count = plan.step_count(),
                "Generated action plan for workflow"
            );

            // Validate the plan before execution
            if let Err(validation_error) = plan.validate() {
                tracing::error!(
                    execution_id,
                    error = %validation_error,
                    "Action plan validation failed"
                );
                let reason = format!("Plan validation failed: {validation_error}");
                record_workflow_terminal_action(
                    &self.storage,
                    &workflow_name,
                    execution_id,
                    pane_id,
                    "workflow_error",
                    "error",
                    Some(&reason),
                    Some(start_step),
                    None,
                    start_action_id,
                )
                .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: reason,
                };
            }

            if let Err(e) = self.storage.upsert_action_plan(execution_id, &plan).await {
                tracing::warn!(
                    execution_id,
                    error = %e,
                    "Failed to persist action plan"
                );
            }

            ctx.set_action_plan(plan);
        }

        while current_step < step_count {
            let step_plan = ctx.get_step_plan(current_step).cloned();
            let mut idempotency_skip: Option<(i64, Option<String>)> = None;
            let mut idempotency_abort: Option<String> = None;

            if let Some(step_plan) = step_plan.as_ref() {
                if step_plan.idempotent {
                    match check_step_idempotency(
                        &self.storage,
                        execution_id,
                        &step_plan.step_id,
                        current_step,
                    )
                    .await
                    {
                        IdempotencyCheckResult::AlreadyCompleted {
                            completed_at,
                            previous_result,
                        } => {
                            tracing::info!(
                                execution_id,
                                step_index = current_step,
                                step_id = %step_plan.step_id,
                                "Skipping idempotent step already completed"
                            );
                            idempotency_skip = Some((completed_at, previous_result));
                        }
                        IdempotencyCheckResult::PartiallyExecuted { started_at } => {
                            let reason = format!(
                                "Idempotent step {} was started at {} but not completed",
                                step_plan.step_id, started_at
                            );
                            tracing::warn!(
                                execution_id,
                                step_index = current_step,
                                step_id = %step_plan.step_id,
                                "Idempotent step partially executed; aborting"
                            );
                            idempotency_abort = Some(reason);
                        }
                        IdempotencyCheckResult::NotExecuted => {}
                    }
                }
            }

            // Execute the step (or skip/abort based on idempotency)
            let step_result = if let Some(reason) = idempotency_abort {
                StepResult::Abort { reason }
            } else if idempotency_skip.is_some() {
                StepResult::Continue
            } else {
                workflow.execute_step(&mut ctx, current_step).await
            };

            // Log step result
            let result_type = match &step_result {
                StepResult::Continue => "continue",
                StepResult::Done { .. } => "done",
                StepResult::Retry { .. } => "retry",
                StepResult::Abort { .. } => "abort",
                StepResult::WaitFor { .. } => "wait_for",
                StepResult::SendText { .. } => "send_text",
                StepResult::JumpTo { .. } => "jump_to",
            };

            let steps = workflow.steps();
            let step_name = steps
                .get(current_step)
                .map_or("unknown", |s| s.name.as_str());

            let step_plan_ref = step_plan.as_ref();
            let step_id = step_plan_ref.map(|step| step.step_id.0.clone());
            let step_kind = step_plan_ref.map(|step| step.action.action_type_name().to_string());
            let verification_refs = build_verification_refs(&step_result, step_plan_ref);
            let step_error_code = step_error_code_from_result(&step_result);
            let log_step_result = redacted_step_result_for_logging(&step_result);

            if let Some(adapter) = self.replay_capture.as_ref() {
                let step_definition_text = steps
                    .get(current_step)
                    .and_then(|step| serde_json::to_string(step).ok())
                    .unwrap_or_else(|| step_name.to_string());
                let step_input = serde_json::json!({
                    "workflow_name": workflow_name.as_str(),
                    "execution_id": execution_id,
                    "pane_id": pane_id,
                    "step_index": current_step,
                    "step_name": step_name,
                    "trigger": ctx.trigger().cloned().unwrap_or(serde_json::Value::Null),
                });
                let step_input_text = serde_json::to_string(&step_input)
                    .unwrap_or_else(|_| format!("workflow={workflow_name};step={current_step}"));
                let step_output = serde_json::to_value(&log_step_result).unwrap_or_else(|_| {
                    serde_json::json!({
                        "result_type": result_type,
                    })
                });
                let decision_event = crate::replay_capture::DecisionEvent::new(
                    crate::replay_capture::DecisionType::WorkflowStep,
                    pane_id,
                    format!("workflow.{workflow_name}.step.{current_step}"),
                    &step_definition_text,
                    &step_input_text,
                    step_output,
                    Some(format!("workflow_execution:{execution_id}")),
                    None,
                    crate::recording::epoch_ms_now(),
                );
                adapter.capture_decision(
                    crate::recording::RecorderEventSource::WorkflowEngine,
                    Some(execution_id.to_string()),
                    decision_event,
                );
            }

            // Build result data, enriching with plan information if available (wa-upg.2.3)
            let result_data = {
                let mut data = serde_json::json!({
                    "step_result": &log_step_result,
                });

                // Include idempotency key from plan if executing in plan-first mode
                if let Some(idempotency_key) = ctx.get_step_idempotency_key(current_step) {
                    data["idempotency_key"] = serde_json::json!(idempotency_key.0);
                }

                // Include step action type from plan if available
                if let Some(step_plan) = step_plan_ref {
                    data["action_type"] = serde_json::json!(step_plan.action.action_type_name());
                    data["step_description"] = serde_json::json!(step_plan.description);
                }

                if let Some((completed_at, previous_result)) = &idempotency_skip {
                    data["idempotency_skip"] = serde_json::json!(true);
                    data["previous_completed_at"] = serde_json::json!(completed_at);
                    if let Some(previous_result) = previous_result {
                        data["previous_result"] = serde_json::json!(previous_result);
                    }
                }

                serde_json::to_string(&data)
                    .inspect_err(
                        |e| tracing::warn!(error = %e, "workflow step data serialization failed"),
                    )
                    .ok()
            };
            let step_started_at = now_ms();
            let step_completed_at = now_ms();

            // Persist step log for non-SendText steps
            // SendText steps are logged after injection to capture the audit_action_id (wa-nu4.1.1.11)
            if !matches!(&step_result, StepResult::SendText { .. }) {
                let step_audit_action_id = record_workflow_step_action(
                    &self.storage,
                    &workflow_name,
                    execution_id,
                    pane_id,
                    current_step,
                    step_name,
                    step_id.clone(),
                    step_kind.clone(),
                    result_type,
                    start_action_id,
                )
                .await;

                if let Err(e) = self
                    .storage
                    .insert_step_log(
                        execution_id,
                        step_audit_action_id,
                        current_step,
                        step_name,
                        step_id.clone(),
                        step_kind.clone(),
                        result_type,
                        result_data.clone(),
                        None,
                        verification_refs.clone(),
                        step_error_code,
                        step_started_at,
                        step_completed_at,
                    )
                    .await
                {
                    tracing::warn!(
                        workflow = %workflow_name,
                        execution_id,
                        step = current_step,
                        error = %e,
                        "Failed to log step"
                    );
                }
            }

            // Handle step result
            match step_result {
                StepResult::Continue => {
                    current_step += 1;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_aborted",
                                "aborted",
                                Some(&reason),
                                Some(current_step),
                                None,
                                start_action_id,
                            )
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::JumpTo { step } => {
                    current_step = step;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after jump"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_aborted",
                                "aborted",
                                Some(&reason),
                                Some(current_step),
                                None,
                                start_action_id,
                            )
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::Done { result } => {
                    // Workflow completed
                    let elapsed_ms = elapsed_ms(start_time);

                    // Update execution to completed
                    if let Err(e) = self
                        .complete_execution(execution_id, Some(result.clone()))
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to complete execution"
                        );
                    }

                    // Mark trigger event as handled
                    if let Err(e) = self
                        .mark_trigger_event_handled(execution_id, "completed")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event as handled"
                        );
                    }

                    // Release lock
                    self.lock_manager.release(pane_id, execution_id);

                    record_workflow_terminal_action(
                        &self.storage,
                        &workflow_name,
                        execution_id,
                        pane_id,
                        "workflow_completed",
                        "completed",
                        None,
                        Some(current_step),
                        Some(current_step + 1),
                        start_action_id,
                    )
                    .await;

                    return WorkflowExecutionResult::Completed {
                        execution_id: execution_id.to_string(),
                        result,
                        elapsed_ms,
                        steps_executed: current_step + 1,
                    };
                }
                StepResult::Retry { delay_ms } => {
                    retries += 1;
                    if retries > self.config.max_retries_per_step {
                        let elapsed_ms = elapsed_ms(start_time);
                        let reason = format!(
                            "Max retries ({}) exceeded at step {}",
                            self.config.max_retries_per_step, current_step
                        );

                        // Update execution to failed
                        if let Err(e) = self.fail_execution(execution_id, &reason).await {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to fail execution"
                            );
                        }

                        // Mark trigger event as handled (with failed status)
                        if let Err(e) = self
                            .mark_trigger_event_handled(execution_id, "failed")
                            .await
                        {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to mark trigger event as handled"
                            );
                        }

                        // Cleanup and release lock
                        workflow.cleanup(&mut ctx).await;
                        self.lock_manager.release(pane_id, execution_id);

                        record_workflow_terminal_action(
                            &self.storage,
                            &workflow_name,
                            execution_id,
                            pane_id,
                            "workflow_aborted",
                            "aborted",
                            Some(&reason),
                            Some(current_step),
                            Some(current_step + 1),
                            start_action_id,
                        )
                        .await;

                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason,
                            step_index: current_step,
                            elapsed_ms,
                        };
                    }

                    // Wait before retry
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                StepResult::Abort { reason } => {
                    let elapsed_ms = elapsed_ms(start_time);

                    // Update execution to failed
                    if let Err(e) = self.fail_execution(execution_id, &reason).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to fail execution"
                        );
                    }

                    // Mark trigger event as handled (with aborted status)
                    if let Err(e) = self
                        .mark_trigger_event_handled(execution_id, "aborted")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event as handled"
                        );
                    }

                    // Cleanup and release lock
                    workflow.cleanup(&mut ctx).await;
                    self.lock_manager.release(pane_id, execution_id);

                    record_workflow_terminal_action(
                        &self.storage,
                        &workflow_name,
                        execution_id,
                        pane_id,
                        "workflow_aborted",
                        "aborted",
                        Some(&reason),
                        Some(current_step),
                        Some(current_step + 1),
                        start_action_id,
                    )
                    .await;

                    return WorkflowExecutionResult::Aborted {
                        execution_id: execution_id.to_string(),
                        reason,
                        step_index: current_step,
                        elapsed_ms,
                    };
                }
                StepResult::WaitFor {
                    condition,
                    timeout_ms,
                } => {
                    // Update execution to waiting
                    if let Err(e) = self
                        .set_execution_waiting(execution_id, current_step, &condition)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to set waiting state"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_aborted",
                                "aborted",
                                Some(&reason),
                                Some(current_step),
                                None,
                                start_action_id,
                            )
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }

                    // Execute wait condition
                    let timeout = timeout_ms.map_or_else(
                        || Duration::from_millis(self.config.step_timeout_ms),
                        Duration::from_millis,
                    );

                    // Simple wait implementation - in practice would use WaitConditionExecutor
                    match &condition {
                        WaitCondition::PaneIdle {
                            idle_threshold_ms, ..
                        } => {
                            sleep(Duration::from_millis(*idle_threshold_ms)).await;
                        }
                        WaitCondition::Pattern { .. } => {
                            // Would use WaitConditionExecutor here
                            sleep(timeout).await;
                        }
                        WaitCondition::StableTail { stable_for_ms, .. } => {
                            // Would use WaitConditionExecutor here
                            sleep(Duration::from_millis(*stable_for_ms)).await;
                        }
                        WaitCondition::TextMatch { .. } => {
                            // Would use WaitConditionExecutor here
                            sleep(timeout).await;
                        }
                        WaitCondition::Sleep { duration_ms } => {
                            sleep(Duration::from_millis(*duration_ms)).await;
                        }
                        WaitCondition::External { .. } => {
                            // Would wait for external signal
                            sleep(timeout).await;
                        }
                    }

                    // Continue to next step after wait
                    current_step += 1;
                    retries = 0;

                    // Update execution back to running
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after wait"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_aborted",
                                "aborted",
                                Some(&reason),
                                Some(current_step),
                                None,
                                start_action_id,
                            )
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::SendText {
                    text,
                    wait_for,
                    wait_timeout_ms,
                } => {
                    // Attempt to send text via policy-gated injector
                    tracing::info!(
                        pane_id,
                        execution_id,
                        text_len = text.len(),
                        "Workflow requesting text injection"
                    );

                    let send_result = {
                        let mut guard = self.injector.lock().await;
                        guard
                            .send_text(
                                pane_id,
                                &text,
                                crate::policy::ActorKind::Workflow,
                                ctx.capabilities(),
                                Some(execution_id),
                            )
                            .await
                    };

                    // Log the SendText step with audit_action_id (wa-nu4.1.1.11)
                    let audit_action_id = send_result.audit_action_id();
                    let policy_summary = policy_summary_from_injection(&send_result);
                    let policy_error_code = policy_error_code_from_injection(&send_result);
                    if let Err(e) = self
                        .storage
                        .insert_step_log(
                            execution_id,
                            audit_action_id,
                            current_step,
                            step_name,
                            step_id.clone(),
                            step_kind.clone(),
                            "send_text",
                            result_data.clone(),
                            policy_summary,
                            verification_refs.clone(),
                            policy_error_code,
                            step_started_at,
                            now_ms(), // Use current time as completion
                        )
                        .await
                    {
                        tracing::warn!(
                            workflow = %workflow_name,
                            execution_id,
                            step = current_step,
                            ?audit_action_id,
                            error = %e,
                            "Failed to log SendText step"
                        );
                    }

                    match send_result {
                        crate::policy::InjectionResult::Allowed { .. } => {
                            tracing::info!(pane_id, execution_id, "Text injection succeeded");

                            // If there's a wait condition, handle it
                            if let Some(condition) = wait_for {
                                let timeout = wait_timeout_ms.map_or_else(
                                    || Duration::from_millis(self.config.step_timeout_ms),
                                    Duration::from_millis,
                                );

                                // Simple wait implementation
                                match &condition {
                                    WaitCondition::PaneIdle {
                                        idle_threshold_ms, ..
                                    } => {
                                        sleep(Duration::from_millis(*idle_threshold_ms)).await;
                                    }
                                    WaitCondition::Pattern { .. } => {
                                        sleep(timeout).await;
                                    }
                                    WaitCondition::StableTail { stable_for_ms, .. } => {
                                        sleep(Duration::from_millis(*stable_for_ms)).await;
                                    }
                                    WaitCondition::TextMatch { .. } => {
                                        sleep(timeout).await;
                                    }
                                    WaitCondition::Sleep { duration_ms } => {
                                        sleep(Duration::from_millis(*duration_ms)).await;
                                    }
                                    WaitCondition::External { .. } => {
                                        sleep(timeout).await;
                                    }
                                }
                            }

                            // Continue to next step
                            current_step += 1;
                            retries = 0;

                            if let Err(e) =
                                self.update_execution_step(execution_id, current_step).await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to update execution step after send"
                                );
                                if let crate::Error::Workflow(
                                    crate::error::WorkflowError::Aborted(reason),
                                ) = e
                                {
                                    self.lock_manager.release(pane_id, execution_id);
                                    record_workflow_terminal_action(
                                        &self.storage,
                                        &workflow_name,
                                        execution_id,
                                        pane_id,
                                        "workflow_aborted",
                                        "aborted",
                                        Some(&reason),
                                        Some(current_step),
                                        None,
                                        start_action_id,
                                    )
                                    .await;
                                    return WorkflowExecutionResult::Aborted {
                                        execution_id: execution_id.to_string(),
                                        reason,
                                        step_index: current_step,
                                        elapsed_ms: elapsed_ms(start_time),
                                    };
                                }
                            }
                        }
                        crate::policy::InjectionResult::Denied { decision, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let reason = match &decision {
                                crate::policy::PolicyDecision::Deny { reason, .. } => {
                                    reason.clone()
                                }
                                _ => "Unknown denial reason".to_string(),
                            };
                            let abort_reason = format!("Policy denied text injection: {reason}");

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                reason = %reason,
                                "Text injection denied by policy"
                            );

                            // Update execution to failed
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with denied status)
                            if let Err(e) = self
                                .mark_trigger_event_handled(execution_id, "denied")
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_policy_denied",
                                "policy_denied",
                                Some(&abort_reason),
                                Some(current_step),
                                Some(current_step + 1),
                                start_action_id,
                            )
                            .await;

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                        crate::policy::InjectionResult::RequiresApproval { decision, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let code = match &decision {
                                crate::policy::PolicyDecision::RequireApproval {
                                    approval, ..
                                } => approval.as_ref().map_or_else(
                                    || "unknown".to_string(),
                                    |a| a.allow_once_code.clone(),
                                ),
                                _ => "unknown".to_string(),
                            };
                            let abort_reason =
                                format!("Text injection requires approval (code: {code})");

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                code = %code,
                                "Text injection requires approval"
                            );

                            // Update execution to failed (approval not auto-granted for workflows)
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_requires_approval",
                                "requires_approval",
                                Some(&abort_reason),
                                Some(current_step),
                                Some(current_step + 1),
                                start_action_id,
                            )
                            .await;

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                        crate::policy::InjectionResult::Error { error, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let abort_reason =
                                format!("Text injection failed after policy allowed: {error}");

                            tracing::error!(
                                pane_id,
                                execution_id,
                                error = %error,
                                "Text injection failed after policy allowed"
                            );

                            // Update execution to failed
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with error status)
                            if let Err(e) =
                                self.mark_trigger_event_handled(execution_id, "error").await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action(
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_error",
                                "error",
                                Some(&abort_reason),
                                Some(current_step),
                                Some(current_step + 1),
                                start_action_id,
                            )
                            .await;

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                    }
                }
            }
        }

        // All steps completed without explicit Done
        let elapsed_ms = elapsed_ms(start_time);
        let result = serde_json::json!({ "status": "completed" });

        if let Err(e) = self
            .complete_execution(execution_id, Some(result.clone()))
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to complete execution"
            );
        }

        // Mark trigger event as handled
        if let Err(e) = self
            .mark_trigger_event_handled(execution_id, "completed")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled"
            );
        }

        self.lock_manager.release(pane_id, execution_id);

        record_workflow_terminal_action(
            &self.storage,
            &workflow_name,
            execution_id,
            pane_id,
            "workflow_completed",
            "completed",
            None,
            Some(step_count.saturating_sub(1)),
            Some(step_count),
            start_action_id,
        )
        .await;

        WorkflowExecutionResult::Completed {
            execution_id: execution_id.to_string(),
            result,
            elapsed_ms,
            steps_executed: step_count,
        }
    }

    /// Run the event loop, subscribing to detection events.
    ///
    /// This spawns workflow executions for matching detections. The loop
    /// runs until the event bus channel is closed.
    ///
    /// On startup, resumes any incomplete workflows that were interrupted
    /// (e.g., by a previous watcher crash or restart).
    pub async fn run(&self, event_bus: &crate::events::EventBus) {
        // Resume any incomplete workflows from a previous run
        let resumed = self.resume_incomplete().await;
        if !resumed.is_empty() {
            tracing::info!(
                count = resumed.len(),
                "Resumed incomplete workflows from previous run"
            );
            for result in &resumed {
                match result {
                    WorkflowExecutionResult::Completed { execution_id, .. } => {
                        tracing::info!(execution_id, "Resumed workflow completed");
                    }
                    WorkflowExecutionResult::Error {
                        execution_id,
                        error,
                    } => {
                        tracing::warn!(?execution_id, error, "Resumed workflow errored");
                    }
                    _ => {}
                }
            }
        }

        let mut subscriber = event_bus.subscribe_detections();

        loop {
            match subscriber.recv().await {
                Ok(event) => {
                    if let crate::events::Event::PatternDetected {
                        pane_id,
                        pane_uuid: _,
                        detection,
                        event_id,
                    } = event
                    {
                        // Handle detection with event_id for proper event lifecycle
                        let result = self.handle_detection(pane_id, &detection, event_id).await;

                        match result {
                            WorkflowStartResult::Started {
                                execution_id,
                                workflow_name,
                            } => {
                                // Find workflow and spawn execution
                                if let Some(workflow) = self.find_workflow_by_name(&workflow_name) {
                                    let execution_id_clone = execution_id.clone();
                                    let workflow_clone = Arc::clone(&workflow);
                                    let storage = Arc::clone(&self.storage);
                                    let lock_manager = Arc::clone(&self.lock_manager);
                                    let config = self.config.clone();
                                    let engine = WorkflowEngine::new(config.max_concurrent);

                                    // Create a mini-runner for the spawned task
                                    let runner = Self {
                                        workflows: std::sync::RwLock::new(vec![
                                            workflow_clone.clone(),
                                        ]),
                                        engine,
                                        lock_manager,
                                        storage,
                                        injector: Arc::clone(&self.injector),
                                        config,
                                        replay_capture: self.replay_capture.clone(),
                                    };

                                    crate::runtime_compat::task::spawn(async move {
                                        let result = runner
                                            .run_workflow(
                                                pane_id,
                                                workflow_clone,
                                                &execution_id_clone,
                                                0,
                                            )
                                            .await;

                                        match &result {
                                            WorkflowExecutionResult::Completed {
                                                execution_id,
                                                steps_executed,
                                                elapsed_ms,
                                                ..
                                            } => {
                                                tracing::info!(
                                                    execution_id,
                                                    steps = steps_executed,
                                                    elapsed_ms,
                                                    "Workflow completed"
                                                );
                                            }
                                            WorkflowExecutionResult::Aborted {
                                                execution_id,
                                                reason,
                                                step_index,
                                                ..
                                            } => {
                                                tracing::warn!(
                                                    execution_id,
                                                    step = step_index,
                                                    reason,
                                                    "Workflow aborted"
                                                );
                                            }
                                            WorkflowExecutionResult::PolicyDenied {
                                                execution_id,
                                                step_index,
                                                reason,
                                            } => {
                                                tracing::warn!(
                                                    execution_id,
                                                    step = step_index,
                                                    reason,
                                                    "Workflow denied by policy"
                                                );
                                            }
                                            WorkflowExecutionResult::Error {
                                                execution_id,
                                                error,
                                            } => {
                                                tracing::error!(
                                                    execution_id = execution_id.as_deref(),
                                                    error,
                                                    "Workflow error"
                                                );
                                            }
                                        }
                                    });
                                }
                            }
                            WorkflowStartResult::NoMatchingWorkflow { rule_id } => {
                                tracing::debug!(rule_id, "No workflow handles detection");
                            }
                            WorkflowStartResult::PaneLocked {
                                pane_id,
                                held_by_workflow,
                                ..
                            } => {
                                tracing::debug!(
                                    pane_id,
                                    held_by = %held_by_workflow,
                                    "Pane locked, skipping detection"
                                );
                            }
                            WorkflowStartResult::Error { error } => {
                                tracing::error!(error, "Failed to start workflow");
                            }
                        }
                    }
                }
                Err(crate::events::RecvError::Lagged { missed_count }) => {
                    tracing::warn!(
                        skipped = missed_count,
                        "Workflow runner lagged, skipped events"
                    );
                }
                Err(crate::events::RecvError::Closed) => {
                    tracing::info!("Event bus closed, workflow runner stopping");
                    break;
                }
            }
        }
    }

    /// Resume incomplete workflows after restart.
    ///
    /// Queries storage for workflows with status 'running' or 'waiting'
    /// and attempts to resume them.
    pub async fn resume_incomplete(&self) -> Vec<WorkflowExecutionResult> {
        let incomplete = match self.storage.find_incomplete_workflows().await {
            Ok(workflows) => workflows,
            Err(e) => {
                tracing::error!(error = %e, "Failed to query incomplete workflows");
                return vec![];
            }
        };

        let mut results = Vec::new();

        for record in incomplete {
            // Find the workflow definition
            let Some(workflow) = self.find_workflow_by_name(&record.workflow_name) else {
                tracing::warn!(
                    workflow_name = %record.workflow_name,
                    execution_id = %record.id,
                    "Cannot resume: workflow not registered"
                );
                continue;
            };

            // Compute next step from logs
            let step_logs = match self.storage.get_step_logs(&record.id).await {
                Ok(logs) => logs,
                Err(e) => {
                    tracing::warn!(
                        execution_id = %record.id,
                        error = %e,
                        "Failed to get step logs for resume"
                    );
                    continue;
                }
            };

            let next_step = compute_next_step(&step_logs);

            // Try to re-acquire lock
            let lock_result =
                self.lock_manager
                    .try_acquire(record.pane_id, &record.workflow_name, &record.id);

            match lock_result {
                LockAcquisitionResult::AlreadyLocked { .. } => {
                    tracing::warn!(
                        execution_id = %record.id,
                        pane_id = record.pane_id,
                        "Cannot resume: pane locked"
                    );
                    continue;
                }
                LockAcquisitionResult::Acquired => {}
            }

            tracing::info!(
                execution_id = %record.id,
                workflow = %record.workflow_name,
                pane_id = record.pane_id,
                resume_step = next_step,
                "Resuming workflow"
            );

            let result = self
                .run_workflow(record.pane_id, workflow, &record.id, next_step)
                .await;

            results.push(result);
        }

        results
    }

    // --- Private helper methods ---

    async fn update_execution_step(&self, execution_id: &str, step: usize) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if workflow was externally aborted/completed
        if record.status == "aborted" || record.status == "failed" || record.status == "completed" {
            return Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "Workflow externally modified to status: {}",
                    record.status
                )),
            ));
        }

        record.current_step = step;
        record.status = "running".to_string();
        record.wait_condition = None;
        record.updated_at = now_ms();

        self.storage.upsert_workflow(record).await
    }

    async fn set_execution_waiting(
        &self,
        execution_id: &str,
        step: usize,
        condition: &WaitCondition,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if workflow was externally aborted/completed
        if record.status == "aborted" || record.status == "failed" || record.status == "completed" {
            return Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "Workflow externally modified to status: {}",
                    record.status
                )),
            ));
        }

        record.current_step = step;
        record.status = "waiting".to_string();
        record.wait_condition = Some(serde_json::to_value(condition)?);
        record.updated_at = now_ms();

        self.storage.upsert_workflow(record).await
    }

    async fn complete_execution(
        &self,
        execution_id: &str,
        result: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        record.status = "completed".to_string();
        record.result = result;
        let now = now_ms();
        record.updated_at = now;
        record.completed_at = Some(now);

        let duration_ms = now.saturating_sub(record.started_at);
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let metric = crate::storage::UsageMetricRecord {
            id: 0,
            timestamp: now,
            metric_type: crate::storage::MetricType::WorkflowCost,
            pane_id: Some(pane_id),
            agent_type: None,
            account_id: None,
            workflow_id: Some(record.id.clone()),
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: Some(
                serde_json::json!({
                    "source": "workflow.runner",
                    "workflow_name": workflow_name,
                    "status": "completed",
                    "duration_ms": duration_ms,
                })
                .to_string(),
            ),
            created_at: now,
        };

        self.storage.upsert_workflow(record).await?;
        if let Err(err) = self.storage.record_usage_metric(metric).await {
            tracing::warn!(pane_id, error = %err, "Failed to record workflow completion metric");
        }
        Ok(())
    }

    async fn fail_execution(&self, execution_id: &str, error: &str) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        record.status = "failed".to_string();
        record.error = Some(error.to_string());
        let now = now_ms();
        record.updated_at = now;
        record.completed_at = Some(now);

        let duration_ms = now.saturating_sub(record.started_at);
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let metric = crate::storage::UsageMetricRecord {
            id: 0,
            timestamp: now,
            metric_type: crate::storage::MetricType::WorkflowCost,
            pane_id: Some(pane_id),
            agent_type: None,
            account_id: None,
            workflow_id: Some(record.id.clone()),
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: Some(
                serde_json::json!({
                    "source": "workflow.runner",
                    "workflow_name": workflow_name,
                    "status": "failed",
                    "duration_ms": duration_ms,
                })
                .to_string(),
            ),
            created_at: now,
        };

        self.storage.upsert_workflow(record).await?;
        if let Err(err) = self.storage.record_usage_metric(metric).await {
            tracing::warn!(pane_id, error = %err, "Failed to record workflow failure metric");
        }
        Ok(())
    }

    /// Mark the triggering event as handled after workflow completion.
    ///
    /// This ensures proper event lifecycle management - events that triggered
    /// workflows are marked with the outcome so they won't be re-processed.
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID
    /// * `status` - The handling status ("completed", "failed", "aborted", "denied")
    async fn mark_trigger_event_handled(
        &self,
        execution_id: &str,
        status: &str,
    ) -> crate::Result<()> {
        // Get the workflow record to find trigger_event_id
        let record = self.storage.get_workflow(execution_id).await?;

        if let Some(record) = record {
            if let Some(event_id) = record.trigger_event_id {
                self.storage
                    .mark_event_handled(event_id, Some(execution_id.to_string()), status)
                    .await?;

                tracing::debug!(
                    execution_id,
                    event_id,
                    status,
                    "Marked trigger event as handled"
                );
            }
        }

        Ok(())
    }

    /// Abort a running workflow execution.
    ///
    /// This is the external API for aborting workflows (e.g., from robot mode).
    /// It differs from internal abort handling in that:
    /// 1. It validates the execution state before aborting
    /// 2. It releases the pane lock if held
    /// 3. It returns detailed abort information
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID to abort
    /// * `reason` - Optional reason for the abort (recorded in audit)
    /// * `force` - If true, skip cleanup steps
    ///
    /// # Returns
    /// * `Ok(AbortResult)` - Details about the aborted workflow
    /// * `Err` - If the workflow doesn't exist or is in invalid state
    pub async fn abort_execution(
        &self,
        execution_id: &str,
        reason: Option<&str>,
        _force: bool, // Reserved for future cleanup skipping
    ) -> crate::Result<AbortResult> {
        // Load the workflow record
        let record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if already in terminal state
        match record.status.as_str() {
            "completed" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_completed".to_string()),
                });
            }
            "aborted" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_aborted".to_string()),
                });
            }
            "failed" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_failed".to_string()),
                });
            }
            _ => {} // running, waiting - proceed with abort
        }

        let previous_status = record.status.clone();
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let aborted_at_step = record.current_step;
        let now = now_ms();

        // Update the record to aborted status
        let mut updated_record = record;
        updated_record.status = "aborted".to_string();
        updated_record.error = reason.map(|r| format!("Aborted: {r}"));
        updated_record.updated_at = now;
        updated_record.completed_at = Some(now);

        self.storage.upsert_workflow(updated_record).await?;

        // Release the pane lock if held
        self.lock_manager.release(pane_id, execution_id);

        // Mark trigger event as handled with aborted status
        if let Err(e) = self
            .mark_trigger_event_handled(execution_id, "aborted")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled during abort"
            );
        }

        tracing::info!(
            execution_id,
            workflow_name,
            pane_id,
            reason = reason.unwrap_or("no reason provided"),
            "Workflow aborted"
        );

        Ok(AbortResult {
            aborted: true,
            execution_id: execution_id.to_string(),
            workflow_name,
            pane_id,
            previous_status,
            aborted_at_step,
            reason: reason.map(std::string::ToString::to_string),
            aborted_at: Some(now as u64),
            error_reason: None,
        })
    }
}

/// Result of an abort operation
#[derive(Debug, Clone, serde::Serialize)]
pub struct AbortResult {
    /// Whether the abort was successful
    pub aborted: bool,
    /// Execution ID
    pub execution_id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane ID
    pub pane_id: u64,
    /// Status before abort
    pub previous_status: String,
    /// Step index where abort occurred
    pub aborted_at_step: usize,
    /// Reason for abort (if provided)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Timestamp of abort (epoch ms)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aborted_at: Option<u64>,
    /// Error reason if abort failed (e.g., "already_completed")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
}

// ============================================================================
// Built-in Workflows
// ============================================================================

/// Agent-specific prompts for context refresh after compaction.
///
/// These prompts are carefully crafted to be:
/// - Minimal in length (to avoid adding too much to already-compacted context)
/// - Clear in intent (agent should re-read key project files)
/// - Agent-specific (matching each agent's communication style)
pub mod compaction_prompts {
    /// Prompt for Claude Code agents.
    pub const CLAUDE_CODE: &str = crate::config::DEFAULT_COMPACTION_PROMPT_CLAUDE_CODE;

    /// Prompt for Codex CLI agents.
    pub const CODEX: &str = crate::config::DEFAULT_COMPACTION_PROMPT_CODEX;

    /// Prompt for Gemini CLI agents.
    pub const GEMINI: &str = crate::config::DEFAULT_COMPACTION_PROMPT_GEMINI;

    /// Default prompt for unknown agents.
    pub const UNKNOWN: &str = crate::config::DEFAULT_COMPACTION_PROMPT_UNKNOWN;
}

#[derive(Debug, Clone)]
struct PromptRenderContext {
    pane_id: u64,
    agent_type: crate::patterns::AgentType,
    pane_domain: Option<String>,
    pane_title: Option<String>,
    pane_cwd: Option<String>,
}

impl PromptRenderContext {
    fn from_context(ctx: &WorkflowContext) -> Self {
        let agent_type = HandleCompaction::agent_type_from_trigger(ctx);
        let meta = ctx.pane_meta();
        Self {
            pane_id: ctx.pane_id(),
            agent_type,
            pane_domain: meta.domain.clone(),
            pane_title: meta.title.clone(),
            pane_cwd: meta.cwd.clone(),
        }
    }
}

fn render_compaction_prompt(
    template: &str,
    ctx: &PromptRenderContext,
    config: &crate::config::CompactionPromptConfig,
) -> String {
    let redactor = Redactor::new();
    let max_prompt_len = config.max_prompt_len as usize;
    let max_snippet_len = config.max_snippet_len as usize;

    let mut rendered = template.to_string();
    let replacements = [
        ("agent_type", ctx.agent_type.to_string()),
        ("pane_id", ctx.pane_id.to_string()),
        ("pane_domain", ctx.pane_domain.clone().unwrap_or_default()),
        ("pane_title", ctx.pane_title.clone().unwrap_or_default()),
        ("pane_cwd", ctx.pane_cwd.clone().unwrap_or_default()),
    ];

    for (key, value) in replacements {
        let token = format!("{{{{{key}}}}}");
        if rendered.contains(&token) {
            let redacted = redactor.redact(&value);
            let clipped = truncate_to_len(&redacted, max_snippet_len);
            rendered = rendered.replace(&token, &clipped);
        }
    }

    let redacted = redactor.redact(&rendered);
    truncate_to_len(&redacted, max_prompt_len)
}

fn truncate_to_len(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }

    value.chars().take(max_len).collect()
}

#[derive(Debug)]
struct StabilizationOutcome {
    waited_ms: u64,
    polls: usize,
    last_activity_ms: Option<i64>,
}

/// Handle compaction workflow: re-inject critical context after conversation compaction.
///
/// This workflow is triggered when an AI agent compacts or summarizes its context window.
/// After compaction, the agent may have lost important project context, so we prompt
/// the agent to re-read key files like AGENTS.md.
///
/// # Steps
///
/// 1. **Acquire lock**: Get per-pane workflow lock to prevent concurrent workflows.
/// 2. **Validate state**: Check that pane is not in alt-screen mode and has no recent gap.
/// 3. **Confirm anchor**: Re-read pane tail to verify compaction anchor is still present.
/// 4. **Stabilize**: Wait for pane to be idle (2s default) before sending.
/// 5. **Send prompt**: Inject agent-specific context refresh prompt.
/// 6. **Verify**: Wait for response pattern or timeout.
///
/// # Safety
///
/// - All sends are policy-gated (may be denied by PolicyEngine).
/// - Workflow is idempotent: dedupe/cooldown prevents spam on repeated detections.
/// - Guards abort workflow if pane state is unsuitable for injection.
///
/// # Example Detection
///
/// ```text
/// rule_id: "claude_code.compaction"
/// event_type: "session.compaction"
/// matched_text: "Auto-compact: compacted 150,000 tokens to 25,000 tokens"
/// ```
pub struct HandleCompaction {
    /// Default stabilization wait time in milliseconds.
    pub stabilization_ms: u64,
    /// Timeout for the idle wait condition.
    pub idle_timeout_ms: u64,
    /// Prompt templates and bounds for compaction prompts.
    pub prompt_config: crate::config::CompactionPromptConfig,
}

impl Default for HandleCompaction {
    fn default() -> Self {
        Self {
            stabilization_ms: 2000,
            idle_timeout_ms: 10_000,
            prompt_config: crate::config::CompactionPromptConfig::default(),
        }
    }
}

impl HandleCompaction {
    /// Create a new HandleCompaction workflow with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom stabilization time.
    #[must_use]
    pub fn with_stabilization_ms(mut self, ms: u64) -> Self {
        self.stabilization_ms = ms;
        self
    }

    /// Create with custom idle timeout.
    #[must_use]
    pub fn with_idle_timeout_ms(mut self, ms: u64) -> Self {
        self.idle_timeout_ms = ms;
        self
    }

    /// Create with custom compaction prompt configuration.
    #[must_use]
    pub fn with_prompt_config(
        mut self,
        prompt_config: crate::config::CompactionPromptConfig,
    ) -> Self {
        self.prompt_config = prompt_config;
        self
    }

    /// Get the agent-specific prompt based on agent type from trigger detection.
    fn resolve_prompt(&self, ctx: &WorkflowContext) -> String {
        let render_ctx = PromptRenderContext::from_context(ctx);
        let template = self.select_prompt_template(&render_ctx);
        render_compaction_prompt(template, &render_ctx, &self.prompt_config)
    }

    fn select_prompt_template<'a>(&'a self, ctx: &PromptRenderContext) -> &'a str {
        if let Some(prompt) = self.prompt_config.by_pane.get(&ctx.pane_id) {
            return prompt;
        }

        let domain = ctx.pane_domain.as_deref().unwrap_or_default();
        let title = ctx.pane_title.as_deref().unwrap_or_default();
        let cwd = ctx.pane_cwd.as_deref().unwrap_or_default();
        for override_item in &self.prompt_config.by_project {
            if override_item.rule.matches(domain, title, cwd) {
                return &override_item.prompt;
            }
        }

        let agent_key = ctx.agent_type.to_string();
        if let Some(prompt) = self.prompt_config.by_agent.get(&agent_key) {
            return prompt;
        }

        &self.prompt_config.default
    }

    /// Extract agent type from trigger context, if available.
    fn agent_type_from_trigger(ctx: &WorkflowContext) -> crate::patterns::AgentType {
        ctx.trigger()
            .and_then(|t| t.get("agent_type"))
            .and_then(|v| v.as_str())
            .map_or(crate::patterns::AgentType::Unknown, |s| match s {
                "claude_code" => crate::patterns::AgentType::ClaudeCode,
                "codex" => crate::patterns::AgentType::Codex,
                "gemini" => crate::patterns::AgentType::Gemini,
                _ => crate::patterns::AgentType::Unknown,
            })
    }

    /// Check if pane state allows workflow execution.
    ///
    /// Guards against:
    /// - Alt-screen mode (vim, less, etc.)
    /// - Recent output gap (unknown pane state)
    /// - Command currently running
    fn check_pane_guards(ctx: &WorkflowContext) -> Result<(), String> {
        let caps = ctx.capabilities();

        // Guard: alt-screen blocks sends (Some(true) = definitely in alt-screen)
        if caps.alt_screen == Some(true) {
            return Err("Pane is in alt-screen mode (vim, less, etc.) - aborting".to_string());
        }

        // Guard: command running could cause issues
        if caps.command_running {
            return Err("Command is currently running in pane - aborting".to_string());
        }

        // Guard: recent gap suggests unknown state
        if caps.has_recent_gap {
            return Err("Recent output gap detected - pane state uncertain".to_string());
        }

        Ok(())
    }

    /// Wait until output has been stable for the requested window.
    ///
    /// Uses captured output activity timestamps from storage to avoid
    /// reading from the pane directly. This is a best-effort stabilization
    /// strategy until deterministic compaction-complete markers are wired in.
    async fn wait_for_stable_output(
        storage: Arc<StorageHandle>,
        pane_id: u64,
        stable_for_ms: u64,
        timeout_ms: u64,
    ) -> Result<StabilizationOutcome, String> {
        if stable_for_ms == 0 {
            return Ok(StabilizationOutcome {
                waited_ms: 0,
                polls: 0,
                last_activity_ms: None,
            });
        }

        let start = Instant::now();
        let deadline = start + Duration::from_millis(timeout_ms);
        let mut interval = Duration::from_millis(50);
        let mut polls = 0usize;

        let stable_for_ms_i64 = i64::try_from(stable_for_ms).unwrap_or(i64::MAX);

        loop {
            polls += 1;

            let activity_map = storage
                .get_last_activity_by_pane()
                .await
                .map_err(|e| format!("Failed to read pane activity: {e}"))?;

            let last_activity_ms = activity_map.get(&pane_id).copied();

            // If we have no activity recorded, treat as stable enough to proceed.
            if last_activity_ms.is_none() {
                return Ok(StabilizationOutcome {
                    waited_ms: elapsed_ms(start),
                    polls,
                    last_activity_ms,
                });
            }

            let now = now_ms();
            let since_ms = now.saturating_sub(last_activity_ms.unwrap_or(now));
            if since_ms >= stable_for_ms_i64 {
                return Ok(StabilizationOutcome {
                    waited_ms: elapsed_ms(start),
                    polls,
                    last_activity_ms,
                });
            }

            if Instant::now() >= deadline {
                return Err(format!(
                    "Stabilization timeout after {}ms (last_activity_ms={:?}, stable_for_ms={})",
                    elapsed_ms(start),
                    last_activity_ms,
                    stable_for_ms
                ));
            }

            sleep(interval).await;
            interval = interval.saturating_mul(2);
            if interval > Duration::from_secs(1) {
                interval = Duration::from_secs(1);
            }
        }
    }
}

impl Workflow for HandleCompaction {
    fn name(&self) -> &'static str {
        "handle_compaction"
    }

    fn description(&self) -> &'static str {
        "Re-inject critical context (AGENTS.md) after conversation compaction"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        // Handle any compaction-related detection
        detection.event_type == "session.compaction" || detection.rule_id.contains("compaction")
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows injection"),
            WorkflowStep::new("stabilize", "Wait for compaction output to stabilize"),
            WorkflowStep::new("send_prompt", "Send agent-specific context refresh prompt"),
            WorkflowStep::new("verify_send", "Verify the prompt was processed"),
        ]
    }

    fn to_action_plan(
        &self,
        ctx: &WorkflowContext,
        execution_id: &str,
    ) -> Option<crate::plan::ActionPlan> {
        let pane_id = ctx.pane_id();
        let workspace_id = ctx.workspace_id().unwrap_or("default");
        let prompt = self.resolve_prompt(ctx);

        let check_guards = crate::plan::StepPlan::new(
            1,
            crate::plan::StepAction::Custom {
                action_type: "check_guards".to_string(),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                }),
            },
            "Validate pane state allows injection",
        );

        let stabilize = crate::plan::StepPlan::new(
            2,
            crate::plan::StepAction::Custom {
                action_type: "stabilize_output".to_string(),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                    "stable_for_ms": self.stabilization_ms,
                    "timeout_ms": self.idle_timeout_ms,
                }),
            },
            "Wait for compaction output to stabilize",
        );

        let send_prompt = crate::plan::StepPlan::new(
            3,
            crate::plan::StepAction::SendText {
                pane_id,
                text: prompt,
                paste_mode: None,
            },
            "Send agent-specific context refresh prompt",
        )
        .idempotent();

        let verify_send = crate::plan::StepPlan::new(
            4,
            crate::plan::StepAction::Custom {
                action_type: "verify_send".to_string(),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                }),
            },
            "Verify the prompt was processed",
        );

        Some(
            crate::plan::ActionPlan::builder(self.description(), workspace_id)
                .add_steps([check_guards, stabilize, send_prompt, verify_send])
                .metadata(serde_json::json!({
                    "workflow_name": self.name(),
                    "execution_id": execution_id,
                    "pane_id": pane_id,
                }))
                .created_at(now_ms())
                .build(),
        )
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        // Capture all values needed in the async block BEFORE entering it.
        // This avoids lifetime issues since we own the captured values.
        let stabilization_ms = self.stabilization_ms;
        let idle_timeout_ms = self.idle_timeout_ms;
        let pane_id = ctx.pane_id();
        let execution_id = ctx.execution_id().to_string();
        let storage = Arc::clone(ctx.storage());

        // For step 0: capture guard check result
        let guard_check_result = if step_idx == 0 {
            Some(Self::check_pane_guards(ctx))
        } else {
            None
        };

        // For step 2: capture prompt and injector availability
        let prompt = if step_idx == 2 {
            Some(self.resolve_prompt(ctx))
        } else {
            None
        };
        let has_injector = ctx.has_injector();

        // For step 3: capture trigger info
        let (tokens_before, tokens_after) = if step_idx == 3 {
            let before = ctx
                .trigger()
                .and_then(|t| t.get("extracted"))
                .and_then(|e| e.get("tokens_before"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let after = ctx
                .trigger()
                .and_then(|t| t.get("extracted"))
                .and_then(|e| e.get("tokens_after"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            (before, after)
        } else {
            (String::new(), String::new())
        };

        Box::pin(async move {
            match step_idx {
                // Step 0: Check guards - validate pane state
                0 => {
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_compaction: checking pane guards"
                    );

                    if let Some(Err(reason)) = guard_check_result {
                        tracing::warn!(
                            pane_id,
                            reason = %reason,
                            "handle_compaction: guard check failed"
                        );
                        return StepResult::abort(reason);
                    }

                    tracing::debug!(
                        pane_id,
                        "handle_compaction: guards passed, proceeding to stabilization"
                    );
                    StepResult::cont()
                }

                // Step 1: Stabilize - wait for pane to be idle
                1 => {
                    tracing::info!(
                        pane_id,
                        stabilization_ms,
                        idle_timeout_ms,
                        "handle_compaction: waiting for output to stabilize"
                    );

                    match Self::wait_for_stable_output(
                        storage.clone(),
                        pane_id,
                        stabilization_ms,
                        idle_timeout_ms,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            tracing::info!(
                                pane_id,
                                waited_ms = outcome.waited_ms,
                                polls = outcome.polls,
                                last_activity_ms = ?outcome.last_activity_ms,
                                "handle_compaction: output stabilized"
                            );
                            StepResult::cont()
                        }
                        Err(reason) => {
                            tracing::warn!(pane_id, reason = %reason, "handle_compaction: stabilization failed");
                            StepResult::abort(reason)
                        }
                    }
                }

                // Step 2: Send agent-specific prompt
                // The runner will handle the actual text injection via policy-gated injector.
                2 => {
                    let prompt = prompt.unwrap_or_else(|| compaction_prompts::UNKNOWN.to_string());

                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        prompt_len = prompt.len(),
                        "handle_compaction: sending context refresh prompt"
                    );

                    // Check if injector is available
                    if !has_injector {
                        tracing::error!(pane_id, "handle_compaction: no injector configured");
                        return StepResult::abort("No injector configured for text injection");
                    }

                    // Use SendText to request the runner inject the prompt.
                    // The runner will call the policy-gated injector and abort if denied.
                    StepResult::send_text(prompt)
                }

                // Step 3: Verify the send (best-effort)
                3 => {
                    // For now, we consider the workflow done after the send step.
                    // Future: wait for OSC 133 prompt boundary or agent response pattern.
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_compaction: workflow completed successfully"
                    );

                    StepResult::done(serde_json::json!({
                        "status": "completed",
                        "pane_id": pane_id,
                        "tokens_before": tokens_before,
                        "tokens_after": tokens_after,
                        "action": "sent_context_refresh_prompt"
                    }))
                }

                _ => {
                    tracing::error!(
                        pane_id,
                        step_idx,
                        "handle_compaction: unexpected step index"
                    );
                    StepResult::abort(format!("Unexpected step index: {step_idx}"))
                }
            }
        })
    }

    fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
        // Note: We don't use ctx here because the async block would need to capture
        // values from ctx, which has a different lifetime. For a simple cleanup,
        // we just log that cleanup was called.
        Box::pin(async move {
            tracing::debug!("handle_compaction: cleanup completed");
        })
    }
}

/// Handle usage limits workflow: exit agent, persist session, and select new account.
pub struct HandleUsageLimits;

impl HandleUsageLimits {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for HandleUsageLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleUsageLimits {
    fn name(&self) -> &'static str {
        "handle_usage_limits"
    }

    fn description(&self) -> &'static str {
        "Exit agent, persist session summary, and select new account for failover"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.rule_id.contains("usage")
            && detection.agent_type == crate::patterns::AgentType::Codex
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new("exit_and_persist", "Exit Codex and persist session summary"),
            WorkflowStep::new("select_account", "Select best available account"),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let ctx_clone = ctx.clone();

        Box::pin(async move {
            match step_idx {
                0 => {
                    // Best-effort usage-limit metric (do not fail the workflow on storage errors).
                    let trigger = ctx_clone
                        .trigger()
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let now = now_ms();
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let rule_id = trigger.get("rule_id").and_then(|v| v.as_str());
                    let extracted = trigger
                        .get("extracted")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);

                    if let Err(err) = storage
                        .record_usage_metric(crate::storage::UsageMetricRecord {
                            id: 0,
                            timestamp: now,
                            metric_type: crate::storage::MetricType::RateLimitHit,
                            pane_id: Some(pane_id),
                            agent_type,
                            account_id: None,
                            workflow_id: None,
                            count: Some(1),
                            amount: None,
                            tokens: None,
                            metadata: Some(
                                serde_json::json!({
                                    "source": "workflow.handle_usage_limits",
                                    "rule_id": rule_id,
                                    "extracted": extracted,
                                })
                                .to_string(),
                            ),
                            created_at: now,
                        })
                        .await
                    {
                        tracing::warn!(
                            pane_id,
                            error = %err,
                            "handle_usage_limits: failed to record rate limit metric"
                        );
                    }

                    let caps = ctx_clone.capabilities();
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running");
                    }
                    StepResult::cont()
                }
                1 => {
                    let wezterm = default_wezterm_handle();
                    let source = WeztermHandleSource::new(Arc::clone(&wezterm));
                    let options = CodexExitOptions::default();

                    let outcome = codex_exit_and_wait_for_summary(
                        pane_id,
                        &source,
                        || {
                            let mut c = ctx_clone.clone();
                            async move { c.send_ctrl_c().await.map_err(ToString::to_string) }
                        },
                        &options,
                    )
                    .await;

                    match outcome {
                        Ok(_) => {
                            let text = match wezterm.get_text(pane_id, false).await {
                                Ok(t) => t,
                                Err(e) => {
                                    return StepResult::abort(format!("Failed to get text: {e}"));
                                }
                            };
                            let tail = crate::wezterm::tail_text(&text, 200);

                            match parse_codex_session_summary(&tail) {
                                Ok(parsed) => {
                                    if let Err(e) =
                                        persist_codex_session_summary(&storage, pane_id, &parsed)
                                            .await
                                    {
                                        tracing::warn!("Failed to persist session summary: {e}");
                                    }
                                    StepResult::cont()
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse session summary: {e}");
                                    StepResult::cont()
                                }
                            }
                        }
                        Err(e) => StepResult::abort(format!("Failed to exit Codex: {e}")),
                    }
                }
                2 => {
                    let caut_client = crate::caut::CautClient::new();
                    let config = crate::accounts::AccountSelectionConfig::default();
                    let result = refresh_and_select_account(&caut_client, &storage, &config).await;

                    match result {
                        Ok(selection) => {
                            if selection.selected.is_some() {
                                if matches!(
                                    selection.quota_advisory.availability,
                                    crate::accounts::QuotaAvailability::Low
                                ) {
                                    tracing::warn!(
                                        pane_id,
                                        selected_percent = ?selection.quota_advisory.selected_percent_remaining,
                                        threshold_percent = selection.quota_advisory.low_quota_threshold_percent,
                                        warning = ?selection.quota_advisory.warning,
                                        "handle_usage_limits: selected account has low remaining quota"
                                    );
                                }
                                // Account available — proceed with failover
                                let json = serde_json::to_value(&selection).unwrap_or_default();
                                StepResult::done(json)
                            } else {
                                // All accounts exhausted — enter safe fallback path (wa-4r7)
                                tracing::warn!(
                                    pane_id,
                                    total = selection.explanation.total_considered,
                                    "handle_usage_limits: all accounts exhausted, entering fallback"
                                );

                                // Fetch accounts for reset time calculation
                                let accounts = storage
                                    .get_accounts_by_service("openai")
                                    .await
                                    .unwrap_or_default();
                                let exhaustion = crate::accounts::build_exhaustion_info(
                                    &accounts,
                                    selection.explanation,
                                );

                                let plan = build_all_accounts_exhausted_plan(
                                    pane_id,
                                    exhaustion.accounts_checked,
                                    None, // resume_session_id not available at this step
                                    exhaustion.earliest_reset_ms,
                                    now_ms(),
                                );

                                tracing::info!(
                                    pane_id,
                                    accounts_checked = exhaustion.accounts_checked,
                                    earliest_reset_ms = ?exhaustion.earliest_reset_ms,
                                    earliest_reset_account = ?exhaustion.earliest_reset_account,
                                    "handle_usage_limits: built fallback plan"
                                );

                                fallback_plan_to_step_result(&plan)
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_usage_limits: account selection failed"
                            );
                            StepResult::abort(e.to_string())
                        }
                    }
                }
                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleSessionEnd — persist structured session summaries (wa-nu4.2.2.3)
// ============================================================================

/// Persist structured session summaries when agents emit session.summary or session.end events.
///
/// Supported agents: Codex, Claude Code, Gemini.
/// Extracts available fields (session_id, tokens, cost, end_reason) from the
/// detection trigger and upserts an `AgentSessionRecord`.
pub struct HandleSessionEnd;

impl HandleSessionEnd {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Build an [`AgentSessionRecord`] from a detection trigger's extracted fields.
    fn record_from_detection(
        pane_id: u64,
        detection: &serde_json::Value,
    ) -> crate::storage::AgentSessionRecord {
        let agent_type_str = detection
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let extracted = detection.get("extracted");

        let mut record = crate::storage::AgentSessionRecord::new_start(pane_id, agent_type_str);
        let now = now_ms();
        record.ended_at = Some(now);

        // Determine end_reason from event_type
        let event_type = detection
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        record.end_reason = Some(match event_type {
            "session.end" => "completed".to_string(),
            "session.summary" => "completed".to_string(),
            other => other.to_string(),
        });

        // Extract session_id (Codex resume hint, Gemini session summary)
        if let Some(ext) = extracted {
            record.session_id = ext
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);

            // Token fields (Codex session.summary)
            record.total_tokens = ext
                .get("total")
                .and_then(|v| v.as_str())
                .and_then(parse_number);
            record.input_tokens = ext
                .get("input")
                .and_then(|v| v.as_str())
                .and_then(parse_number);
            record.output_tokens = ext
                .get("output")
                .and_then(|v| v.as_str())
                .and_then(parse_number);
            record.cached_tokens = ext
                .get("cached")
                .and_then(|v| v.as_str())
                .and_then(parse_number);
            record.reasoning_tokens = ext
                .get("reasoning")
                .and_then(|v| v.as_str())
                .and_then(parse_number);

            // Cost field (Claude Code session.cost_summary)
            record.estimated_cost_usd = ext
                .get("cost")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok());

            // Model name (if present in extracted)
            record.model_name = ext
                .get("model")
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
        }

        record
    }
}

impl Default for HandleSessionEnd {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleSessionEnd {
    fn name(&self) -> &'static str {
        "handle_session_end"
    }

    fn description(&self) -> &'static str {
        "Persist structured session summary when an agent session ends"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        matches!(
            detection.event_type.as_str(),
            "session.summary" | "session.end"
        )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["session.summary", "session.end"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["codex", "claude_code", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "extract_summary",
                "Extract structured session data from detection",
            ),
            WorkflowStep::new("persist_record", "Persist session record to database"),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);

        Box::pin(async move {
            match step_idx {
                // Step 0: Extract and validate detection data
                0 => {
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    tracing::info!(
                        pane_id,
                        agent_type,
                        event_type,
                        has_trigger = !trigger.is_null(),
                        "handle_session_end: extracted session data from detection"
                    );

                    StepResult::cont()
                }

                // Step 1: Build and persist the session record
                1 => {
                    let mut record = Self::record_from_detection(pane_id, &trigger);

                    // If we already have an active session for this pane, prefer updating it
                    // rather than inserting a second record. This makes duration metrics meaningful.
                    if let Ok(existing) = storage.get_sessions_for_pane(pane_id).await {
                        let want_session_id = record.session_id.as_deref();
                        let candidate = existing
                            .into_iter()
                            .filter(|s| s.ended_at.is_none() && s.agent_type == record.agent_type)
                            .filter(|s| {
                                want_session_id.is_none_or(|id| s.session_id.as_deref() == Some(id))
                            })
                            .max_by_key(|s| s.started_at);

                        if let Some(active) = candidate {
                            record.id = active.id;
                            record.started_at = active.started_at;
                            if record.session_id.is_none() {
                                record.session_id = active.session_id;
                            }
                            if record.external_id.is_none() {
                                record.external_id = active.external_id;
                            }
                            if record.external_meta.is_none() {
                                record.external_meta = active.external_meta;
                            }
                        }
                    }

                    let agent_type = record.agent_type.clone();
                    let session_id = record.session_id.clone();
                    let has_tokens = record.total_tokens.is_some();
                    let has_cost = record.estimated_cost_usd.is_some();
                    let record_for_metrics = record.clone();

                    match storage.upsert_agent_session(record).await {
                        Ok(db_id) => {
                            // Best-effort usage metrics derived from the persisted session record.
                            // If these fail, don't fail the workflow.
                            let mut metrics: Vec<crate::storage::UsageMetricRecord> = Vec::new();
                            let now = now_ms();

                            if let Some(total) = record_for_metrics.total_tokens {
                                metrics.push(crate::storage::UsageMetricRecord {
                                    id: 0,
                                    timestamp: now,
                                    metric_type: crate::storage::MetricType::TokenUsage,
                                    pane_id: Some(pane_id),
                                    agent_type: Some(record_for_metrics.agent_type.clone()),
                                    account_id: None,
                                    workflow_id: None,
                                    count: None,
                                    amount: None,
                                    tokens: Some(total),
                                    metadata: Some(
                                        serde_json::json!({
                                            "source": "workflow.handle_session_end",
                                            "session_id": record_for_metrics.session_id.clone(),
                                            "input_tokens": record_for_metrics.input_tokens,
                                            "output_tokens": record_for_metrics.output_tokens,
                                            "cached_tokens": record_for_metrics.cached_tokens,
                                            "reasoning_tokens": record_for_metrics.reasoning_tokens,
                                            "model": record_for_metrics.model_name.clone(),
                                        })
                                        .to_string(),
                                    ),
                                    created_at: now,
                                });
                            }

                            if let Some(cost) = record_for_metrics.estimated_cost_usd {
                                metrics.push(crate::storage::UsageMetricRecord {
                                    id: 0,
                                    timestamp: now,
                                    metric_type: crate::storage::MetricType::ApiCost,
                                    pane_id: Some(pane_id),
                                    agent_type: Some(record_for_metrics.agent_type.clone()),
                                    account_id: None,
                                    workflow_id: None,
                                    count: None,
                                    amount: Some(cost),
                                    tokens: None,
                                    metadata: Some(
                                        serde_json::json!({
                                            "source": "workflow.handle_session_end",
                                            "session_id": record_for_metrics.session_id.clone(),
                                            "model": record_for_metrics.model_name.clone(),
                                        })
                                        .to_string(),
                                    ),
                                    created_at: now,
                                });
                            }

                            if let Some(ended_at) = record_for_metrics.ended_at {
                                let duration_ms =
                                    ended_at.saturating_sub(record_for_metrics.started_at);
                                let duration_secs = duration_ms / 1000;
                                metrics.push(crate::storage::UsageMetricRecord {
                                    id: 0,
                                    timestamp: ended_at,
                                    metric_type: crate::storage::MetricType::SessionDuration,
                                    pane_id: Some(pane_id),
                                    agent_type: Some(record_for_metrics.agent_type.clone()),
                                    account_id: None,
                                    workflow_id: None,
                                    count: Some(duration_secs),
                                    amount: None,
                                    tokens: None,
                                    metadata: Some(
                                        serde_json::json!({
                                            "source": "workflow.handle_session_end",
                                            "session_id": record_for_metrics.session_id.clone(),
                                            "duration_ms": duration_ms,
                                        })
                                        .to_string(),
                                    ),
                                    created_at: now,
                                });
                            }

                            if !metrics.is_empty() {
                                if let Err(err) = storage.record_usage_metrics_batch(metrics).await
                                {
                                    tracing::warn!(
                                        pane_id,
                                        error = %err,
                                        "handle_session_end: failed to record usage metrics"
                                    );
                                }
                            }

                            tracing::info!(
                                pane_id,
                                db_id,
                                agent_type = %agent_type,
                                session_id = ?session_id,
                                has_tokens,
                                has_cost,
                                "handle_session_end: persisted session record"
                            );

                            StepResult::done(serde_json::json!({
                                "status": "persisted",
                                "db_id": db_id,
                                "pane_id": pane_id,
                                "agent_type": agent_type,
                                "session_id": session_id,
                                "has_tokens": has_tokens,
                                "has_cost": has_cost,
                            }))
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_session_end: failed to persist session record"
                            );
                            StepResult::abort(format!("Failed to persist session: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleProcessTriageLifecycle — deterministic process-triage lifecycle wiring
// ============================================================================

const PROCESS_TRIAGE_LIFECYCLE_EVENT_TYPE: &str = "process_triage.lifecycle";
const PROCESS_TRIAGE_LIFECYCLE_RULE_ID: &str = "process_triage.lifecycle";

#[derive(Debug, Clone, Copy)]
struct ProcessTriagePlanStats {
    entry_count: usize,
    auto_safe_count: usize,
    review_count: usize,
    protected_count: usize,
    has_protected_destructive: bool,
}

impl ProcessTriagePlanStats {
    fn verify_invariants(self) -> Result<(), String> {
        let total = self
            .auto_safe_count
            .saturating_add(self.review_count)
            .saturating_add(self.protected_count);
        if total != self.entry_count {
            return Err(format!(
                "triage plan counts mismatch (entries={}, auto_safe={}, review={}, protected={})",
                self.entry_count, self.auto_safe_count, self.review_count, self.protected_count
            ));
        }
        Ok(())
    }
}

/// Wire process-triage lifecycle phases into the durable workflow runner.
///
/// This workflow does not mutate pane state directly. It provides deterministic
/// orchestration and explicit abort semantics for the six lifecycle phases:
/// snapshot -> plan -> apply -> verify -> diff -> session.
pub struct HandleProcessTriageLifecycle;

impl HandleProcessTriageLifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    fn triage_plan_value(trigger: &serde_json::Value) -> Option<&serde_json::Value> {
        trigger
            .pointer("/process_triage/plan")
            .or_else(|| trigger.pointer("/extracted/triage_plan"))
            .or_else(|| trigger.get("triage_plan"))
    }

    fn snapshot_value(
        trigger: &serde_json::Value,
        pane_id: u64,
        execution_id: &str,
    ) -> serde_json::Value {
        trigger
            .pointer("/process_triage/snapshot")
            .or_else(|| trigger.pointer("/extracted/process_snapshot"))
            .or_else(|| trigger.get("process_snapshot"))
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "status": "synthetic",
                    "pane_id": pane_id,
                    "execution_id": execution_id,
                    "captured_at_ms": now_ms(),
                })
            })
    }

    fn diff_value(trigger: &serde_json::Value, stats: ProcessTriagePlanStats) -> serde_json::Value {
        trigger
            .pointer("/process_triage/diff")
            .or_else(|| trigger.pointer("/extracted/triage_diff"))
            .or_else(|| trigger.get("triage_diff"))
            .cloned()
            .unwrap_or_else(|| {
                serde_json::json!({
                    "status": "derived",
                    "entry_count": stats.entry_count,
                    "auto_safe_count": stats.auto_safe_count,
                    "review_count": stats.review_count,
                    "protected_count": stats.protected_count,
                })
            })
    }

    fn plan_stats_from_trigger(
        trigger: &serde_json::Value,
    ) -> Result<ProcessTriagePlanStats, String> {
        let Some(plan) = Self::triage_plan_value(trigger) else {
            return Ok(ProcessTriagePlanStats {
                entry_count: 0,
                auto_safe_count: 0,
                review_count: 0,
                protected_count: 0,
                has_protected_destructive: false,
            });
        };
        Self::plan_stats(plan)
    }

    fn plan_stats(plan: &serde_json::Value) -> Result<ProcessTriagePlanStats, String> {
        let entries: Vec<&serde_json::Value> = match plan.get("entries") {
            Some(raw) => raw
                .as_array()
                .ok_or_else(|| "triage plan entries must be an array".to_string())?
                .iter()
                .collect(),
            None => Vec::new(),
        };

        let mut inferred_auto_safe = 0usize;
        let mut inferred_review = 0usize;
        let mut inferred_protected = 0usize;
        let mut has_protected_destructive = false;

        for entry in &entries {
            let category = entry
                .get("category")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let action = entry
                .pointer("/action/action")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            if Self::category_is_auto_safe(category) {
                inferred_auto_safe = inferred_auto_safe.saturating_add(1);
            } else if Self::category_is_protected(category) {
                inferred_protected = inferred_protected.saturating_add(1);
                if !matches!(action, "protect" | "renice" | "flag_for_review") {
                    has_protected_destructive = true;
                }
            } else {
                inferred_review = inferred_review.saturating_add(1);
            }
        }

        let read_count = |key: &str, fallback: usize| -> Result<usize, String> {
            match plan.get(key) {
                Some(raw) => raw
                    .as_u64()
                    .and_then(|v| usize::try_from(v).ok())
                    .ok_or_else(|| {
                        format!("triage plan field '{key}' must be a non-negative integer")
                    }),
                None => Ok(fallback),
            }
        };

        Ok(ProcessTriagePlanStats {
            entry_count: entries.len(),
            auto_safe_count: read_count("auto_safe_count", inferred_auto_safe)?,
            review_count: read_count("review_count", inferred_review)?,
            protected_count: read_count("protected_count", inferred_protected)?,
            has_protected_destructive,
        })
    }

    fn category_is_auto_safe(category: &str) -> bool {
        matches!(
            category,
            "zombie" | "stuck_test" | "stuck_cli" | "duplicate_build"
        )
    }

    fn category_is_protected(category: &str) -> bool {
        matches!(category, "active_agent" | "system_process")
    }

    fn session_artifact(
        trigger: &serde_json::Value,
        pane_id: u64,
        execution_id: &str,
    ) -> serde_json::Value {
        let ft_session_id = trigger
            .pointer("/process_triage/ft_session_id")
            .or_else(|| trigger.pointer("/extracted/ft_session_id"))
            .or_else(|| trigger.get("ft_session_id"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("ft-{pane_id}-{execution_id}"));

        let pt_session_id = trigger
            .pointer("/process_triage/pt_session_id")
            .or_else(|| trigger.pointer("/extracted/pt_session_id"))
            .or_else(|| trigger.get("pt_session_id"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| format!("pt-{execution_id}"));

        let provider = trigger
            .pointer("/process_triage/provider")
            .or_else(|| trigger.pointer("/extracted/provider"))
            .or_else(|| trigger.get("provider"))
            .and_then(|v| v.as_str())
            .unwrap_or("heuristic");

        serde_json::json!({
            "ft_session_id": ft_session_id,
            "pt_session_id": pt_session_id,
            "provider": provider,
        })
    }
}

impl Default for HandleProcessTriageLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleProcessTriageLifecycle {
    fn name(&self) -> &'static str {
        "handle_process_triage_lifecycle"
    }

    fn description(&self) -> &'static str {
        "Orchestrate process triage lifecycle phases (snapshot, plan, apply, verify, diff, session)"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.event_type == PROCESS_TRIAGE_LIFECYCLE_EVENT_TYPE
            || detection.rule_id == PROCESS_TRIAGE_LIFECYCLE_RULE_ID
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &[PROCESS_TRIAGE_LIFECYCLE_EVENT_TYPE]
    }

    fn trigger_rule_ids(&self) -> &'static [&'static str] {
        &[PROCESS_TRIAGE_LIFECYCLE_RULE_ID]
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "snapshot",
                "Capture process snapshot baseline for lifecycle run",
            ),
            WorkflowStep::new(
                "plan",
                "Build triage plan artifact from snapshot/provider data",
            ),
            WorkflowStep::new(
                "apply",
                "Apply triage plan actions through policy-gated semantics",
            ),
            WorkflowStep::new("verify", "Verify triage outcomes and invariant integrity"),
            WorkflowStep::new(
                "diff",
                "Produce pre/post diff summary for audit and diagnostics",
            ),
            WorkflowStep::new(
                "session",
                "Emit session correlation artifact and finalize lifecycle",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let execution_id = ctx.execution_id().to_string();
        let capabilities = ctx.capabilities().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);

        Box::pin(async move {
            let stats = match Self::plan_stats_from_trigger(&trigger) {
                Ok(stats) => stats,
                Err(reason) => {
                    return StepResult::abort(format!(
                        "process triage lifecycle: invalid triage plan payload: {reason}"
                    ));
                }
            };

            match step_idx {
                0 => {
                    if capabilities.alt_screen == Some(true) {
                        return StepResult::abort(
                            "process triage lifecycle: pane in alt-screen mode; refusing snapshot step",
                        );
                    }
                    if capabilities.command_running {
                        return StepResult::abort(
                            "process triage lifecycle: command currently running; refusing snapshot step",
                        );
                    }
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_process_triage_lifecycle: snapshot step completed"
                    );
                    StepResult::cont()
                }
                1 => {
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        entry_count = stats.entry_count,
                        auto_safe_count = stats.auto_safe_count,
                        review_count = stats.review_count,
                        protected_count = stats.protected_count,
                        "handle_process_triage_lifecycle: plan step completed"
                    );
                    StepResult::cont()
                }
                2 => {
                    if stats.has_protected_destructive {
                        return StepResult::abort(
                            "process triage lifecycle: protected category includes destructive action",
                        );
                    }
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        auto_safe_count = stats.auto_safe_count,
                        review_count = stats.review_count,
                        protected_count = stats.protected_count,
                        "handle_process_triage_lifecycle: apply step completed"
                    );
                    StepResult::cont()
                }
                3 => match stats.verify_invariants() {
                    Ok(()) => {
                        tracing::info!(
                            pane_id,
                            execution_id = %execution_id,
                            "handle_process_triage_lifecycle: verify step completed"
                        );
                        StepResult::cont()
                    }
                    Err(reason) => StepResult::abort(format!(
                        "process triage lifecycle: verify step failed: {reason}"
                    )),
                },
                4 => {
                    let diff = Self::diff_value(&trigger, stats);
                    if !diff.is_object() {
                        return StepResult::abort(
                            "process triage lifecycle: diff artifact must be a JSON object",
                        );
                    }
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_process_triage_lifecycle: diff step completed"
                    );
                    StepResult::cont()
                }
                5 => {
                    let snapshot = Self::snapshot_value(&trigger, pane_id, &execution_id);
                    let plan = Self::triage_plan_value(&trigger)
                        .cloned()
                        .unwrap_or_else(|| {
                            serde_json::json!({
                                "entries": [],
                                "auto_safe_count": stats.auto_safe_count,
                                "review_count": stats.review_count,
                                "protected_count": stats.protected_count,
                            })
                        });
                    let apply = trigger
                        .pointer("/process_triage/apply")
                        .or_else(|| trigger.pointer("/extracted/triage_apply"))
                        .or_else(|| trigger.get("triage_apply"))
                        .cloned()
                        .unwrap_or_else(|| {
                            serde_json::json!({
                                "status": "derived",
                                "applied_auto_safe_count": stats.auto_safe_count,
                                "requires_approval_count": stats.review_count,
                                "protected_skipped_count": stats.protected_count,
                            })
                        });
                    let verify = serde_json::json!({
                        "status": "ok",
                        "entry_count": stats.entry_count,
                        "invariants_passed": true,
                    });
                    let diff = Self::diff_value(&trigger, stats);
                    let session = Self::session_artifact(&trigger, pane_id, &execution_id);

                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_process_triage_lifecycle: session step completed"
                    );

                    StepResult::done(serde_json::json!({
                        "status": "completed",
                        "pane_id": pane_id,
                        "workflow": "handle_process_triage_lifecycle",
                        "snapshot": snapshot,
                        "plan": plan,
                        "apply": apply,
                        "verify": verify,
                        "diff": diff,
                        "session": session,
                    }))
                }
                _ => StepResult::abort(format!(
                    "process triage lifecycle: unexpected step index: {step_idx}"
                )),
            }
        })
    }
}

// ============================================================================
// HandleAuthRequired — centralize auth recovery (wa-nu4.2.2.4)
// ============================================================================

/// Default cooldown window in milliseconds (5 minutes).
/// Auth events within this window for the same pane are suppressed.
const AUTH_COOLDOWN_MS: i64 = 5 * 60 * 1000;
const AUTH_CASS_HINT_LIMIT: usize = 3;
const AUTH_CASS_TIMEOUT_SECS: u64 = 8;
const AUTH_CASS_LOOKBACK_DAYS: u32 = 30;
const AUTH_CASS_QUERY_MAX_CHARS: usize = 160;
const AUTH_CASS_HINT_MAX_CHARS: usize = 140;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AuthCassHintsLookup {
    query: Option<String>,
    workspace: Option<String>,
    hints: Vec<String>,
    error: Option<String>,
}

/// Recovery strategy for an auth-required event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "strategy")]
pub enum AuthRecoveryStrategy {
    /// Device code auth: user must enter code in browser.
    DeviceCode {
        code: Option<String>,
        url: Option<String>,
    },
    /// API key error: environment variable needs fixing.
    ApiKeyError { key_hint: Option<String> },
    /// Generic auth prompt requiring manual intervention.
    ManualIntervention { agent_type: String, hint: String },
}

impl AuthRecoveryStrategy {
    /// Determine recovery strategy from a detection trigger.
    pub fn from_detection(trigger: &serde_json::Value) -> Self {
        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let rule_id = trigger
            .get("rule_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let extracted = trigger.get("extracted");
        let agent_type = trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if event_type == "auth.device_code" || rule_id.contains("device_code") {
            let code = extracted
                .and_then(|e| e.get("code"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            let url = extracted
                .and_then(|e| e.get("url"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            Self::DeviceCode { code, url }
        } else if event_type == "auth.error" || rule_id.contains("api_key") {
            let key_hint = extracted
                .and_then(|e| e.get("key_name"))
                .and_then(|v| v.as_str())
                .map(ToString::to_string);
            Self::ApiKeyError { key_hint }
        } else {
            Self::ManualIntervention {
                agent_type: agent_type.to_string(),
                hint: format!("Auth required for {agent_type}; manual login may be needed"),
            }
        }
    }

    /// Human-readable label for the strategy.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::DeviceCode { .. } => "device_code",
            Self::ApiKeyError { .. } => "api_key_error",
            Self::ManualIntervention { .. } => "manual_intervention",
        }
    }
}

/// Centralize auth-required events into a single workflow that selects the
/// correct recovery strategy, records the outcome, and avoids spamming.
pub struct HandleAuthRequired {
    cooldown_ms: i64,
}

impl HandleAuthRequired {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: AUTH_COOLDOWN_MS,
        }
    }

    /// Create with a custom cooldown (useful for testing or configuration).
    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    fn cass_agent_from_trigger(trigger: &serde_json::Value) -> Option<CassAgent> {
        trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .and_then(CassAgent::from_slug)
    }

    fn compact_whitespace(input: &str) -> String {
        input.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn truncate_chars(input: &str, max_chars: usize) -> String {
        input.chars().take(max_chars).collect()
    }

    fn normalized_cass_query(trigger: &serde_json::Value) -> Option<String> {
        let candidates = [
            trigger.get("matched_text").and_then(|v| v.as_str()),
            trigger
                .get("extracted")
                .and_then(|v| v.get("message"))
                .and_then(|v| v.as_str()),
            trigger
                .get("extracted")
                .and_then(|v| v.get("error"))
                .and_then(|v| v.as_str()),
        ];

        for raw in candidates.into_iter().flatten() {
            let compact = Self::compact_whitespace(raw);
            if compact.is_empty() {
                continue;
            }
            return Some(Self::truncate_chars(&compact, AUTH_CASS_QUERY_MAX_CHARS));
        }

        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let agent_type = trigger
            .get("agent_type")
            .and_then(|v| v.as_str())
            .unwrap_or("agent");
        let fallback = format!("{agent_type} {event_type} auth error");
        Some(Self::truncate_chars(&fallback, AUTH_CASS_QUERY_MAX_CHARS))
    }

    fn workspace_for_pane(pane: &crate::storage::PaneRecord) -> Option<String> {
        let cwd = pane.cwd.as_deref()?;
        let parsed = crate::wezterm::CwdInfo::parse(cwd);
        if parsed.is_remote || parsed.path.is_empty() {
            return None;
        }
        Some(parsed.path)
    }

    fn format_cass_hint(hit: &CassSearchHit) -> Option<String> {
        let snippet = hit
            .content
            .as_deref()
            .map(Self::compact_whitespace)
            .unwrap_or_default();
        if snippet.is_empty() {
            return None;
        }

        let source_path = hit
            .source_path
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown");
        let line_suffix = hit
            .line_number
            .map_or_else(String::new, |line| format!(":{line}"));
        let compact_snippet = Self::truncate_chars(&snippet, AUTH_CASS_HINT_MAX_CHARS);

        Some(format!("{source_path}{line_suffix} - {compact_snippet}"))
    }

    async fn lookup_cass_hints(
        storage: &StorageHandle,
        pane_id: u64,
        trigger: &serde_json::Value,
    ) -> AuthCassHintsLookup {
        let query = Self::normalized_cass_query(trigger);
        let Some(query_value) = query.clone() else {
            return AuthCassHintsLookup::default();
        };

        let pane = match storage.get_pane(pane_id).await {
            Ok(record) => record,
            Err(error) => {
                return AuthCassHintsLookup {
                    query,
                    workspace: None,
                    hints: Vec::new(),
                    error: Some(format!("pane_lookup_failed: {error}")),
                };
            }
        };

        let workspace = pane.as_ref().and_then(Self::workspace_for_pane);

        let options = SearchOptions {
            limit: Some(AUTH_CASS_HINT_LIMIT),
            offset: None,
            agent: Self::cass_agent_from_trigger(trigger),
            workspace: workspace.clone(),
            days: Some(AUTH_CASS_LOOKBACK_DAYS),
            fields: Some("minimal".to_string()),
            max_tokens: Some(180),
        };

        let cass = CassClient::new().with_timeout_secs(AUTH_CASS_TIMEOUT_SECS);
        match cass.search(&query_value, &options).await {
            Ok(result) => {
                let hints = result
                    .hits
                    .iter()
                    .filter_map(Self::format_cass_hint)
                    .take(AUTH_CASS_HINT_LIMIT)
                    .collect();
                AuthCassHintsLookup {
                    query,
                    workspace,
                    hints,
                    error: None,
                }
            }
            Err(error) => AuthCassHintsLookup {
                query,
                workspace,
                hints: Vec::new(),
                error: Some(error.to_string()),
            },
        }
    }

    fn build_recovery_prompt(
        strategy: &AuthRecoveryStrategy,
        trigger: &serde_json::Value,
        cass_lookup: &AuthCassHintsLookup,
    ) -> String {
        let mut lines = vec![
            "Auth recovery needed for this pane.".to_string(),
            format!("Strategy: {}", strategy.label()),
        ];

        match strategy {
            AuthRecoveryStrategy::DeviceCode { code, url } => {
                if let Some(device_code) = code {
                    lines.push(format!("Device code: {device_code}"));
                }
                if let Some(login_url) = url {
                    lines.push(format!("Login URL: {login_url}"));
                }
                lines.push("Complete device auth in browser, then continue.".to_string());
            }
            AuthRecoveryStrategy::ApiKeyError { key_hint } => {
                if let Some(key_name) = key_hint {
                    lines.push(format!("Check API key variable: {key_name}"));
                } else {
                    lines.push("Check API key configuration for this agent.".to_string());
                }
                lines.push("Fix credentials, then retry the previous command.".to_string());
            }
            AuthRecoveryStrategy::ManualIntervention { hint, .. } => {
                lines.push(hint.clone());
            }
        }

        if !cass_lookup.hints.is_empty() {
            lines.push(String::new());
            lines.push("Related fixes from past sessions (cass):".to_string());
            for hint in &cass_lookup.hints {
                lines.push(format!("- {hint}"));
            }
        } else if let Some(error) = cass_lookup.error.as_deref() {
            lines.push(String::new());
            lines.push(format!("Cass lookup unavailable: {error}"));
        }

        if let Some(query) = cass_lookup.query.as_deref() {
            lines.push(String::new());
            lines.push(format!("Cass query: {query}"));
        }
        if let Some(workspace) = cass_lookup.workspace.as_deref() {
            lines.push(format!("Cass workspace filter: {workspace}"));
        }
        if let Some(matched_text) = trigger.get("matched_text").and_then(|v| v.as_str()) {
            let compact = Self::truncate_chars(&Self::compact_whitespace(matched_text), 120);
            if !compact.is_empty() {
                lines.push(format!("Detected text: {compact}"));
            }
        }

        let mut prompt = lines.join("\n");
        prompt.push('\n');
        prompt
    }
}

impl Default for HandleAuthRequired {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleAuthRequired {
    fn name(&self) -> &'static str {
        "handle_auth_required"
    }

    fn description(&self) -> &'static str {
        "Centralize auth-required events with strategy selection and cooldown"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        matches!(
            detection.event_type.as_str(),
            "auth.device_code" | "auth.error"
        )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["auth.device_code", "auth.error"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["codex", "claude_code", "gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new(
                "check_cooldown",
                "Skip if auth was recently handled for this pane",
            ),
            WorkflowStep::new("classify_auth", "Determine auth type and recovery strategy"),
            WorkflowStep::new(
                "record_and_plan",
                "Record auth event and produce recovery plan",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;

        Box::pin(async move {
            match step_idx {
                // Step 0: Check cooldown — query audit log for recent auth events
                0 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("auth_required".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_auth_ts = recent[0].ts,
                                "handle_auth_required: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_auth_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => {
                            tracing::debug!(
                                pane_id,
                                "handle_auth_required: no recent auth events, proceeding"
                            );
                            StepResult::cont()
                        }
                        Err(e) => {
                            // Non-fatal: if we can't check cooldown, proceed anyway
                            tracing::warn!(
                                pane_id,
                                error = %e,
                                "handle_auth_required: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 1: Classify the auth event
                1 => {
                    let strategy = AuthRecoveryStrategy::from_detection(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");

                    tracing::info!(
                        pane_id,
                        agent_type,
                        event_type,
                        strategy = strategy.label(),
                        "handle_auth_required: classified auth event"
                    );

                    StepResult::cont()
                }

                // Step 2: Record audit event, gather cass hints, and inject recovery prompt
                2 => {
                    let strategy = AuthRecoveryStrategy::from_detection(&trigger);
                    let strategy_json = serde_json::to_value(&strategy).unwrap_or_default();
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let event_type = trigger
                        .get("event_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let cass_lookup = Self::lookup_cass_hints(&storage, pane_id, &trigger).await;
                    let cass_hint_count = cass_lookup.hints.len();
                    let recovery_prompt =
                        Self::build_recovery_prompt(&strategy, &trigger, &cass_lookup);

                    // Record the auth event in the audit log
                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: now_ms(),
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "auth_required".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(format!(
                            "Auth required for {agent_type}: {}",
                            strategy.label()
                        )),
                        verification_summary: None,
                        decision_context: Some(
                            serde_json::to_string(&serde_json::json!({
                                "strategy": strategy_json,
                                "event_type": event_type,
                                "cass_query": cass_lookup.query,
                                "cass_workspace": cass_lookup.workspace,
                                "cass_hints": cass_lookup.hints,
                                "cass_lookup_error": cass_lookup.error,
                            }))
                            .unwrap_or_default(),
                        ),
                        result: "recorded".to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                strategy = strategy.label(),
                                "handle_auth_required: recorded auth event"
                            );
                            tracing::info!(
                                pane_id,
                                audit_id,
                                event_type,
                                cass_hint_count,
                                "handle_auth_required: injecting auth recovery prompt"
                            );
                            StepResult::send_text(recovery_prompt)
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_auth_required: failed to record auth event"
                            );
                            StepResult::abort(format!("Failed to record auth event: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleClaudeCodeLimits — safe-pause on Claude Code usage/rate limits
// (wa-03j, wa-nu4.2.2.1)
// ============================================================================

/// Default cooldown window in milliseconds (10 minutes).
/// Usage-limit events within this window for the same pane are suppressed.
const CLAUDE_CODE_LIMITS_COOLDOWN_MS: i64 = 10 * 60 * 1000;

/// Handle Claude Code usage-limit and rate-limit events.
///
/// Unlike the Codex-specific [`HandleUsageLimits`], this workflow does **not**
/// attempt account rotation or automated exit. Instead it:
///   1. Guards against unsafe pane states.
///   2. Applies a cooldown so repeated limit events don't spam actions.
///   3. Classifies the limit type (usage warning, usage reached, rate limit).
///   4. Persists an audit record and produces a recovery plan the operator
///      can act on (wait for reset, switch accounts manually, etc.).
pub struct HandleClaudeCodeLimits {
    cooldown_ms: i64,
}

impl HandleClaudeCodeLimits {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: CLAUDE_CODE_LIMITS_COOLDOWN_MS,
        }
    }

    /// Create with a custom cooldown (useful for testing).
    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    /// Classify the limit type from a detection trigger.
    fn classify_limit(trigger: &serde_json::Value) -> (&'static str, Option<String>) {
        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let extracted = trigger.get("extracted");

        let reset_time = extracted
            .and_then(|e| e.get("reset_time"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        let limit_type = match event_type {
            "usage.warning" => "usage_warning",
            "usage.reached" => "usage_reached",
            _ => "unknown_limit",
        };

        (limit_type, reset_time)
    }

    /// Build a recovery plan JSON object for the operator.
    fn build_recovery_plan(
        limit_type: &str,
        reset_time: Option<&str>,
        pane_id: u64,
    ) -> serde_json::Value {
        let next_steps = match limit_type {
            "usage_warning" => vec![
                "Save current work and commit progress",
                "Consider wrapping up the current task",
                "If approaching hard limit, start a new session",
            ],
            "usage_reached" => {
                let mut steps = vec![
                    "Session has hit its usage limit",
                    "Do not send further input to avoid wasted tokens",
                ];
                if reset_time.is_some() {
                    steps.push("Wait for the limit to reset (see reset_time)");
                }
                steps.push("Or start a new Claude Code session manually");
                steps
            }
            _ => vec!["Unknown limit type; check pane output for details"],
        };

        serde_json::json!({
            "limit_type": limit_type,
            "pane_id": pane_id,
            "reset_time": reset_time,
            "next_steps": next_steps,
            "safe_to_send": limit_type == "usage_warning",
        })
    }
}

impl Default for HandleClaudeCodeLimits {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleClaudeCodeLimits {
    fn name(&self) -> &'static str {
        "handle_claude_code_limits"
    }

    fn description(&self) -> &'static str {
        "Safe-pause on Claude Code usage/rate limits with recovery plan"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.agent_type == crate::patterns::AgentType::ClaudeCode
            && matches!(
                detection.event_type.as_str(),
                "usage.warning" | "usage.reached"
            )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["usage.warning", "usage.reached"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["claude_code"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new(
                "check_cooldown",
                "Skip if usage limit was recently handled for this pane",
            ),
            WorkflowStep::new(
                "classify_and_record",
                "Classify limit type, record audit event, and build recovery plan",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;
        let caps = ctx.capabilities().to_owned();

        Box::pin(async move {
            match step_idx {
                // Step 0: Guard checks
                0 => {
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running in pane");
                    }
                    tracing::debug!(pane_id, "handle_claude_code_limits: guard checks passed");
                    StepResult::cont()
                }

                // Step 1: Cooldown check
                1 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("claude_code_usage_limit".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_limit_ts = recent[0].ts,
                                "handle_claude_code_limits: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_limit_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => {
                            tracing::debug!(
                                pane_id,
                                "handle_claude_code_limits: no recent limit events, proceeding"
                            );
                            StepResult::cont()
                        }
                        Err(e) => {
                            tracing::warn!(
                                pane_id,
                                error = %e,
                                "handle_claude_code_limits: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                // Step 2: Classify limit, record audit event, build plan
                2 => {
                    let (limit_type, reset_time) = Self::classify_limit(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("claude_code");
                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);

                    let plan =
                        Self::build_recovery_plan(limit_type, reset_time.as_deref(), pane_id);

                    // Record the limit event in the audit log
                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: now_ms(),
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "claude_code_usage_limit".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(format!("Claude Code {limit_type} on pane {pane_id}")),
                        verification_summary: None,
                        decision_context: Some(serde_json::to_string(&plan).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "quota audit plan serialization failed");
                            String::new()
                        })),
                        result: "recorded".to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                limit_type,
                                reset_time = ?reset_time,
                                "handle_claude_code_limits: recorded usage limit event"
                            );

                            StepResult::done(serde_json::json!({
                                "status": "recorded",
                                "pane_id": pane_id,
                                "agent_type": agent_type,
                                "limit_type": limit_type,
                                "reset_time": reset_time,
                                "recovery_plan": plan,
                                "audit_id": audit_id,
                            }))
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_claude_code_limits: failed to record limit event"
                            );
                            StepResult::abort(format!("Failed to record usage limit event: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// HandleGeminiQuota — safe-pause on Gemini usage/quota limits (wa-smm)
// ============================================================================

/// Default cooldown window in milliseconds (10 minutes).
const GEMINI_QUOTA_COOLDOWN_MS: i64 = 10 * 60 * 1000;

/// Handle Gemini usage-limit and quota events.
///
/// Similar to [`HandleClaudeCodeLimits`], this workflow does not attempt
/// automated account rotation. It:
///   1. Guards against unsafe pane states.
///   2. Applies a cooldown to avoid spamming on repeated events.
///   3. Classifies the quota type (warning vs reached).
///   4. Persists an audit record and produces a recovery plan.
pub struct HandleGeminiQuota {
    cooldown_ms: i64,
}

impl HandleGeminiQuota {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cooldown_ms: GEMINI_QUOTA_COOLDOWN_MS,
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn with_cooldown_ms(cooldown_ms: i64) -> Self {
        Self { cooldown_ms }
    }

    fn classify_quota(trigger: &serde_json::Value) -> (&'static str, Option<String>) {
        let event_type = trigger
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let extracted = trigger.get("extracted");

        let remaining = extracted
            .and_then(|e| e.get("remaining"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string);

        let quota_type = match event_type {
            "usage.warning" => "quota_warning",
            "usage.reached" => "quota_reached",
            _ => "unknown_quota",
        };

        (quota_type, remaining)
    }

    fn build_recovery_plan(
        quota_type: &str,
        remaining_pct: Option<&str>,
        pane_id: u64,
    ) -> serde_json::Value {
        let next_steps = match quota_type {
            "quota_warning" => vec![
                "Save current work and commit progress",
                "Consider switching to a non-Pro model if available",
                "Check quota reset time in Google AI Studio",
            ],
            "quota_reached" => vec![
                "Gemini Pro models quota is exhausted",
                "Do not send further input to avoid wasted requests",
                "Switch to a non-Pro model or wait for quota reset",
                "Or start a new session with a different Google account",
            ],
            _ => vec!["Unknown quota type; check pane output for details"],
        };

        serde_json::json!({
            "quota_type": quota_type,
            "pane_id": pane_id,
            "remaining_pct": remaining_pct,
            "next_steps": next_steps,
            "safe_to_send": quota_type == "quota_warning",
        })
    }
}

impl Default for HandleGeminiQuota {
    fn default() -> Self {
        Self::new()
    }
}

impl Workflow for HandleGeminiQuota {
    fn name(&self) -> &'static str {
        "handle_gemini_quota"
    }

    fn description(&self) -> &'static str {
        "Safe-pause on Gemini quota/usage limits with recovery plan"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.agent_type == crate::patterns::AgentType::Gemini
            && matches!(
                detection.event_type.as_str(),
                "usage.warning" | "usage.reached"
            )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["usage.warning", "usage.reached"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["gemini"]
    }

    fn requires_pane(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn is_destructive(&self) -> bool {
        false
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new(
                "check_cooldown",
                "Skip if quota event was recently handled for this pane",
            ),
            WorkflowStep::new(
                "classify_and_record",
                "Classify quota type, record audit event, and build recovery plan",
            ),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let trigger = ctx.trigger().cloned().unwrap_or(serde_json::Value::Null);
        let execution_id = ctx.execution_id().to_string();
        let cooldown_ms = self.cooldown_ms;
        let caps = ctx.capabilities().clone();

        Box::pin(async move {
            match step_idx {
                0 => {
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running in pane");
                    }
                    tracing::debug!(pane_id, "handle_gemini_quota: guard checks passed");
                    StepResult::cont()
                }

                1 => {
                    let since = now_ms() - cooldown_ms;
                    let query = crate::storage::AuditQuery {
                        pane_id: Some(pane_id),
                        action_kind: Some("gemini_quota_limit".to_string()),
                        since: Some(since),
                        limit: Some(1),
                        ..Default::default()
                    };

                    match storage.get_audit_actions(query).await {
                        Ok(recent) if !recent.is_empty() => {
                            tracing::info!(
                                pane_id,
                                last_quota_ts = recent[0].ts,
                                "handle_gemini_quota: within cooldown, skipping"
                            );
                            StepResult::done(serde_json::json!({
                                "status": "cooldown_skipped",
                                "pane_id": pane_id,
                                "last_quota_ts": recent[0].ts,
                            }))
                        }
                        Ok(_) => {
                            tracing::debug!(
                                pane_id,
                                "handle_gemini_quota: no recent quota events, proceeding"
                            );
                            StepResult::cont()
                        }
                        Err(e) => {
                            tracing::warn!(
                                pane_id,
                                error = %e,
                                "handle_gemini_quota: cooldown check failed, proceeding"
                            );
                            StepResult::cont()
                        }
                    }
                }

                2 => {
                    let (quota_type, remaining_pct) = Self::classify_quota(&trigger);
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("gemini");
                    let rule_id = trigger
                        .get("rule_id")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);

                    let plan =
                        Self::build_recovery_plan(quota_type, remaining_pct.as_deref(), pane_id);

                    let audit = crate::storage::AuditActionRecord {
                        id: 0,
                        ts: now_ms(),
                        actor_kind: "workflow".to_string(),
                        actor_id: Some(execution_id.clone()),
                        correlation_id: None,
                        pane_id: Some(pane_id),
                        domain: None,
                        action_kind: "gemini_quota_limit".to_string(),
                        policy_decision: "allow".to_string(),
                        decision_reason: None,
                        rule_id,
                        input_summary: Some(format!("Gemini {quota_type} on pane {pane_id}")),
                        verification_summary: None,
                        decision_context: Some(serde_json::to_string(&plan).unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "quota audit plan serialization failed");
                            String::new()
                        })),
                        result: "recorded".to_string(),
                    };

                    match storage.record_audit_action(audit).await {
                        Ok(audit_id) => {
                            tracing::info!(
                                pane_id,
                                audit_id,
                                quota_type,
                                remaining_pct = ?remaining_pct,
                                "handle_gemini_quota: recorded quota event"
                            );

                            StepResult::done(serde_json::json!({
                                "status": "recorded",
                                "pane_id": pane_id,
                                "agent_type": agent_type,
                                "quota_type": quota_type,
                                "remaining_pct": remaining_pct,
                                "recovery_plan": plan,
                                "audit_id": audit_id,
                            }))
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_gemini_quota: failed to record quota event"
                            );
                            StepResult::abort(format!("Failed to record quota event: {e}"))
                        }
                    }
                }

                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

// ============================================================================
// Device Auth Workflow Step (wa-nu4.1.3.6)
// ============================================================================
//
// Integrates browser-based OpenAI device auth into the usage-limit failover
// workflow. This step:
//   1. Validates the device code format
//   2. Initializes a BrowserContext
//   3. Runs the OpenAiDeviceAuthFlow via Playwright
//   4. Returns a structured result mapping to workflow step outcomes

/// Result of executing the device auth workflow step.
///
/// Maps browser automation outcomes to workflow-level concepts.
#[cfg(feature = "browser")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status")]
pub enum DeviceAuthStepOutcome {
    /// Device auth completed successfully; profile is now authenticated.
    #[serde(rename = "authenticated")]
    Authenticated {
        /// Wall-clock time the flow took (ms).
        elapsed_ms: u64,
        /// Account used for auth.
        account: String,
    },

    /// Interactive bootstrap is required (password/MFA).
    ///
    /// The workflow should transition to the safe fallback path
    /// (wa-nu4.1.3.8) rather than retrying.
    #[serde(rename = "bootstrap_required")]
    BootstrapRequired {
        /// Why interactive login is needed.
        reason: String,
        /// Account that needs bootstrap.
        account: String,
        /// Path to failure artifacts, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        artifacts_dir: Option<std::path::PathBuf>,
    },

    /// Auth step failed with an error.
    #[serde(rename = "failed")]
    Failed {
        /// Human-readable error description.
        error: String,
        /// Error classification for programmatic handling.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
        /// Path to failure artifacts, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        artifacts_dir: Option<std::path::PathBuf>,
    },
}

/// Execute the Playwright-based device auth flow as a workflow step.
///
/// This function:
/// 1. Validates the device code format (returns early on invalid codes)
/// 2. Initializes a BrowserContext from the given data directory
/// 3. Runs OpenAiDeviceAuthFlow with the persistent browser profile
/// 4. Maps the result to a [`DeviceAuthStepOutcome`]
///
/// # Arguments
///
/// * `device_code` - The device code from the Codex pane (e.g., "ABCD-EFGH").
/// * `account` - Account identifier for profile selection (e.g., "default").
/// * `data_dir` - Data directory containing browser profiles (typically `.ft/`).
/// * `artifacts_dir` - Optional directory for failure artifacts (screenshots, etc.).
/// * `headless` - Whether to run the browser in headless mode.
///
/// # Safety
///
/// - The device code is validated but **never logged** (secret material).
/// - Only the code format is checked, not the code content.
/// - Failure artifacts contain redacted DOM, never session tokens.
#[cfg(feature = "browser")]
pub fn execute_device_auth_step(
    device_code: &str,
    account: &str,
    data_dir: &std::path::Path,
    artifacts_dir: Option<&std::path::Path>,
    headless: bool,
) -> DeviceAuthStepOutcome {
    use crate::browser::openai_device::{AuthFlowResult, OpenAiDeviceAuthFlow};
    use crate::browser::{BrowserConfig, BrowserContext};

    // Step 1: Validate device code format before touching the browser
    if !validate_device_code(device_code) {
        return DeviceAuthStepOutcome::Failed {
            error: "Invalid device code format".into(),
            error_kind: Some("invalid_code".into()),
            artifacts_dir: None,
        };
    }

    // Step 2: Initialize browser context
    let config = BrowserConfig {
        headless,
        ..Default::default()
    };
    let mut ctx = BrowserContext::new(config, data_dir);

    if let Err(e) = ctx.ensure_ready() {
        return DeviceAuthStepOutcome::Failed {
            error: format!("Browser initialization failed: {e}"),
            error_kind: Some("browser_not_ready".into()),
            artifacts_dir: None,
        };
    }

    // Step 3: Run the device auth flow
    let mut flow = OpenAiDeviceAuthFlow::with_defaults();
    if let Some(dir) = artifacts_dir {
        flow = flow.with_artifacts(dir);
    }

    let result = flow.execute(&ctx, device_code, account, None);

    // Step 4: Map browser result to workflow outcome
    match result {
        AuthFlowResult::Success { elapsed_ms } => {
            tracing::info!(
                account = %account,
                elapsed_ms,
                "Device auth step: success"
            );
            // NOTE: device_code intentionally NOT logged
            DeviceAuthStepOutcome::Authenticated {
                elapsed_ms,
                account: account.to_string(),
            }
        }
        AuthFlowResult::InteractiveBootstrapRequired {
            reason,
            artifacts_dir: art_dir,
        } => {
            tracing::warn!(
                account = %account,
                reason = %reason,
                "Device auth step: interactive bootstrap required"
            );
            DeviceAuthStepOutcome::BootstrapRequired {
                reason,
                account: account.to_string(),
                artifacts_dir: art_dir,
            }
        }
        AuthFlowResult::Failed {
            error,
            kind,
            artifacts_dir: art_dir,
        } => {
            tracing::error!(
                account = %account,
                error = %error,
                kind = ?kind,
                "Device auth step: failed"
            );
            DeviceAuthStepOutcome::Failed {
                error,
                error_kind: Some(format!("{kind:?}")),
                artifacts_dir: art_dir,
            }
        }
    }
}

/// Convert a [`DeviceAuthStepOutcome`] to a [`StepResult`] for workflow integration.
///
/// Mapping:
/// - `Authenticated` → `StepResult::Continue` (proceed to resume step)
/// - `BootstrapRequired` → `StepResult::Abort` (enter fallback path)
/// - `Failed` → `StepResult::Abort` with error details
#[cfg(feature = "browser")]
pub fn device_auth_outcome_to_step_result(outcome: &DeviceAuthStepOutcome) -> StepResult {
    match outcome {
        DeviceAuthStepOutcome::Authenticated { .. } => {
            let json = serde_json::to_value(outcome).unwrap_or_default();
            StepResult::Done { result: json }
        }
        DeviceAuthStepOutcome::BootstrapRequired {
            reason, account, ..
        } => StepResult::Abort {
            reason: format!("Interactive bootstrap required for account '{account}': {reason}"),
        },
        DeviceAuthStepOutcome::Failed { error, .. } => StepResult::Abort {
            reason: format!("Device auth failed: {error}"),
        },
    }
}

// ============================================================================
// Resume Session Step (wa-nu4.1.3.7)
// ============================================================================
//
// After device auth completes, resume the Codex session with the saved
// session ID and send "proceed." to continue. Verifies the session is
// ready by waiting for a stable output marker.

/// Configuration for the resume session step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeSessionConfig {
    /// Template for the resume command. `{session_id}` is replaced at runtime.
    pub resume_command_template: String,
    /// Text to send after session resumes (triggers continuation).
    pub proceed_text: String,
    /// Wait for pane output to stabilize after sending the resume command (ms).
    pub post_resume_stable_ms: u64,
    /// Wait for pane output to stabilize after sending proceed (ms).
    pub post_proceed_stable_ms: u64,
    /// Maximum wait time for the resume command to take effect (ms).
    pub resume_timeout_ms: u64,
    /// Maximum wait time for the proceed signal to be acknowledged (ms).
    pub proceed_timeout_ms: u64,
}

impl Default for ResumeSessionConfig {
    fn default() -> Self {
        Self {
            resume_command_template: "cod resume {session_id}\n".to_string(),
            proceed_text: "proceed.\n".to_string(),
            post_resume_stable_ms: 3_000,
            post_proceed_stable_ms: 5_000,
            resume_timeout_ms: 30_000,
            proceed_timeout_ms: 30_000,
        }
    }
}

/// Outcome of the resume session step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum ResumeSessionOutcome {
    /// Session resumed and ready for continued interaction.
    #[serde(rename = "ready")]
    Ready {
        /// Session ID that was resumed.
        session_id: String,
    },

    /// Resume command sent but could not verify ready state within timeout.
    ///
    /// This is a soft failure — the session may still be resuming.
    #[serde(rename = "timeout")]
    VerifyTimeout {
        /// Session ID attempted.
        session_id: String,
        /// Which phase timed out ("resume" or "proceed").
        phase: String,
        /// Time waited (ms).
        waited_ms: u64,
    },

    /// Session could not be resumed due to an error.
    #[serde(rename = "failed")]
    Failed {
        /// Human-readable error description.
        error: String,
    },
}

/// Format the resume command for a given session ID.
///
/// Replaces `{session_id}` in the template with the actual session ID.
///
/// # Panics
///
/// None — if `{session_id}` is not in the template, the template is returned as-is.
#[allow(clippy::literal_string_with_formatting_args)]
const SESSION_ID_TOKEN: &str = "{session_id}";

#[must_use]
pub fn format_resume_command(session_id: &str, config: &ResumeSessionConfig) -> String {
    config
        .resume_command_template
        .replace(SESSION_ID_TOKEN, session_id)
}

/// Validate a session ID for resume.
///
/// Session IDs are hex UUIDs (e.g., "a1b2c3d4-e5f6-7890-abcd-ef1234567890").
/// Returns true if the ID has at least 8 hex characters separated by hyphens.
#[must_use]
pub fn validate_session_id(session_id: &str) -> bool {
    let trimmed = session_id.trim();
    if trimmed.len() < 8 {
        return false;
    }
    // Must contain only hex chars and hyphens
    trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Build the resume StepResult (sends resume command, waits for stable tail).
///
/// This returns a `StepResult::SendText` with a `StableTail` wait condition
/// so the workflow runner handles the policy-gated injection and wait.
#[must_use]
pub fn build_resume_step_result(session_id: &str, config: &ResumeSessionConfig) -> StepResult {
    let command = format_resume_command(session_id, config);
    StepResult::send_text_and_wait(
        command,
        WaitCondition::stable_tail(config.post_resume_stable_ms),
        config.resume_timeout_ms,
    )
}

/// Build the proceed StepResult (sends proceed text, waits for stable tail).
///
/// This returns a `StepResult::SendText` with a `StableTail` wait condition
/// so the workflow runner handles the policy-gated injection and wait.
#[must_use]
pub fn build_proceed_step_result(config: &ResumeSessionConfig) -> StepResult {
    StepResult::send_text_and_wait(
        config.proceed_text.clone(),
        WaitCondition::stable_tail(config.post_proceed_stable_ms),
        config.proceed_timeout_ms,
    )
}

/// Convert a [`ResumeSessionOutcome`] to a [`StepResult`] for workflow integration.
///
/// Mapping:
/// - `Ready` → `StepResult::Done` (workflow complete)
/// - `VerifyTimeout` → `StepResult::Done` with timeout info (non-fatal)
/// - `Failed` → `StepResult::Abort`
#[must_use]
pub fn resume_outcome_to_step_result(outcome: &ResumeSessionOutcome) -> StepResult {
    match outcome {
        ResumeSessionOutcome::Ready { .. } => {
            let json = serde_json::to_value(outcome).unwrap_or_default();
            StepResult::Done { result: json }
        }
        ResumeSessionOutcome::VerifyTimeout { .. } => {
            // Timeouts are soft failures — report but don't abort.
            // The session may still be resuming; let the caller decide.
            let json = serde_json::to_value(outcome).unwrap_or_default();
            StepResult::Done { result: json }
        }
        ResumeSessionOutcome::Failed { error } => StepResult::Abort {
            reason: format!("Resume session failed: {error}"),
        },
    }
}

// ============================================================================
// Safe Fallback Path (wa-nu4.1.3.8)
// ============================================================================

/// Why the safe fallback path was entered.
///
/// Captures the blocking condition that prevents full automated failover.
/// All variants carry enough context to build a structured next-step plan
/// without exposing secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FallbackReason {
    /// Browser auth returned NeedsHuman (password, MFA, SSO).
    #[serde(rename = "needs_human_auth")]
    NeedsHumanAuth {
        /// Which account triggered the interactive requirement.
        account: String,
        /// Human-readable explanation (already redacted by caller).
        detail: String,
    },
    /// Failover is disabled in configuration.
    #[serde(rename = "failover_disabled")]
    FailoverDisabled,
    /// A required external tool is missing (caut, Playwright, etc.).
    #[serde(rename = "tool_missing")]
    ToolMissing {
        /// Name of the missing tool.
        tool: String,
    },
    /// Policy denied the injection (alt-screen, recent gap, unknown state).
    #[serde(rename = "policy_denied")]
    PolicyDenied {
        /// Policy rule that denied.
        rule: String,
    },
    /// All configured accounts have reached usage limits.
    #[serde(rename = "all_accounts_exhausted")]
    AllAccountsExhausted {
        /// Number of accounts checked.
        accounts_checked: u32,
    },
    /// A catch-all for unexpected blocking conditions.
    #[serde(rename = "other")]
    Other {
        /// Human-readable description.
        detail: String,
    },
}

impl std::fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsHumanAuth { account, detail } => {
                write!(
                    f,
                    "Interactive auth required for account {account}: {detail}"
                )
            }
            Self::FailoverDisabled => write!(f, "Account failover is disabled in configuration"),
            Self::ToolMissing { tool } => write!(f, "Required tool not found: {tool}"),
            Self::PolicyDenied { rule } => write!(f, "Policy denied injection: {rule}"),
            Self::AllAccountsExhausted { accounts_checked } => {
                write!(
                    f,
                    "All {accounts_checked} configured account(s) at usage limit"
                )
            }
            Self::Other { detail } => write!(f, "{detail}"),
        }
    }
}

/// A structured next-step plan persisted when the safe fallback path activates.
///
/// This plan enables both human operators and downstream agents to understand
/// what happened and how to recover, without leaking secrets.
///
/// # Redaction
///
/// The caller is responsible for passing already-redacted values for any field
/// that might contain sensitive data (session IDs are opaque hashes, not tokens).
/// Use [`crate::policy::Redactor`] before constructing this struct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackNextStepPlan {
    /// Schema version for forward compatibility.
    pub version: u32,

    /// Why the fallback was entered.
    pub reason: FallbackReason,

    /// Pane ID where the usage-limit event was detected.
    pub pane_id: u64,

    /// Explicit steps the operator must take to recover.
    ///
    /// Each entry is a human-readable instruction, e.g.:
    /// - "Run `ft auth bootstrap --account openai-team` in a terminal"
    /// - "Wait for usage-limit reset (estimated 2024-03-15T12:00:00Z)"
    pub operator_steps: Vec<String>,

    /// When it is safe to retry automated failover (epoch ms), if known.
    ///
    /// Derived from the reset time parsed from the usage-limit transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<i64>,

    /// Resume session ID, if available (opaque identifier, not a secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_session_id: Option<String>,

    /// Non-secret account identifier that was in use when the limit was hit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,

    /// Suggested CLI commands the operator can run to resume or inspect.
    ///
    /// e.g., `["ft auth bootstrap --account openai-team", "ft events --pane 42"]`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_commands: Vec<String>,

    /// Timestamp when the plan was created (epoch ms).
    pub created_at_ms: i64,
}

impl FallbackNextStepPlan {
    /// Current schema version.
    pub const CURRENT_VERSION: u32 = 1;
}

/// Build a [`FallbackNextStepPlan`] for the "needs human auth" scenario.
///
/// This is the most common fallback: device auth returned `BootstrapRequired`
/// because password/MFA/SSO is needed.
#[must_use]
pub fn build_needs_human_auth_plan(
    pane_id: u64,
    account: &str,
    detail: &str,
    resume_session_id: Option<&str>,
    retry_after_ms: Option<i64>,
    now_ms: i64,
) -> FallbackNextStepPlan {
    let mut operator_steps = vec![format!(
        "Run `ft auth bootstrap --account {account}` to complete interactive login"
    )];

    if let Some(session_id) = resume_session_id {
        operator_steps.push(format!("After auth, resume with: cod resume {session_id}"));
    }

    if retry_after_ms.is_some() {
        operator_steps.push(
            "Alternatively, wait for the usage-limit reset and retry automatically".to_string(),
        );
    }

    let mut suggested_commands = vec![format!("ft auth bootstrap --account {account}")];
    suggested_commands.push(format!("ft events --pane {pane_id}"));

    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::NeedsHumanAuth {
            account: account.to_string(),
            detail: detail.to_string(),
        },
        pane_id,
        operator_steps,
        retry_after_ms,
        resume_session_id: resume_session_id.map(ToString::to_string),
        account_id: Some(account.to_string()),
        suggested_commands,
        created_at_ms: now_ms,
    }
}

/// Build a [`FallbackNextStepPlan`] for the "failover disabled" scenario.
#[must_use]
pub fn build_failover_disabled_plan(
    pane_id: u64,
    resume_session_id: Option<&str>,
    retry_after_ms: Option<i64>,
    now_ms: i64,
) -> FallbackNextStepPlan {
    let mut operator_steps = vec![
        "Account failover is disabled. Enable it in ft config or handle manually.".to_string(),
    ];

    if let Some(session_id) = resume_session_id {
        operator_steps.push(format!("Resume manually with: cod resume {session_id}"));
    }

    if retry_after_ms.is_some() {
        operator_steps
            .push("Wait for the usage-limit reset time, then the session can retry.".to_string());
    }

    let mut suggested_commands = vec![format!("ft events --pane {pane_id}")];
    suggested_commands.push("ft config show".to_string());

    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::FailoverDisabled,
        pane_id,
        operator_steps,
        retry_after_ms,
        resume_session_id: resume_session_id.map(ToString::to_string),
        account_id: None,
        suggested_commands,
        created_at_ms: now_ms,
    }
}

/// Build a [`FallbackNextStepPlan`] for the "tool missing" scenario.
#[must_use]
pub fn build_tool_missing_plan(pane_id: u64, tool: &str, now_ms: i64) -> FallbackNextStepPlan {
    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::ToolMissing {
            tool: tool.to_string(),
        },
        pane_id,
        operator_steps: vec![
            format!("Install the required tool: {tool}"),
            "Re-run the workflow after installation.".to_string(),
        ],
        retry_after_ms: None,
        resume_session_id: None,
        account_id: None,
        suggested_commands: vec![format!("ft events --pane {pane_id}")],
        created_at_ms: now_ms,
    }
}

/// Build a [`FallbackNextStepPlan`] for the "all accounts exhausted" scenario.
#[must_use]
pub fn build_all_accounts_exhausted_plan(
    pane_id: u64,
    accounts_checked: u32,
    resume_session_id: Option<&str>,
    retry_after_ms: Option<i64>,
    now_ms: i64,
) -> FallbackNextStepPlan {
    let mut operator_steps = vec![format!(
        "All {accounts_checked} configured account(s) are at their usage limit."
    )];

    if retry_after_ms.is_some() {
        operator_steps.push(
            "Wait for usage-limit reset, then failover will retry automatically.".to_string(),
        );
    } else {
        operator_steps.push(
            "Check account limits with `ft accounts status` and add or rotate accounts."
                .to_string(),
        );
    }

    let mut suggested_commands = vec![
        "ft accounts status".to_string(),
        format!("ft events --pane {pane_id}"),
    ];

    if let Some(session_id) = resume_session_id {
        suggested_commands.push(format!("cod resume {session_id}"));
    }

    FallbackNextStepPlan {
        version: FallbackNextStepPlan::CURRENT_VERSION,
        reason: FallbackReason::AllAccountsExhausted { accounts_checked },
        pane_id,
        operator_steps,
        retry_after_ms,
        resume_session_id: resume_session_id.map(ToString::to_string),
        account_id: None,
        suggested_commands,
        created_at_ms: now_ms,
    }
}

/// Convert a [`FallbackNextStepPlan`] to a [`StepResult`] for workflow integration.
///
/// The plan is serialized into the `Done` result so it persists in step logs.
/// The workflow marks the originating event as **paused** (not failed), signalling
/// that automation stopped intentionally and recovery is documented.
#[must_use]
pub fn fallback_plan_to_step_result(plan: &FallbackNextStepPlan) -> StepResult {
    let mut result = serde_json::to_value(plan).unwrap_or_default();
    // Tag the result so downstream consumers can distinguish fallback from success.
    if let serde_json::Value::Object(ref mut map) = result {
        map.insert("fallback".to_string(), serde_json::Value::Bool(true));
    }
    StepResult::Done { result }
}

/// The handled-status string used when an event enters the safe fallback path.
///
/// Events marked with this status are excluded from `--unhandled` queries
/// (because `handled_at` is set) but carry distinct semantics from "completed".
pub const FALLBACK_HANDLED_STATUS: &str = "paused";

/// Check whether a [`StepResult::Done`] result represents a fallback plan
/// (as opposed to a normal successful completion).
#[must_use]
pub fn is_fallback_result(result: &StepResult) -> bool {
    match result {
        StepResult::Done { result } => result
            .as_object()
            .and_then(|m| m.get("fallback"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Detection, PatternEngine, Severity};
    use crate::runtime_compat::CompatRuntime;

    // ========================================================================
    // StepResult Tests
    // ========================================================================

    #[test]
    fn step_result_continue_serializes() {
        let result = StepResult::Continue;
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("continue"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_continue());
    }

    #[test]
    fn step_result_done_serializes() {
        let result = StepResult::done(serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("done"));
        assert!(json.contains("status"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_done());
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_retry_serializes() {
        let result = StepResult::retry(5000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("retry"));
        assert!(json.contains("5000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::Retry { delay_ms } => assert_eq!(delay_ms, 5000),
            _ => panic!("Expected Retry"),
        }
    }

    #[test]
    fn step_result_abort_serializes() {
        let result = StepResult::abort("test failure");
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("abort"));
        assert!(json.contains("test failure"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_wait_for_serializes() {
        let result =
            StepResult::wait_for_with_timeout(WaitCondition::pattern("prompt.ready"), 10_000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("wait_for"));
        assert!(json.contains("prompt.ready"));
        assert!(json.contains("10000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::WaitFor {
                condition,
                timeout_ms,
            } => {
                assert_eq!(timeout_ms, Some(10_000));
                match condition {
                    WaitCondition::Pattern { rule_id, .. } => assert_eq!(rule_id, "prompt.ready"),
                    _ => panic!("Expected Pattern condition"),
                }
            }
            _ => panic!("Expected WaitFor"),
        }
    }

    #[test]
    fn step_result_helper_methods() {
        assert!(StepResult::cont().is_continue());
        assert!(StepResult::done_empty().is_done());
        assert!(StepResult::done_empty().is_terminal());
        assert!(StepResult::abort("error").is_terminal());
        assert!(!StepResult::retry(100).is_terminal());
        assert!(!StepResult::wait_for(WaitCondition::external("key")).is_terminal());
    }

    // ========================================================================
    // WaitCondition Tests
    // ========================================================================

    #[test]
    fn wait_condition_pattern_serializes() {
        let cond = WaitCondition::pattern("test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pattern"));
        assert!(json.contains("test.rule"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_pattern_on_pane_serializes() {
        let cond = WaitCondition::pattern_on_pane(42, "test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("42"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_idle_serializes() {
        let cond = WaitCondition::pane_idle(1000);
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pane_idle"));
        assert!(json.contains("1000"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
    }

    #[test]
    fn wait_condition_pane_idle_on_serializes() {
        let cond = WaitCondition::pane_idle_on(99, 500);
        assert_eq!(cond.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_external_serializes() {
        let cond = WaitCondition::external("approval_granted");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("external"));
        assert!(json.contains("approval_granted"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_text_match_serializes() {
        let cond = WaitCondition::text_match(TextMatch::substring("ready"));
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("text_match"));
        assert!(json.contains("substring"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_sleep_serializes() {
        let cond = WaitCondition::sleep(1500);
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("sleep"));
        assert!(json.contains("1500"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    // ========================================================================
    // WorkflowStep Tests
    // ========================================================================

    #[test]
    fn workflow_step_creates() {
        let step = WorkflowStep::new("send_prompt", "Send a prompt to the terminal");
        assert_eq!(step.name, "send_prompt");
        assert_eq!(step.description, "Send a prompt to the terminal");
    }

    // ========================================================================
    // WorkflowConfig Tests
    // ========================================================================

    #[test]
    fn workflow_config_defaults() {
        let config = WorkflowConfig::default();
        assert_eq!(config.default_wait_timeout_ms, 30_000);
        assert_eq!(config.max_step_retries, 3);
        assert_eq!(config.retry_delay_ms, 1_000);
    }

    // ========================================================================
    // WorkflowEngine Tests
    // ========================================================================

    #[test]
    fn engine_can_be_created() {
        let engine = WorkflowEngine::new(5);
        assert_eq!(engine.max_concurrent(), 5);
    }

    // ========================================================================
    // Stub Workflow Tests (wa-nu4.1.1.1 acceptance criteria)
    // ========================================================================

    /// A stub workflow for testing that demonstrates all workflow capabilities
    struct StubWorkflow {
        name: &'static str,
        description: &'static str,
        target_rule_prefix: &'static str,
    }

    impl StubWorkflow {
        fn new() -> Self {
            Self {
                name: "stub_workflow",
                description: "A test workflow for verification",
                target_rule_prefix: "test.",
            }
        }
    }

    impl Workflow for StubWorkflow {
        fn name(&self) -> &'static str {
            self.name
        }

        fn description(&self) -> &'static str {
            self.description
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with(self.target_rule_prefix)
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_one", "First step - sends prompt"),
                WorkflowStep::new("step_two", "Second step - waits for response"),
                WorkflowStep::new("step_three", "Third step - completes"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::cont(),
                    1 => StepResult::wait_for(WaitCondition::pattern("response.ready")),
                    2 => StepResult::done(serde_json::json!({"completed": true})),
                    _ => StepResult::abort("unexpected step index"),
                }
            })
        }

        fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
            Box::pin(async {
                // Stub cleanup - no-op
            })
        }
    }

    fn make_test_detection(rule_id: &str) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type: AgentType::Wezterm,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        }
    }

    #[test]
    fn stub_workflow_compiles_and_has_correct_metadata() {
        let workflow = StubWorkflow::new();

        assert_eq!(workflow.name(), "stub_workflow");
        assert_eq!(workflow.description(), "A test workflow for verification");
        assert_eq!(workflow.step_count(), 3);

        let steps = workflow.steps();
        assert_eq!(steps[0].name, "step_one");
        assert_eq!(steps[1].name, "step_two");
        assert_eq!(steps[2].name, "step_three");
    }

    #[test]
    fn stub_workflow_handles_matching_detections() {
        let workflow = StubWorkflow::new();

        // Should handle detections with matching prefix
        assert!(workflow.handles(&make_test_detection("test.prompt_ready")));
        assert!(workflow.handles(&make_test_detection("test.anything")));

        // Should not handle detections with non-matching prefix
        assert!(!workflow.handles(&make_test_detection("other.prompt_ready")));
        assert!(!workflow.handles(&make_test_detection("production.event")));
    }

    #[tokio::test]
    async fn stub_workflow_executes_steps_correctly() {
        let workflow = StubWorkflow::new();

        // Create a minimal context for testing
        // Note: In real usage, this would have an actual StorageHandle
        // For this test, we just verify the step execution logic

        // We can't easily create a WorkflowContext without a real StorageHandle,
        // but we can verify the workflow's step logic independently
        let steps = workflow.steps();
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn step_result_transitions_exhaustive() {
        // Verify all StepResult variants can be created and identified
        let variants = [
            StepResult::Continue,
            StepResult::Done {
                result: serde_json::Value::Null,
            },
            StepResult::Retry { delay_ms: 1000 },
            StepResult::Abort {
                reason: "test".to_string(),
            },
            StepResult::WaitFor {
                condition: WaitCondition::external("key"),
                timeout_ms: None,
            },
        ];

        // Each variant serializes uniquely
        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        // All 5 variants have unique type identifiers
        assert_eq!(json_types.len(), 5);
        assert!(json_types.contains("continue"));
        assert!(json_types.contains("done"));
        assert!(json_types.contains("retry"));
        assert!(json_types.contains("abort"));
        assert!(json_types.contains("wait_for"));
    }

    #[test]
    fn wait_condition_transitions_exhaustive() {
        // Verify all WaitCondition variants
        let variants = [
            WaitCondition::Pattern {
                pane_id: None,
                rule_id: "test".to_string(),
            },
            WaitCondition::PaneIdle {
                pane_id: None,
                idle_threshold_ms: 1000,
            },
            WaitCondition::External {
                key: "test".to_string(),
            },
        ];

        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        assert_eq!(json_types.len(), 3);
        assert!(json_types.contains("pattern"));
        assert!(json_types.contains("pane_idle"));
        assert!(json_types.contains("external"));
    }

    // ========================================================================
    // WaitConditionResult Tests
    // ========================================================================

    #[test]
    fn wait_condition_result_satisfied_is_satisfied() {
        let result = WaitConditionResult::Satisfied {
            elapsed_ms: 100,
            polls: 5,
            context: Some("matched".to_string()),
        };
        assert!(result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(100));
    }

    #[test]
    fn wait_condition_result_timed_out_is_timed_out() {
        let result = WaitConditionResult::TimedOut {
            elapsed_ms: 5000,
            polls: 100,
            last_observed: Some("waiting for prompt".to_string()),
        };
        assert!(!result.is_satisfied());
        assert!(result.is_timed_out());
        assert_eq!(result.elapsed_ms(), Some(5000));
    }

    #[test]
    fn wait_condition_result_unsupported_has_no_elapsed() {
        let result = WaitConditionResult::Unsupported {
            reason: "external signals not implemented".to_string(),
        };
        assert!(!result.is_satisfied());
        assert!(!result.is_timed_out());
        assert_eq!(result.elapsed_ms(), None);
    }

    // ========================================================================
    // WaitConditionOptions Tests
    // ========================================================================

    #[test]
    fn wait_condition_options_defaults() {
        let options = WaitConditionOptions::default();
        assert_eq!(options.tail_lines, 200);
        assert_eq!(options.poll_initial.as_millis(), 50);
        assert_eq!(options.poll_max.as_millis(), 1000);
        assert_eq!(options.max_polls, 10_000);
        assert!(options.allow_idle_heuristics);
    }

    // ========================================================================
    // Helper Function Tests
    // ========================================================================

    #[test]
    fn tail_text_extracts_last_n_lines() {
        let text = "line1\nline2\nline3\nline4\nline5";
        assert_eq!(tail_text(text, 3), "line3\nline4\nline5");
        assert_eq!(tail_text(text, 1), "line5");
        assert_eq!(tail_text(text, 10), text);
        assert_eq!(tail_text(text, 0), "");
    }

    #[test]
    fn tail_text_handles_empty_input() {
        assert_eq!(tail_text("", 5), "");
    }

    #[test]
    fn tail_text_handles_single_line() {
        assert_eq!(tail_text("single line", 5), "single line");
    }

    #[test]
    fn truncate_for_log_preserves_short_strings() {
        assert_eq!(truncate_for_log("hello", 10), "hello");
        assert_eq!(truncate_for_log("exact", 5), "exact");
    }

    #[test]
    fn truncate_for_log_truncates_long_strings() {
        assert_eq!(truncate_for_log("hello world", 8), "hello...");
    }

    // ========================================================================
    // Heuristic Idle Check Tests
    // ========================================================================

    #[test]
    fn heuristic_idle_detects_bash_prompt() {
        let text = "output from command\nuser@host:~$ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_root_prompt() {
        let text = "output\nroot@host:~# ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_zsh_prompt() {
        let text = "output\n❯ ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_python_repl() {
        let text = ">>> ";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_detects_prompt_with_trailing_newline() {
        // Note: Rust's lines() iterator doesn't include trailing empty lines,
        // so "user@host:~$ \n" becomes the last line as "user@host:~$ "
        // which after trim_end becomes "user@host:~$" ending with "$"
        let text = "output\nuser@host:~$ \n";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(is_idle);
        assert!(desc.contains("ends_with_prompt"));
    }

    #[test]
    fn heuristic_idle_rejects_command_output() {
        let text = "building project...\nCompiling foo v1.0.0";
        let (is_idle, desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle);
        assert!(desc.contains("no_prompt_detected"));
    }

    #[test]
    fn heuristic_idle_rejects_running_command() {
        // Use "50/100" instead of "50%" - the % character would match the tcsh prompt pattern
        let text = "npm run build\nProgress: 50/100";
        let (is_idle, _desc) = heuristic_idle_check(text, 10);
        assert!(!is_idle);
    }

    // ========================================================================
    // WaitConditionExecutor Tests (using mock source)
    // ========================================================================

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock pane text source for testing
    struct MockPaneSource {
        texts: Mutex<Vec<String>>,
        call_count: AtomicUsize,
    }

    impl MockPaneSource {
        fn new(texts: Vec<String>) -> Self {
            Self {
                texts: Mutex::new(texts),
                call_count: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    impl crate::wezterm::PaneTextSource for MockPaneSource {
        type Fut<'a> =
            std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let count = self.call_count.fetch_add(1, Ordering::Relaxed);
            let texts = self.texts.lock().unwrap();
            let text = if count < texts.len() {
                texts[count].clone()
            } else {
                texts.last().cloned().unwrap_or_default()
            };
            Box::pin(async move { Ok(text) })
        }
    }

    #[derive(Default)]
    struct MockWezterm;

    impl MockWezterm {
        fn pane_info(pane_id: u64) -> crate::wezterm::PaneInfo {
            crate::wezterm::PaneInfo {
                pane_id,
                tab_id: 1,
                window_id: 1,
                domain_id: None,
                domain_name: None,
                workspace: None,
                size: None,
                rows: None,
                cols: None,
                title: None,
                cwd: None,
                tty_name: None,
                cursor_x: None,
                cursor_y: None,
                cursor_visibility: None,
                left_col: None,
                top_row: None,
                is_active: false,
                is_zoomed: false,
                extra: std::collections::HashMap::new(),
            }
        }
    }

    impl crate::wezterm::WeztermInterface for MockWezterm {
        fn list_panes(&self) -> crate::wezterm::WeztermFuture<'_, Vec<crate::wezterm::PaneInfo>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_pane(
            &self,
            pane_id: u64,
        ) -> crate::wezterm::WeztermFuture<'_, crate::wezterm::PaneInfo> {
            Box::pin(async move { Ok(Self::pane_info(pane_id)) })
        }

        fn get_text(
            &self,
            _pane_id: u64,
            _escapes: bool,
        ) -> crate::wezterm::WeztermFuture<'_, String> {
            Box::pin(async { Ok(String::new()) })
        }

        fn send_text(&self, _pane_id: u64, _text: &str) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_text_no_paste(
            &self,
            _pane_id: u64,
            _text: &str,
        ) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_text_with_options(
            &self,
            _pane_id: u64,
            _text: &str,
            _no_paste: bool,
            _no_newline: bool,
        ) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_control(
            &self,
            _pane_id: u64,
            _control_char: &str,
        ) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_ctrl_c(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn send_ctrl_d(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn spawn(
            &self,
            _cwd: Option<&str>,
            _domain_name: Option<&str>,
        ) -> crate::wezterm::WeztermFuture<'_, u64> {
            Box::pin(async { Ok(1) })
        }

        fn split_pane(
            &self,
            _pane_id: u64,
            _direction: crate::wezterm::SplitDirection,
            _cwd: Option<&str>,
            _percent: Option<u8>,
        ) -> crate::wezterm::WeztermFuture<'_, u64> {
            Box::pin(async { Ok(2) })
        }

        fn activate_pane(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn get_pane_direction(
            &self,
            _pane_id: u64,
            _direction: crate::wezterm::MoveDirection,
        ) -> crate::wezterm::WeztermFuture<'_, Option<u64>> {
            Box::pin(async { Ok(None) })
        }

        fn kill_pane(&self, _pane_id: u64) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn zoom_pane(&self, _pane_id: u64, _zoom: bool) -> crate::wezterm::WeztermFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }

        fn circuit_status(&self) -> crate::circuit_breaker::CircuitBreakerStatus {
            crate::circuit_breaker::CircuitBreakerStatus::default()
        }
    }

    #[tokio::test]
    async fn pattern_wait_succeeds_on_immediate_match() {
        let source = MockPaneSource::new(vec![
            "Conversation compacted 100,000 tokens to 25,000 tokens".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(10),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert_eq!(source.calls(), 1);
    }

    #[tokio::test]
    async fn pattern_wait_times_out_on_no_match() {
        let source = MockPaneSource::new(vec!["no matching pattern here".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_millis(20))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
    }

    #[tokio::test]
    async fn pattern_wait_succeeds_after_multiple_polls() {
        let source = MockPaneSource::new(vec![
            "no match yet".to_string(),
            "still no match".to_string(),
            "Conversation compacted 100,000 tokens to 25,000 tokens".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("claude_code.compaction");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 3);
    }

    #[tokio::test]
    async fn text_match_wait_succeeds_on_substring() {
        let source = MockPaneSource::new(vec!["booting".to_string(), "ready> prompt".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::text_match(TextMatch::substring("ready>"));
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 2);
    }

    #[tokio::test]
    async fn text_match_wait_succeeds_on_regex() {
        let source = MockPaneSource::new(vec![
            "waiting".to_string(),
            "completed in 123ms".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::text_match(TextMatch::regex(r"completed in \d+ms"));
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        assert!(source.calls() >= 2);
    }

    #[tokio::test]
    async fn pane_idle_succeeds_with_osc133_prompt_active() {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["some text".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        // idle_threshold_ms = 0 means immediate satisfaction when idle
        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { context, .. } = result {
            assert!(context.unwrap().contains("osc133"));
        }
    }

    #[tokio::test]
    async fn pane_idle_times_out_with_osc133_command_running() {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["running command...".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::CommandRunning;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(20))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
    }

    // ========================================================================
    // Descriptor Workflow Tests (wa-nu4.1.1.6)
    // ========================================================================

    #[test]
    fn descriptor_yaml_parses_and_validates() {
        let yaml = r#"
workflow_schema_version: 1
name: "demo_wait"
description: "Demo workflow"
steps:
  - type: wait_for
    id: wait_prompt
    matcher:
      kind: substring
      value: "ready>"
    timeout_ms: 1000
  - type: send_text
    id: send_cmd
    text: "echo hi"
    wait_for:
      kind: regex
      pattern: "hi"
    wait_timeout_ms: 2000
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        assert_eq!(descriptor.name, "demo_wait");
        assert_eq!(descriptor.steps.len(), 2);

        let workflow = DescriptorWorkflow::new(descriptor);
        assert_eq!(workflow.steps().len(), 2);
    }

    #[test]
    fn descriptor_toml_parses_and_validates() {
        let toml = r#"
workflow_schema_version = 1
name = "demo_sleep"

[[steps]]
type = "sleep"
id = "pause"
duration_ms = 500
"#;
        let descriptor = WorkflowDescriptor::from_toml_str(toml).unwrap();
        assert_eq!(descriptor.name, "demo_sleep");
        assert_eq!(descriptor.steps.len(), 1);
    }

    #[test]
    fn descriptor_rejects_duplicate_step_ids() {
        let yaml = r#"
workflow_schema_version: 1
name: "dup_steps"
steps:
  - type: sleep
    id: step
    duration_ms: 10
  - type: sleep
    id: step
    duration_ms: 10
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Duplicate step id"));
    }

    #[test]
    fn descriptor_rejects_invalid_regex() {
        let yaml = r#"
workflow_schema_version: 1
name: "bad_regex"
steps:
  - type: wait_for
    id: wait_bad
    matcher:
      kind: regex
      pattern: "(["
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Invalid regex pattern"));
    }

    #[test]
    fn descriptor_rejects_schema_version() {
        let yaml = r#"
workflow_schema_version: 999
name: "bad_version"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("workflow_schema_version"));
    }

    #[test]
    fn descriptor_rejects_too_many_steps() {
        let mut yaml = String::from("workflow_schema_version: 1\nname: \"too_many\"\nsteps:\n");
        for idx in 0..=DESCRIPTOR_MAX_STEPS {
            yaml.push_str(&format!(
                "  - type: sleep\n    id: step_{idx}\n    duration_ms: 1\n"
            ));
        }
        let err = WorkflowDescriptor::from_yaml_str(&yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("too many steps"));
    }

    #[test]
    fn descriptor_rejects_wait_timeout_too_large() {
        let too_long = DESCRIPTOR_MAX_WAIT_TIMEOUT_MS + 1;
        let yaml = format!(
            r#"
workflow_schema_version: 1
name: "timeout_too_large"
steps:
  - type: wait_for
    id: wait_too_long
    matcher:
      kind: substring
      value: "ready"
    timeout_ms: {too_long}
"#
        );
        let err = WorkflowDescriptor::from_yaml_str(&yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("timeout_ms too large"));
    }

    #[tokio::test]
    async fn descriptor_send_ctrl_requires_injector() {
        let yaml = r#"
workflow_schema_version: 1
name: "ctrl_only"
steps:
  - type: send_ctrl
    id: interrupt
    key: ctrl_c
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow = DescriptorWorkflow::new(descriptor);

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("descriptor_send_ctrl.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-ctrl");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("No injector configured"));
            }
            other => panic!("Expected abort, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn descriptor_workflow_logs_policy_summary_on_send_text() {
        let yaml = r#"
workflow_schema_version: 1
name: "send_text_policy"
steps:
  - type: send_text
    id: send_cmd
    text: "echo secret"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow: Arc<dyn Workflow> = Arc::new(DescriptorWorkflow::new(descriptor));

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("descriptor_policy.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::strict(),
                default_wezterm_handle(),
            ),
        ));
        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        let pane_id = 101u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::clone(&workflow));

        let execution_id = generate_workflow_id(workflow.name());
        runner
            .engine
            .start_with_id(
                &storage,
                execution_id.clone(),
                workflow.name(),
                pane_id,
                None,
                None,
            )
            .await
            .unwrap();

        let result = runner
            .run_workflow(pane_id, workflow, &execution_id, 0)
            .await;
        assert!(result.is_aborted(), "Expected policy-gated abort");

        let logs = storage.get_step_logs(&execution_id).await.unwrap();
        assert_eq!(logs.len(), 1, "Expected a single step log entry");

        let policy_summary = logs[0]
            .policy_summary
            .as_ref()
            .expect("policy summary missing");
        let summary_json: serde_json::Value = serde_json::from_str(policy_summary).unwrap();
        assert_ne!(
            summary_json.get("decision").and_then(|v| v.as_str()),
            Some("allow")
        );
    }

    #[tokio::test]
    async fn workflow_runner_emits_step_and_policy_decision_capture_events() {
        let yaml = r#"
workflow_schema_version: 1
name: "decision_capture_flow"
steps:
  - type: send_text
    id: send_cmd
    text: "echo hello"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let workflow: Arc<dyn Workflow> = Arc::new(DescriptorWorkflow::new(descriptor));

        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("workflow_decision_capture.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::strict(),
                default_wezterm_handle(),
            ),
        ));

        let sink = Arc::new(crate::replay_capture::CollectingCaptureSink::new());
        let replay_adapter = Arc::new(crate::replay_capture::CaptureAdapter::new(
            sink.clone(),
            crate::replay_capture::CaptureConfig::default(),
        ));

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        )
        .with_replay_capture_adapter(replay_adapter);

        let pane_id = 101u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::clone(&workflow));

        let execution_id = generate_workflow_id(workflow.name());
        runner
            .engine
            .start_with_id(
                &storage,
                execution_id.clone(),
                workflow.name(),
                pane_id,
                None,
                None,
            )
            .await
            .unwrap();

        let result = runner
            .run_workflow(pane_id, workflow, &execution_id, 0)
            .await;
        assert!(result.is_aborted(), "Expected policy-gated abort");

        let events = sink.recorder_events();
        assert!(
            events.iter().any(|event| {
                matches!(
                    &event.payload,
                    crate::recording::RecorderEventPayload::ControlMarker { details, .. }
                        if details.get("decision_type")
                            == Some(&serde_json::json!("WorkflowStep"))
                )
            }),
            "expected workflow_step decision provenance event"
        );
        assert!(
            events.iter().any(|event| {
                matches!(
                    &event.payload,
                    crate::recording::RecorderEventPayload::ControlMarker { details, .. }
                        if details.get("decision_type")
                            == Some(&serde_json::json!("PolicyEvaluation"))
                )
            }),
            "expected policy_evaluation decision provenance event"
        );
    }

    // --- Custom workflow extensions (wa-fno.2) ---

    #[test]
    fn descriptor_with_triggers_parses() {
        let yaml = r#"
workflow_schema_version: 1
name: "triggered_flow"
description: "A workflow with triggers"
triggers:
  - event_types: ["session.compaction"]
    agent_types: ["codex"]
    rule_ids: ["compaction.detected"]
steps:
  - type: notify
    id: note
    message: "Trigger activated"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        assert_eq!(descriptor.triggers.len(), 1);
        assert_eq!(
            descriptor.triggers[0].event_types,
            vec!["session.compaction"]
        );
        assert_eq!(descriptor.triggers[0].agent_types, vec!["codex"]);
        assert_eq!(descriptor.triggers[0].rule_ids, vec!["compaction.detected"]);
    }

    #[test]
    fn descriptor_trigger_handles_detection() {
        let yaml = r#"
workflow_schema_version: 1
name: "trigger_match"
triggers:
  - event_types: ["session.compaction"]
    agent_types: ["codex"]
steps:
  - type: log
    id: entry
    message: "handled"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        let matching = crate::patterns::Detection {
            rule_id: "any_rule".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(workflow.handles(&matching));

        let non_matching = crate::patterns::Detection {
            rule_id: "any_rule".to_string(),
            agent_type: crate::patterns::AgentType::ClaudeCode,
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&non_matching));
    }

    #[test]
    fn descriptor_no_triggers_does_not_handle() {
        let yaml = r#"
workflow_schema_version: 1
name: "no_triggers"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let detection = crate::patterns::Detection {
            rule_id: "test".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "any.event".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&detection));
    }

    #[test]
    fn descriptor_on_failure_parses() {
        let yaml = r#"
workflow_schema_version: 1
name: "with_failure"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
on_failure:
  action: notify
  message: "Failed at step: ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        assert!(descriptor.on_failure.is_some());
        let handler = descriptor.on_failure.as_ref().unwrap();
        let msg = handler.interpolate_message("pause");
        assert_eq!(msg, "Failed at step: pause");
    }

    #[test]
    fn descriptor_on_failure_log_variant() {
        let yaml = r#"
workflow_schema_version: 1
name: "failure_log"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
on_failure:
  action: log
  message: "Error in ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let handler = descriptor.on_failure.as_ref().unwrap();
        assert!(matches!(handler, DescriptorFailureHandler::Log { .. }));
        assert_eq!(handler.interpolate_message("step_x"), "Error in step_x");
    }

    #[test]
    fn descriptor_notify_step_parses_and_builds_steps() {
        let yaml = r#"
workflow_schema_version: 1
name: "notify_flow"
steps:
  - type: notify
    id: alert
    message: "Something happened"
  - type: log
    id: record
    message: "Logged event"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let steps = workflow.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].name, "alert");
        assert_eq!(steps[1].name, "record");
    }

    #[tokio::test]
    async fn descriptor_notify_step_returns_continue() {
        let yaml = r#"
workflow_schema_version: 1
name: "notify_exec"
steps:
  - type: notify
    id: alert
    message: "test notification"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("notify.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-notify");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    }

    #[tokio::test]
    async fn descriptor_log_step_returns_continue() {
        let yaml = r#"
workflow_schema_version: 1
name: "log_exec"
steps:
  - type: log
    id: entry
    message: "audit trail entry"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("log.db").to_string_lossy().to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-log");
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    }

    #[tokio::test]
    async fn descriptor_abort_step_returns_abort() {
        let yaml = r#"
workflow_schema_version: 1
name: "abort_exec"
steps:
  - type: abort
    id: bail
    reason: "cannot proceed"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-abort");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "cannot proceed"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn descriptor_conditional_then_branch() {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_then"
steps:
  - type: conditional
    id: check
    test_text: "error detected in output"
    matcher:
      kind: substring
      value: "error"
    then_steps:
      - type: notify
        id: alert
        message: "error found"
    else_steps:
      - type: log
        id: ok
        message: "all clear"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond");
        // test_text contains "error" so then_steps should run (notify returns Continue)
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    }

    #[tokio::test]
    async fn descriptor_conditional_else_branch() {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_else"
steps:
  - type: conditional
    id: check
    test_text: "all systems nominal"
    matcher:
      kind: substring
      value: "error"
    then_steps:
      - type: abort
        id: bail
        reason: "error found"
    else_steps:
      - type: notify
        id: ok
        message: "all clear"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond_else.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond-else");
        // test_text does NOT contain "error" so else branch runs (notify returns Continue).
        // Conditionals compile to multiple executable steps (JumpIfFalse + body + Jump);
        // simulate the engine loop: Continue advances to the next step, JumpTo jumps,
        // and Abort/Done/etc. are terminal.
        let num_steps = workflow.step_count();
        let mut step = 0usize;
        let result = loop {
            let r = workflow.execute_step(&mut ctx, step).await;
            match r {
                StepResult::JumpTo { step: target } => step = target,
                StepResult::Continue if step + 1 < num_steps => step += 1,
                other => break other,
            }
        };
        assert!(result.is_continue());
    }

    #[tokio::test]
    async fn descriptor_conditional_then_with_abort() {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_abort"
steps:
  - type: conditional
    id: check
    test_text: "FATAL error occurred"
    matcher:
      kind: regex
      pattern: "FATAL"
    then_steps:
      - type: abort
        id: bail
        reason: "fatal error"
    else_steps: []
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("cond_abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-cond-abort");
        // Conditionals compile to multiple executable steps; simulate the engine loop:
        // Continue advances to the next step, JumpTo jumps, Abort/Done/etc. are terminal.
        let num_steps = workflow.step_count();
        let mut step = 0usize;
        let result = loop {
            let r = workflow.execute_step(&mut ctx, step).await;
            match r {
                StepResult::JumpTo { step: target } => step = target,
                StepResult::Continue if step + 1 < num_steps => step += 1,
                other => break other,
            }
        };
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "fatal error"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn descriptor_loop_repeats_steps() {
        let yaml = r#"
workflow_schema_version: 1
name: "loop_test"
steps:
  - type: loop
    id: repeat
    count: 3
    body:
      - type: log
        id: tick
        message: "iteration"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("loop.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx = WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-loop");
        // All body steps are log (Continue), so loop completes with Continue
        let result = workflow.execute_step(&mut ctx, 0).await;
        assert!(result.is_continue());
    }

    #[tokio::test]
    async fn descriptor_loop_aborts_on_abort_step() {
        let yaml = r#"
workflow_schema_version: 1
name: "loop_abort"
steps:
  - type: loop
    id: repeat
    count: 10
    body:
      - type: abort
        id: bail
        reason: "stop early"
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("loop_abort.db")
            .to_string_lossy()
            .to_string();
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let mut ctx =
            WorkflowContext::new(storage, 1, PaneCapabilities::default(), "exec-loop-abort");
        let result = workflow.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => assert_eq!(reason, "stop early"),
            other => panic!("Expected Abort, got: {other:?}"),
        }
    }

    #[test]
    fn descriptor_rejects_loop_count_too_large() {
        let yaml = r#"
workflow_schema_version: 1
name: "big_loop"
steps:
  - type: loop
    id: big
    count: 1001
    body:
      - type: sleep
        id: pause
        duration_ms: 1
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Loop count too large"));
    }

    #[test]
    fn descriptor_rejects_empty_loop_body() {
        let yaml = r#"
workflow_schema_version: 1
name: "empty_loop"
steps:
  - type: loop
    id: empty
    count: 5
    body: []
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Loop body must contain at least one step"));
    }

    #[test]
    fn descriptor_conditional_validates_nested_steps() {
        let yaml = r#"
workflow_schema_version: 1
name: "cond_validate"
steps:
  - type: conditional
    id: check
    test_text: "test"
    matcher:
      kind: regex
      pattern: "(["
    then_steps: []
    else_steps: []
"#;
        let err = WorkflowDescriptor::from_yaml_str(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Invalid regex pattern"));
    }

    #[test]
    fn descriptor_toml_with_triggers_and_on_failure() {
        let toml = r#"
workflow_schema_version = 1
name = "toml_full"

[[triggers]]
event_types = ["restart.prompt"]
agent_types = ["custom-agent"]

[[steps]]
type = "send_text"
id = "restart"
text = "/restart\n"

[on_failure]
action = "notify"
message = "Failed at ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_toml_str(toml).unwrap();
        assert_eq!(descriptor.triggers.len(), 1);
        assert!(descriptor.on_failure.is_some());
        let handler = descriptor.on_failure.as_ref().unwrap();
        assert_eq!(handler.interpolate_message("restart"), "Failed at restart");
    }

    #[test]
    fn load_workflows_from_dir_loads_yaml_and_toml() {
        let dir = tempfile::TempDir::new().unwrap();

        let yaml_content = r#"
workflow_schema_version: 1
name: "yaml_wf"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        std::fs::write(dir.path().join("wf1.yaml"), yaml_content).unwrap();

        let toml_content = r#"
workflow_schema_version = 1
name = "toml_wf"

[[steps]]
type = "sleep"
id = "pause"
duration_ms = 10
"#;
        std::fs::write(dir.path().join("wf2.toml"), toml_content).unwrap();

        // Should skip non-workflow files
        std::fs::write(dir.path().join("readme.txt"), "ignore me").unwrap();

        let loaded = load_workflows_from_dir(dir.path());
        assert_eq!(loaded.len(), 2);
        let names: Vec<&str> = loaded.iter().map(|(wf, _)| wf.name).collect();
        assert!(names.contains(&"yaml_wf"));
        assert!(names.contains(&"toml_wf"));
    }

    #[test]
    fn load_workflows_from_dir_skips_invalid_files() {
        let dir = tempfile::TempDir::new().unwrap();

        // Valid
        let yaml = r#"
workflow_schema_version: 1
name: "valid"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        std::fs::write(dir.path().join("valid.yaml"), yaml).unwrap();

        // Invalid YAML
        std::fs::write(dir.path().join("broken.yaml"), "not: valid: yaml: {{").unwrap();

        let loaded = load_workflows_from_dir(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.name, "valid");
    }

    #[test]
    fn load_workflows_from_nonexistent_dir_returns_empty() {
        let loaded =
            load_workflows_from_dir(std::path::Path::new("/nonexistent/path/wa/workflows"));
        assert!(loaded.is_empty());
    }

    #[test]
    fn default_workflow_dir_returns_some() {
        // On most systems, config_dir() is available
        let dir = default_workflow_dir();
        if let Some(d) = &dir {
            assert!(d.ends_with("ft/workflows"));
        }
    }

    #[test]
    fn descriptor_failure_handler_abort_variant() {
        let yaml = r#"
workflow_schema_version: 1
name: "fail_abort"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
on_failure:
  action: abort
  message: "Critical failure in ${failed_step}"
"#;
        let descriptor = WorkflowDescriptor::from_yaml_str(yaml).unwrap();
        let handler = descriptor.on_failure.as_ref().unwrap();
        assert!(matches!(handler, DescriptorFailureHandler::Abort { .. }));
        assert_eq!(
            handler.interpolate_message("pause"),
            "Critical failure in pause"
        );
    }

    #[test]
    fn descriptor_multiple_triggers_any_match() {
        let yaml = r#"
workflow_schema_version: 1
name: "multi_trigger"
triggers:
  - event_types: ["session.compaction"]
  - rule_ids: ["usage.warning"]
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        let workflow = DescriptorWorkflow::from_yaml_str(yaml).unwrap();

        // First trigger matches on event_type
        let det1 = crate::patterns::Detection {
            rule_id: "other_rule".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(workflow.handles(&det1));

        // Second trigger matches on rule_id
        let det2 = crate::patterns::Detection {
            rule_id: "usage.warning".to_string(),
            agent_type: crate::patterns::AgentType::ClaudeCode,
            event_type: "other.event".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(workflow.handles(&det2));

        // Neither trigger matches
        let det3 = crate::patterns::Detection {
            rule_id: "unrelated".to_string(),
            agent_type: crate::patterns::AgentType::Gemini,
            event_type: "unrelated.event".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: String::new(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&det3));
    }

    #[test]
    fn descriptor_yml_extension_loads() {
        let dir = tempfile::TempDir::new().unwrap();
        let yaml = r#"
workflow_schema_version: 1
name: "yml_ext"
steps:
  - type: sleep
    id: pause
    duration_ms: 10
"#;
        std::fs::write(dir.path().join("test.yml"), yaml).unwrap();
        let loaded = load_workflows_from_dir(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0.name, "yml_ext");
    }

    #[tokio::test]
    async fn pane_idle_uses_heuristics_when_no_osc133() {
        let source = MockPaneSource::new(vec!["user@host:~$ ".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pane_idle(0);
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        if let WaitConditionResult::Satisfied { context, .. } = result {
            assert!(context.unwrap().contains("heuristic"));
        }
    }

    #[tokio::test]
    async fn pane_idle_respects_threshold_duration() {
        use crate::ingest::{Osc133State, ShellState};

        let source = MockPaneSource::new(vec!["some text".to_string()]);
        let engine = PatternEngine::new();
        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let executor = WaitConditionExecutor::new(&source, &engine)
            .with_osc_state(&osc_state)
            .with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(10),
                poll_max: Duration::from_millis(50),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        // Require 50ms idle threshold
        let condition = WaitCondition::pane_idle(50);
        let start = std::time::Instant::now();
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;
        let elapsed = start.elapsed();

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
        // Should have waited at least the threshold duration
        assert!(elapsed >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn stable_tail_succeeds_after_stability_window() {
        let source = MockPaneSource::new(vec![
            "compaction in progress".to_string(),
            "compaction in progress".to_string(),
            "compaction in progress".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 100,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::stable_tail(1);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(50))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_satisfied());
    }

    #[tokio::test]
    async fn stable_tail_times_out_when_changing() {
        let source = MockPaneSource::new(vec![
            "line 1".to_string(),
            "line 2".to_string(),
            "line 3".to_string(),
            "line 4".to_string(),
        ]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(5),
                max_polls: 5,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::stable_tail(100);
        let result = executor
            .execute(&condition, 1, Duration::from_millis(10))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
    }

    #[tokio::test]
    async fn external_wait_returns_unsupported() {
        let source = MockPaneSource::new(vec!["text".to_string()]);
        let engine = PatternEngine::new();

        let executor = WaitConditionExecutor::new(&source, &engine);
        let condition = WaitCondition::external("my_signal");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(5))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        match result {
            WaitConditionResult::Unsupported { reason } => {
                assert!(reason.contains("my_signal"));
            }
            _ => panic!("Expected Unsupported"),
        }
    }

    #[tokio::test]
    async fn wait_respects_max_polls() {
        let source = MockPaneSource::new(vec!["no match".to_string()]);
        let engine = PatternEngine::new();

        let executor =
            WaitConditionExecutor::new(&source, &engine).with_options(WaitConditionOptions {
                tail_lines: 200,
                poll_initial: Duration::from_millis(1),
                poll_max: Duration::from_millis(1),
                max_polls: 3,
                allow_idle_heuristics: true,
            });

        let condition = WaitCondition::pattern("nonexistent.rule");
        let result = executor
            .execute(&condition, 1, Duration::from_secs(60))
            .await;

        assert!(result.is_ok());
        let result = result.unwrap();
        assert!(result.is_timed_out());
        if let WaitConditionResult::TimedOut { polls, .. } = result {
            assert!(polls <= 3);
        }
    }

    // ========================================================================
    // Workflow Persistence Tests (wa-nu4.1.1.3)
    // ========================================================================

    #[test]
    fn compute_next_step_empty_logs_returns_zero() {
        let logs: Vec<crate::storage::WorkflowStepLogRecord> = vec![];
        assert_eq!(super::compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_with_continue_returns_next() {
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 0,
            step_name: "step_0".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "continue".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        assert_eq!(super::compute_next_step(&logs), 1);
    }

    #[test]
    fn compute_next_step_with_done_returns_next() {
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 2,
            step_name: "step_2".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "done".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        assert_eq!(super::compute_next_step(&logs), 3);
    }

    #[test]
    fn compute_next_step_with_retry_returns_same() {
        // Retry means the step should be re-executed
        let logs = vec![crate::storage::WorkflowStepLogRecord {
            id: 1,
            workflow_id: "test-123".to_string(),
            audit_action_id: None,
            step_index: 1,
            step_name: "step_1".to_string(),
            step_id: None,
            step_kind: None,
            result_type: "retry".to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 1100,
            duration_ms: 100,
        }];
        // No completed steps, so start from 0
        assert_eq!(super::compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_mixed_logs_finds_highest_completed() {
        let logs = vec![
            crate::storage::WorkflowStepLogRecord {
                id: 1,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 0,
                step_name: "step_0".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1000,
                completed_at: 1100,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 2,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 1,
                step_name: "step_1".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1100,
                completed_at: 1200,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 3,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 2,
                step_name: "step_2".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "retry".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1200,
                completed_at: 1300,
                duration_ms: 100,
            },
        ];
        // Highest completed is step_index 1, so next is 2
        assert_eq!(super::compute_next_step(&logs), 2);
    }

    #[test]
    fn compute_next_step_out_of_order_logs() {
        // Logs might not be in order; function should still find max
        let logs = vec![
            crate::storage::WorkflowStepLogRecord {
                id: 3,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 2,
                step_name: "step_2".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1200,
                completed_at: 1300,
                duration_ms: 100,
            },
            crate::storage::WorkflowStepLogRecord {
                id: 1,
                workflow_id: "test-123".to_string(),
                audit_action_id: None,
                step_index: 0,
                step_name: "step_0".to_string(),
                step_id: None,
                step_kind: None,
                result_type: "continue".to_string(),
                result_data: None,
                policy_summary: None,
                verification_refs: None,
                error_code: None,
                started_at: 1000,
                completed_at: 1100,
                duration_ms: 100,
            },
        ];
        // Highest completed is step_index 2, so next is 3
        assert_eq!(super::compute_next_step(&logs), 3);
    }

    #[test]
    fn generate_workflow_id_format() {
        let id = super::generate_workflow_id("test_workflow");
        assert!(id.starts_with("test_workflow-"));
        // Should have format: name-timestamp-random
        let parts: Vec<&str> = id.split('-').collect();
        assert!(parts.len() >= 3);
        // Last part should be hex (8 chars)
        let last = parts.last().unwrap();
        assert_eq!(last.len(), 8);
        assert!(last.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_workflow_id_uniqueness() {
        let id1 = super::generate_workflow_id("workflow");
        let id2 = super::generate_workflow_id("workflow");
        // Random component should make them different
        assert_ne!(id1, id2);
    }

    #[test]
    fn execution_status_serialization() {
        let statuses = [
            ExecutionStatus::Running,
            ExecutionStatus::Waiting,
            ExecutionStatus::Completed,
            ExecutionStatus::Aborted,
        ];

        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let parsed: ExecutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, status);
        }
    }

    #[test]
    fn workflow_execution_serialization() {
        let execution = WorkflowExecution {
            id: "test-123-abc".to_string(),
            workflow_name: "test_workflow".to_string(),
            pane_id: 42,
            current_step: 2,
            status: ExecutionStatus::Running,
            started_at: 1000,
            updated_at: 1500,
        };

        let json = serde_json::to_string(&execution).unwrap();
        let parsed: WorkflowExecution = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.id, execution.id);
        assert_eq!(parsed.workflow_name, execution.workflow_name);
        assert_eq!(parsed.pane_id, execution.pane_id);
        assert_eq!(parsed.current_step, execution.current_step);
        assert_eq!(parsed.status, execution.status);
    }

    // ========================================================================
    // PaneWorkflowLockManager Tests (wa-nu4.1.1.2)
    // ========================================================================

    #[test]
    fn lock_manager_acquire_and_release() {
        let manager = PaneWorkflowLockManager::new();

        // Initially unlocked
        assert!(manager.is_locked(42).is_none());

        // Acquire succeeds
        let result = manager.try_acquire(42, "test_workflow", "exec-001");
        assert!(result.is_acquired());
        assert!(!result.is_already_locked());

        // Now locked
        let lock_info = manager.is_locked(42);
        assert!(lock_info.is_some());
        let info = lock_info.unwrap();
        assert_eq!(info.pane_id, 42);
        assert_eq!(info.workflow_name, "test_workflow");
        assert_eq!(info.execution_id, "exec-001");
        assert!(info.locked_at_ms > 0);

        // Release succeeds
        assert!(manager.release(42, "exec-001"));

        // Now unlocked
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_manager_double_acquire_fails() {
        let manager = PaneWorkflowLockManager::new();

        // First acquire succeeds
        let result1 = manager.try_acquire(42, "workflow_a", "exec-001");
        assert!(result1.is_acquired());

        // Second acquire fails with details about the existing lock
        let result2 = manager.try_acquire(42, "workflow_b", "exec-002");
        assert!(result2.is_already_locked());
        match result2 {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                locked_since_ms,
            } => {
                assert_eq!(held_by_workflow, "workflow_a");
                assert_eq!(held_by_execution, "exec-001");
                assert!(locked_since_ms > 0);
            }
            LockAcquisitionResult::Acquired => panic!("Expected AlreadyLocked"),
        }

        // Release and retry succeeds
        manager.release(42, "exec-001");
        let result3 = manager.try_acquire(42, "workflow_b", "exec-002");
        assert!(result3.is_acquired());
    }

    #[test]
    fn lock_manager_release_with_wrong_execution_id_fails() {
        let manager = PaneWorkflowLockManager::new();

        manager.try_acquire(42, "test_workflow", "exec-001");

        // Release with wrong execution_id fails
        assert!(!manager.release(42, "wrong-exec-id"));

        // Lock still held
        assert!(manager.is_locked(42).is_some());

        // Correct execution_id works
        assert!(manager.release(42, "exec-001"));
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_manager_multiple_panes_independent() {
        let manager = PaneWorkflowLockManager::new();

        // Lock pane 1
        let r1 = manager.try_acquire(1, "workflow_a", "exec-001");
        assert!(r1.is_acquired());

        // Lock pane 2 succeeds (different pane)
        let r2 = manager.try_acquire(2, "workflow_b", "exec-002");
        assert!(r2.is_acquired());

        // Lock pane 3 succeeds
        let r3 = manager.try_acquire(3, "workflow_c", "exec-003");
        assert!(r3.is_acquired());

        // All locked
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_some());
        assert!(manager.is_locked(3).is_some());

        // Release pane 2 doesn't affect others
        manager.release(2, "exec-002");
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_none());
        assert!(manager.is_locked(3).is_some());
    }

    #[test]
    fn lock_manager_active_locks() {
        let manager = PaneWorkflowLockManager::new();

        // Initially empty
        assert!(manager.active_locks().is_empty());

        manager.try_acquire(1, "workflow_a", "exec-001");
        manager.try_acquire(2, "workflow_b", "exec-002");

        let active = manager.active_locks();
        assert_eq!(active.len(), 2);

        let pane_ids: std::collections::HashSet<u64> = active.iter().map(|l| l.pane_id).collect();
        assert!(pane_ids.contains(&1));
        assert!(pane_ids.contains(&2));
    }

    #[test]
    fn lock_guard_releases_on_drop() {
        let manager = PaneWorkflowLockManager::new();

        // Acquire via guard
        {
            let guard = manager.acquire_guard(42, "test_workflow", "exec-001");
            assert!(guard.is_some());
            let guard = guard.unwrap();
            assert_eq!(guard.pane_id(), 42);
            assert_eq!(guard.execution_id(), "exec-001");

            // Lock is held
            assert!(manager.is_locked(42).is_some());
        }

        // Guard dropped, lock released
        assert!(manager.is_locked(42).is_none());
    }

    #[test]
    fn lock_guard_acquire_fails_when_locked() {
        let manager = PaneWorkflowLockManager::new();

        // Acquire first lock
        let _guard1 = manager.acquire_guard(42, "workflow_a", "exec-001");
        assert!(manager.is_locked(42).is_some());

        // Second acquire fails
        let guard2 = manager.acquire_guard(42, "workflow_b", "exec-002");
        assert!(guard2.is_none());
    }

    #[test]
    fn lock_manager_force_release() {
        let manager = PaneWorkflowLockManager::new();

        manager.try_acquire(42, "test_workflow", "exec-001");
        assert!(manager.is_locked(42).is_some());

        // Force release works even with unknown execution_id
        let removed = manager.force_release(42);
        assert!(removed.is_some());
        let info = removed.unwrap();
        assert_eq!(info.execution_id, "exec-001");

        // Now unlocked
        assert!(manager.is_locked(42).is_none());

        // Force release on unlocked pane returns None
        assert!(manager.force_release(42).is_none());
    }

    #[test]
    fn lock_acquisition_result_methods() {
        let acquired = LockAcquisitionResult::Acquired;
        assert!(acquired.is_acquired());
        assert!(!acquired.is_already_locked());

        let locked = LockAcquisitionResult::AlreadyLocked {
            held_by_workflow: "test".to_string(),
            held_by_execution: "exec-001".to_string(),
            locked_since_ms: 1_234_567_890,
        };
        assert!(!locked.is_acquired());
        assert!(locked.is_already_locked());
    }

    #[test]
    fn lock_manager_concurrent_simulation() {
        use std::sync::Arc;
        use std::thread;

        let manager = Arc::new(PaneWorkflowLockManager::new());
        let pane_id = 42;

        // Simulate concurrent access with threads
        let mut handles = vec![];

        for i in 0..10 {
            let m = Arc::clone(&manager);
            let handle = thread::spawn(move || {
                let exec_id = format!("exec-{i:03}");
                m.try_acquire(pane_id, "concurrent_workflow", &exec_id)
            });
            handles.push(handle);
        }

        // Collect results
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Exactly one should have acquired the lock
        let acquired_count = results.iter().filter(|r| r.is_acquired()).count();
        let locked_count = results.iter().filter(|r| r.is_already_locked()).count();

        assert_eq!(acquired_count, 1);
        assert_eq!(locked_count, 9);
    }

    #[test]
    fn pane_lock_info_serialization() {
        let info = PaneLockInfo {
            pane_id: 42,
            workflow_name: "test_workflow".to_string(),
            execution_id: "exec-001".to_string(),
            locked_at_ms: 1_234_567_890_000,
        };

        let json = serde_json::to_string(&info).unwrap();
        let parsed: PaneLockInfo = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.pane_id, info.pane_id);
        assert_eq!(parsed.workflow_name, info.workflow_name);
        assert_eq!(parsed.execution_id, info.execution_id);
        assert_eq!(parsed.locked_at_ms, info.locked_at_ms);
    }

    // ========================================================================
    // Coordination Primitives Tests (wa-nu4.4.4.1)
    // ========================================================================

    #[test]
    fn pane_group_by_domain() {
        let panes = vec![
            crate::storage::PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 2,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 3,
                pane_uuid: None,
                domain: "SSH:host1".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
        ];

        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByDomain);
        assert_eq!(groups.len(), 2);
        // BTreeMap sorts by key, so "SSH:host1" comes before "local"
        assert_eq!(groups[0].name, "SSH:host1");
        assert_eq!(groups[0].pane_ids, vec![3]);
        assert_eq!(groups[1].name, "local");
        assert_eq!(groups[1].pane_ids, vec![1, 2]);
    }

    #[test]
    fn pane_group_by_agent() {
        let panes = vec![
            crate::storage::PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("codex session".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 2,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("claude code".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 3,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("bash shell".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
        ];

        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByAgent);
        assert_eq!(groups.len(), 3);
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"claude_code"));
        assert!(names.contains(&"unknown"));
    }

    #[test]
    fn pane_group_by_project() {
        let panes = vec![
            crate::storage::PaneRecord {
                pane_id: 1,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/home/user/project-a".to_string()),
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 2,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/home/user/project-a".to_string()),
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
            crate::storage::PaneRecord {
                pane_id: 3,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: None,
                cwd: Some("/home/user/project-b".to_string()),
                tty_name: None,
                first_seen_at: 0,
                last_seen_at: 0,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            },
        ];

        let groups = build_pane_groups(&panes, &PaneGroupStrategy::ByProject);
        assert_eq!(groups.len(), 2);
        let proj_a = groups
            .iter()
            .find(|g| g.name.contains("project-a"))
            .unwrap();
        assert_eq!(proj_a.pane_ids, vec![1, 2]);
        let proj_b = groups
            .iter()
            .find(|g| g.name.contains("project-b"))
            .unwrap();
        assert_eq!(proj_b.pane_ids, vec![3]);
    }

    #[test]
    fn pane_group_explicit() {
        let groups = build_pane_groups(
            &[],
            &PaneGroupStrategy::Explicit {
                pane_ids: vec![5, 3, 1],
            },
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "explicit");
        assert_eq!(groups[0].pane_ids, vec![1, 3, 5]); // sorted
    }

    #[test]
    fn pane_group_len_and_is_empty() {
        let group = PaneGroup::new("test", vec![1, 2, 3], PaneGroupStrategy::ByDomain);
        assert_eq!(group.len(), 3);
        assert!(!group.is_empty());

        let empty = PaneGroup::new("empty", vec![], PaneGroupStrategy::ByDomain);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn group_lock_all_succeed() {
        let manager = PaneWorkflowLockManager::new();
        let result = manager.try_acquire_group(&[1, 2, 3], "swarm_wf", "exec-001");
        assert!(result.is_acquired());
        match &result {
            GroupLockResult::Acquired { locked_panes } => {
                assert_eq!(locked_panes.len(), 3);
            }
            GroupLockResult::PartialFailure { .. } => panic!("Expected Acquired"),
        }

        // All should be locked
        assert!(manager.is_locked(1).is_some());
        assert!(manager.is_locked(2).is_some());
        assert!(manager.is_locked(3).is_some());
    }

    #[test]
    fn group_lock_partial_failure_rolls_back() {
        let manager = PaneWorkflowLockManager::new();

        // Pre-lock pane 2
        manager.try_acquire(2, "other_wf", "exec-other");

        // Group lock should fail and roll back
        let result = manager.try_acquire_group(&[1, 2, 3], "swarm_wf", "exec-001");
        assert!(!result.is_acquired());

        match &result {
            GroupLockResult::PartialFailure {
                would_have_locked,
                conflicts,
            } => {
                // Pane 1 would have been locked but was rolled back
                assert!(would_have_locked.contains(&1));
                assert_eq!(conflicts.len(), 1);
                assert_eq!(conflicts[0].pane_id, 2);
                assert_eq!(conflicts[0].held_by_workflow, "other_wf");
            }
            GroupLockResult::Acquired { .. } => panic!("Expected PartialFailure"),
        }

        // Pane 1 should NOT be locked (rollback)
        assert!(manager.is_locked(1).is_none());
        // Pane 2 still locked by original holder
        assert!(manager.is_locked(2).is_some());
        // Pane 3 should NOT be locked (never reached or rolled back)
        assert!(manager.is_locked(3).is_none());
    }

    #[test]
    fn group_lock_release_all() {
        let manager = PaneWorkflowLockManager::new();
        manager.try_acquire_group(&[1, 2, 3], "swarm_wf", "exec-001");

        let released = manager.release_group(&[1, 2, 3], "exec-001");
        assert_eq!(released, 3);

        assert!(manager.is_locked(1).is_none());
        assert!(manager.is_locked(2).is_none());
        assert!(manager.is_locked(3).is_none());
    }

    #[test]
    fn broadcast_precondition_prompt_active() {
        let passing = PaneCapabilities {
            prompt_active: true,
            ..Default::default()
        };
        let failing = PaneCapabilities {
            prompt_active: false,
            ..Default::default()
        };
        assert!(BroadcastPrecondition::PromptActive.check(&passing));
        assert!(!BroadcastPrecondition::PromptActive.check(&failing));
    }

    #[test]
    fn broadcast_precondition_not_alt_screen() {
        let normal = PaneCapabilities {
            alt_screen: Some(false),
            ..Default::default()
        };
        let alt = PaneCapabilities {
            alt_screen: Some(true),
            ..Default::default()
        };
        let unknown = PaneCapabilities {
            alt_screen: None,
            ..Default::default()
        };
        assert!(BroadcastPrecondition::NotAltScreen.check(&normal));
        assert!(!BroadcastPrecondition::NotAltScreen.check(&alt));
        assert!(BroadcastPrecondition::NotAltScreen.check(&unknown));
    }

    #[test]
    fn broadcast_precondition_no_gap_and_not_reserved() {
        let safe = PaneCapabilities {
            has_recent_gap: false,
            is_reserved: false,
            ..Default::default()
        };
        assert!(BroadcastPrecondition::NoRecentGap.check(&safe));
        assert!(BroadcastPrecondition::NotReserved.check(&safe));

        let risky = PaneCapabilities {
            has_recent_gap: true,
            is_reserved: true,
            ..Default::default()
        };
        assert!(!BroadcastPrecondition::NoRecentGap.check(&risky));
        assert!(!BroadcastPrecondition::NotReserved.check(&risky));
    }

    #[test]
    fn check_preconditions_returns_failed_labels() {
        let preconditions = default_broadcast_preconditions();
        let caps = PaneCapabilities {
            prompt_active: false,
            alt_screen: Some(true),
            has_recent_gap: false,
            is_reserved: false,
            ..Default::default()
        };
        let failures = check_preconditions(&preconditions, &caps);
        assert!(failures.contains(&"prompt_active"));
        assert!(failures.contains(&"not_alt_screen"));
        assert!(!failures.contains(&"no_recent_gap"));
    }

    #[test]
    fn check_preconditions_all_pass() {
        let preconditions = default_broadcast_preconditions();
        let caps = PaneCapabilities {
            prompt_active: true,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            ..Default::default()
        };
        let failures = check_preconditions(&preconditions, &caps);
        assert!(failures.is_empty());
    }

    #[test]
    fn broadcast_result_tracking() {
        let mut result = BroadcastResult::new("test_action");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 100 });
        result.add_outcome(
            2,
            PaneBroadcastOutcome::Denied {
                reason: "policy denied".to_string(),
            },
        );
        result.add_outcome(
            3,
            PaneBroadcastOutcome::PreconditionFailed {
                failed: vec!["prompt_active".to_string()],
            },
        );
        result.add_outcome(
            4,
            PaneBroadcastOutcome::Skipped {
                reason: "locked".to_string(),
            },
        );

        assert_eq!(result.allowed_count(), 1);
        assert_eq!(result.denied_count(), 1);
        assert_eq!(result.precondition_failed_count(), 1);
        assert_eq!(result.skipped_count(), 1);
        assert!(!result.all_allowed());
    }

    #[test]
    fn broadcast_result_all_allowed() {
        let mut result = BroadcastResult::new("multi_pane_restart");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 50 });
        result.add_outcome(2, PaneBroadcastOutcome::Allowed { elapsed_ms: 75 });

        assert!(result.all_allowed());
        assert_eq!(result.allowed_count(), 2);
    }

    #[test]
    fn broadcast_result_serialization() {
        let mut result = BroadcastResult::new("test");
        result.add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 100 });
        result.total_elapsed_ms = 150;

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["action"], "test");
        assert_eq!(json["total_elapsed_ms"], 150);
        assert_eq!(json["outcomes"][0]["pane_id"], 1);
        assert_eq!(json["outcomes"][0]["outcome"]["status"], "allowed");
    }

    #[test]
    fn pane_group_strategy_serialization() {
        let strategy = PaneGroupStrategy::Explicit {
            pane_ids: vec![1, 2, 3],
        };
        let json = serde_json::to_value(&strategy).unwrap();
        assert_eq!(json["type"], "explicit");
        assert_eq!(json["pane_ids"], serde_json::json!([1, 2, 3]));

        let by_domain = PaneGroupStrategy::ByDomain;
        let json = serde_json::to_value(&by_domain).unwrap();
        assert_eq!(json["type"], "by_domain");
    }

    #[test]
    fn infer_agent_from_title_works() {
        assert_eq!(infer_agent_from_title("Codex CLI session"), Some("codex"));
        assert_eq!(
            infer_agent_from_title("Claude Code - project"),
            Some("claude_code")
        );
        assert_eq!(infer_agent_from_title("Gemini workspace"), Some("gemini"));
        assert_eq!(infer_agent_from_title("bash shell"), None);
    }

    #[test]
    fn default_broadcast_preconditions_has_all_four() {
        let preconditions = default_broadcast_preconditions();
        assert_eq!(preconditions.len(), 4);
        let labels: Vec<&str> = preconditions.iter().map(|p| p.label()).collect();
        assert!(labels.contains(&"prompt_active"));
        assert!(labels.contains(&"not_alt_screen"));
        assert!(labels.contains(&"no_recent_gap"));
        assert!(labels.contains(&"not_reserved"));
    }

    // ========================================================================
    // Multi-Pane Coordination Workflow Tests (wa-nu4.4.4.2)
    // ========================================================================

    fn make_test_pane(
        pane_id: u64,
        domain: &str,
        title: Option<&str>,
        cwd: Option<&str>,
    ) -> crate::storage::PaneRecord {
        crate::storage::PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: domain.to_string(),
            window_id: None,
            tab_id: None,
            title: title.map(str::to_string),
            cwd: cwd.map(str::to_string),
            tty_name: None,
            first_seen_at: 0,
            last_seen_at: 0,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    fn make_ready_caps() -> crate::policy::PaneCapabilities {
        crate::policy::PaneCapabilities {
            prompt_active: true,
            command_running: false,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    fn make_busy_caps() -> crate::policy::PaneCapabilities {
        crate::policy::PaneCapabilities {
            prompt_active: false,
            command_running: true,
            alt_screen: Some(false),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    fn make_alt_screen_caps() -> crate::policy::PaneCapabilities {
        crate::policy::PaneCapabilities {
            prompt_active: false,
            command_running: false,
            alt_screen: Some(true),
            has_recent_gap: false,
            is_reserved: false,
            reserved_by: None,
        }
    }

    #[test]
    fn coordinate_agents_config_defaults() {
        let config = CoordinateAgentsConfig::default();
        assert_eq!(config.strategy, PaneGroupStrategy::ByAgent);
        assert_eq!(config.preconditions.len(), 4);
        assert!(!config.abort_on_lock_failure);
    }

    #[test]
    fn coordinate_agents_config_serialization() {
        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::Explicit {
                pane_ids: vec![1, 2],
            },
            preconditions: vec![BroadcastPrecondition::PromptActive],
            abort_on_lock_failure: true,
        };
        let json = serde_json::to_value(&config).expect("serialize");
        assert_eq!(json["abort_on_lock_failure"], true);
        let rt: CoordinateAgentsConfig = serde_json::from_value(json).expect("deserialize");
        assert!(rt.abort_on_lock_failure);
        assert_eq!(rt.preconditions.len(), 1);
    }

    #[test]
    fn agent_reread_prompt_for_known_agents() {
        assert!(agent_reread_prompt("codex").contains("AGENTS.md"));
        assert!(agent_reread_prompt("claude_code").contains("AGENTS.md"));
        assert!(agent_reread_prompt("gemini").contains("AGENTS.md"));
        assert!(agent_reread_prompt("unknown").contains("AGENTS.md"));
    }

    #[test]
    fn agent_pause_text_is_ctrl_c() {
        for agent in &["codex", "claude_code", "gemini", "unknown"] {
            assert_eq!(agent_pause_text(agent), "\x03");
        }
    }

    #[test]
    fn evaluate_preconditions_all_pass() {
        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());

        let results =
            evaluate_pane_preconditions(&[1, 2], &caps, &default_broadcast_preconditions());

        assert_eq!(results.len(), 2);
        assert!(results[0].1.is_none()); // pane 1 passed
        assert!(results[1].1.is_none()); // pane 2 passed
    }

    #[test]
    fn evaluate_preconditions_filters_busy_pane() {
        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_busy_caps()); // command running, no prompt

        let results =
            evaluate_pane_preconditions(&[1, 2], &caps, &default_broadcast_preconditions());

        assert!(results[0].1.is_none()); // pane 1 passed
        match &results[1].1 {
            Some(PaneBroadcastOutcome::PreconditionFailed { failed }) => {
                assert!(failed.contains(&"prompt_active".to_string()));
            }
            other => panic!("Expected PreconditionFailed, got {other:?}"),
        }
    }

    #[test]
    fn evaluate_preconditions_missing_caps_skips() {
        let caps = std::collections::HashMap::new(); // no caps at all

        let results = evaluate_pane_preconditions(&[1], &caps, &default_broadcast_preconditions());

        match &results[0].1 {
            Some(PaneBroadcastOutcome::Skipped { reason }) => {
                assert!(reason.contains("no capabilities"));
            }
            other => panic!("Expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn plan_reread_context_filters_and_groups() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("claude code"), None),
            make_test_pane(3, "local", Some("vim editor"), None), // will be in alt-screen
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());
        caps.insert(3, make_alt_screen_caps());

        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::ByAgent,
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        };

        let result = plan_reread_context(&panes, &caps, &config);

        assert_eq!(result.operation, "reread_context");
        assert_eq!(result.total_panes(), 3);
        assert_eq!(result.total_acted(), 2); // panes 1 and 2
        assert_eq!(result.broadcast.allowed_count(), 2);
        assert_eq!(result.broadcast.precondition_failed_count(), 1); // pane 3
    }

    #[test]
    fn plan_reread_context_empty_panes() {
        let panes: Vec<crate::storage::PaneRecord> = vec![];
        let caps = std::collections::HashMap::new();
        let config = CoordinateAgentsConfig::default();

        let result = plan_reread_context(&panes, &caps, &config);

        assert_eq!(result.total_panes(), 0);
        assert_eq!(result.total_acted(), 0);
        assert!(result.groups.is_empty());
    }

    #[test]
    fn plan_pause_all_relaxed_preconditions() {
        // pause_all should only filter by NotAltScreen, not by PromptActive
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("claude code"), None),
            make_test_pane(3, "local", Some("vim editor"), None),
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_busy_caps()); // command running — should still get paused
        caps.insert(2, make_ready_caps());
        caps.insert(3, make_alt_screen_caps()); // alt-screen — should be filtered out

        let config = CoordinateAgentsConfig::default();
        let result = plan_pause_all(&panes, &caps, &config);

        assert_eq!(result.operation, "pause_all");
        // panes 1 and 2 should be acted on, pane 3 filtered by alt-screen
        assert_eq!(result.broadcast.allowed_count(), 2);
        assert_eq!(result.broadcast.precondition_failed_count(), 1);
    }

    #[test]
    fn plan_pause_all_explicit_strategy() {
        let panes = vec![
            make_test_pane(1, "local", None, None),
            make_test_pane(2, "local", None, None),
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());

        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::Explicit {
                pane_ids: vec![1, 2],
            },
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        };

        let result = plan_pause_all(&panes, &caps, &config);
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].group_name, "explicit");
        assert_eq!(result.broadcast.allowed_count(), 2);
    }

    #[test]
    fn resolve_reread_prompts_agent_specific() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("claude code"), None),
            make_test_pane(3, "local", Some("bash shell"), None),
        ];

        let prompts = resolve_reread_prompts(&panes);
        assert_eq!(prompts.len(), 3);
        assert!(prompts[&1].contains("AGENTS.md"));
        assert!(prompts[&2].contains("AGENTS.md"));
        assert!(prompts[&3].contains("AGENTS.md"));
        // Codex gets the plain-text version, Claude gets /read
        assert!(prompts[&1].starts_with("Read"));
        assert!(prompts[&2].starts_with("/read"));
    }

    #[test]
    fn resolve_pause_texts_all_ctrl_c() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex session"), None),
            make_test_pane(2, "local", Some("unknown"), None),
        ];

        let texts = resolve_pause_texts(&panes);
        assert_eq!(texts[&1], "\x03");
        assert_eq!(texts[&2], "\x03");
    }

    #[test]
    fn coordination_result_new_and_accessors() {
        let mut result = CoordinationResult::new("test_op");
        assert_eq!(result.operation, "test_op");
        assert_eq!(result.total_panes(), 0);
        assert_eq!(result.total_acted(), 0);

        result.groups.push(GroupCoordinationEntry {
            group_name: "g1".to_string(),
            pane_count: 3,
            acted_count: 2,
            precondition_failed_count: 1,
            skipped_count: 0,
        });
        result.groups.push(GroupCoordinationEntry {
            group_name: "g2".to_string(),
            pane_count: 2,
            acted_count: 1,
            precondition_failed_count: 0,
            skipped_count: 1,
        });

        assert_eq!(result.total_panes(), 5);
        assert_eq!(result.total_acted(), 3);
    }

    #[test]
    fn coordination_result_serialization() {
        let mut result = CoordinationResult::new("reread_context");
        result.groups.push(GroupCoordinationEntry {
            group_name: "codex".to_string(),
            pane_count: 2,
            acted_count: 2,
            precondition_failed_count: 0,
            skipped_count: 0,
        });
        result
            .broadcast
            .add_outcome(1, PaneBroadcastOutcome::Allowed { elapsed_ms: 5 });
        result
            .broadcast
            .add_outcome(2, PaneBroadcastOutcome::Allowed { elapsed_ms: 3 });

        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(json["operation"], "reread_context");
        assert_eq!(json["groups"][0]["group_name"], "codex");
        assert_eq!(json["broadcast"]["outcomes"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn plan_reread_context_by_domain_groups_correctly() {
        let panes = vec![
            make_test_pane(1, "local", Some("codex"), None),
            make_test_pane(2, "local", Some("claude"), None),
            make_test_pane(3, "SSH:remote", Some("codex"), None),
        ];

        let mut caps = std::collections::HashMap::new();
        caps.insert(1, make_ready_caps());
        caps.insert(2, make_ready_caps());
        caps.insert(3, make_ready_caps());

        let config = CoordinateAgentsConfig {
            strategy: PaneGroupStrategy::ByDomain,
            preconditions: default_broadcast_preconditions(),
            abort_on_lock_failure: false,
        };

        let result = plan_reread_context(&panes, &caps, &config);
        assert_eq!(result.groups.len(), 2); // "SSH:remote" and "local"
        assert_eq!(result.broadcast.allowed_count(), 3);
    }

    #[test]
    fn group_coordination_entry_fields() {
        let entry = GroupCoordinationEntry {
            group_name: "test".to_string(),
            pane_count: 5,
            acted_count: 3,
            precondition_failed_count: 1,
            skipped_count: 1,
        };
        let json = serde_json::to_value(&entry).expect("serialize");
        assert_eq!(json["pane_count"], 5);
        assert_eq!(json["acted_count"], 3);
        assert_eq!(json["precondition_failed_count"], 1);
        assert_eq!(json["skipped_count"], 1);
    }

    // ========================================================================
    // Unstick Workflow Tests (wa-nu4.4.4.4)
    // ========================================================================

    #[test]
    fn unstick_finding_kind_labels() {
        assert_eq!(UnstickFindingKind::TodoComment.label(), "TODO/FIXME");
        assert_eq!(UnstickFindingKind::PanicSite.label(), "panic site");
        assert_eq!(
            UnstickFindingKind::SuppressedError.label(),
            "suppressed error"
        );
    }

    #[test]
    fn unstick_finding_kind_serialization() {
        let json = serde_json::to_value(UnstickFindingKind::TodoComment).expect("serialize");
        assert_eq!(json, "todo_comment");

        let json = serde_json::to_value(UnstickFindingKind::PanicSite).expect("serialize");
        assert_eq!(json, "panic_site");

        let rt: UnstickFindingKind =
            serde_json::from_str("\"suppressed_error\"").expect("deserialize");
        assert_eq!(rt, UnstickFindingKind::SuppressedError);
    }

    #[test]
    fn unstick_config_defaults() {
        let config = UnstickConfig::default();
        assert_eq!(config.max_findings_per_kind, 10);
        assert_eq!(config.max_total_findings, 25);
        assert!(config.extensions.contains(&"rs".to_string()));
        assert!(config.extensions.contains(&"py".to_string()));
    }

    #[test]
    fn unstick_report_empty() {
        let report = UnstickReport::empty("text");
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.scanner, "text");
        assert!(!report.truncated);
        assert_eq!(report.human_summary(), "No actionable findings.");
    }

    #[test]
    fn unstick_report_with_findings() {
        let mut report = UnstickReport::empty("text");
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::TodoComment,
            file: "src/main.rs".to_string(),
            line: 42,
            snippet: "// TODO: fix this".to_string(),
            suggestion: "Address this TODO".to_string(),
        });
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::PanicSite,
            file: "src/lib.rs".to_string(),
            line: 100,
            snippet: "value.unwrap()".to_string(),
            suggestion: "Use ? operator".to_string(),
        });
        report.files_scanned = 5;
        report.counts.insert("TODO/FIXME".to_string(), 1);
        report.counts.insert("panic site".to_string(), 1);

        assert_eq!(report.total_findings(), 2);
        let summary = report.human_summary();
        assert!(summary.contains("Found 2 items"));
        assert!(summary.contains("src/main.rs:42"));
        assert!(summary.contains("src/lib.rs:100"));
    }

    #[test]
    fn unstick_report_serialization() {
        let mut report = UnstickReport::empty("text");
        report.findings.push(UnstickFinding {
            kind: UnstickFindingKind::TodoComment,
            file: "test.rs".to_string(),
            line: 1,
            snippet: "// TODO".to_string(),
            suggestion: "fix it".to_string(),
        });
        report.files_scanned = 1;

        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["scanner"], "text");
        assert_eq!(json["files_scanned"], 1);
        assert_eq!(json["findings"][0]["kind"], "todo_comment");
        assert_eq!(json["findings"][0]["file"], "test.rs");
        assert_eq!(json["findings"][0]["line"], 1);
    }

    #[test]
    fn truncate_snippet_short_passthrough() {
        assert_eq!(truncate_snippet("short", 80), "short");
        assert_eq!(truncate_snippet("  padded  ", 80), "padded");
    }

    #[test]
    fn truncate_snippet_long_truncated() {
        let long = "a".repeat(100);
        let result = truncate_snippet(&long, 20);
        assert!(result.len() <= 20);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn truncate_snippet_small_limit_stays_bounded() {
        let long = "abcdef";
        assert_eq!(truncate_snippet(long, 0), "");
        assert_eq!(truncate_snippet(long, 1), ".");
        assert_eq!(truncate_snippet(long, 2), "..");
        assert_eq!(truncate_snippet(long, 3), "...");
    }

    #[test]
    fn truncate_snippet_unicode_boundary_safe() {
        let emoji = "😀😀😀😀";
        let result = truncate_snippet(emoji, 7);
        assert_eq!(result, "😀...");
        assert!(result.len() <= 7);
    }

    #[test]
    fn scan_file_text_finds_todo() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "fn main() {\n    // TODO: fix this\n}\n").expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 10, &mut kind_counts);

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, UnstickFindingKind::TodoComment);
        assert_eq!(findings[0].file, "test.rs");
        assert_eq!(findings[0].line, 2);
        assert!(findings[0].snippet.contains("TODO"));
    }

    #[test]
    fn scan_file_text_finds_unwrap() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "let x = foo.unwrap();\n").expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 10, &mut kind_counts);

        assert!(
            findings
                .iter()
                .any(|f| f.kind == UnstickFindingKind::PanicSite)
        );
    }

    #[test]
    fn scan_file_text_finds_suppressed_error() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        std::fs::write(&file_path, "let _ = some_fallible_call()?;\n").expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 10, &mut kind_counts);

        assert!(
            findings
                .iter()
                .any(|f| f.kind == UnstickFindingKind::SuppressedError)
        );
    }

    #[test]
    fn scan_file_text_respects_max_per_kind() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let file_path = dir.path().join("test.rs");
        let content = (0..20)
            .map(|i| format!("// TODO: item {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&file_path, content).expect("write file");

        let patterns = TextScanPatterns::new();
        let mut kind_counts = std::collections::HashMap::new();

        let findings = scan_file_text(&file_path, dir.path(), &patterns, 3, &mut kind_counts);

        assert_eq!(findings.len(), 3); // capped at max_per_kind
    }

    #[test]
    fn run_unstick_scan_text_on_fixture_dir() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create a small fixture tree
        std::fs::write(
            dir.path().join("main.rs"),
            "fn main() {\n    // TODO: implement\n    let x = foo.unwrap();\n}\n",
        )
        .expect("write");
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn bar() {\n    // FIXME: clean up\n}\n",
        )
        .expect("write");
        // Non-matching extension should be skipped
        std::fs::write(
            dir.path().join("notes.txt"),
            "TODO: this should be skipped\n",
        )
        .expect("write");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            max_findings_per_kind: 10,
            max_total_findings: 25,
            extensions: vec!["rs".to_string()],
        };

        let report = run_unstick_scan_text(&config);

        assert_eq!(report.scanner, "text");
        assert_eq!(report.files_scanned, 2);
        assert!(!report.truncated);
        // Should find: 2 TODOs + 1 unwrap = 3 findings minimum
        assert!(report.total_findings() >= 3);
        // .txt file should not contribute findings
        assert!(report.findings.iter().all(|f| {
            std::path::Path::new(&f.file)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
        }));
    }

    #[test]
    fn run_unstick_scan_text_skips_hidden_and_target() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create dirs that should be skipped
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).expect("mkdir");
        std::fs::write(hidden.join("secret.rs"), "// TODO: hidden").expect("write");

        let target = dir.path().join("target");
        std::fs::create_dir(&target).expect("mkdir");
        std::fs::write(target.join("built.rs"), "// TODO: target").expect("write");

        // Create a file that should be scanned
        std::fs::write(dir.path().join("src.rs"), "// TODO: visible").expect("write");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            ..UnstickConfig::default()
        };

        let report = run_unstick_scan_text(&config);

        assert_eq!(report.files_scanned, 1);
        assert_eq!(report.total_findings(), 1);
        assert_eq!(report.findings[0].file, "src.rs");
    }

    #[test]
    fn run_unstick_scan_text_truncates_at_max_total() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create many TODO lines
        let content = (0..50)
            .map(|i| format!("// TODO: item {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.path().join("big.rs"), content).expect("write");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            max_findings_per_kind: 100, // high per-kind limit
            max_total_findings: 5,      // low total limit
            extensions: vec!["rs".to_string()],
        };

        let report = run_unstick_scan_text(&config);

        assert!(report.total_findings() <= 5);
        assert!(report.truncated);
    }

    #[test]
    fn run_unstick_scan_text_empty_dir() {
        let dir = tempfile::tempdir().expect("create temp dir");

        let config = UnstickConfig {
            root: dir.path().to_path_buf(),
            ..UnstickConfig::default()
        };

        let report = run_unstick_scan_text(&config);
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.files_scanned, 0);
        assert!(!report.truncated);
    }

    #[test]
    fn run_unstick_scan_text_nonexistent_dir() {
        let config = UnstickConfig {
            root: std::path::PathBuf::from("/nonexistent/path/does/not/exist"),
            ..UnstickConfig::default()
        };

        let report = run_unstick_scan_text(&config);
        assert_eq!(report.total_findings(), 0);
        assert_eq!(report.files_scanned, 0);
    }

    #[test]
    fn unstick_human_summary_with_many_findings() {
        let mut report = UnstickReport::empty("text");
        for i in 0..15 {
            report.findings.push(UnstickFinding {
                kind: UnstickFindingKind::TodoComment,
                file: format!("file{i}.rs"),
                line: i as u32 + 1,
                snippet: format!("// TODO: item {i}"),
                suggestion: "fix it".to_string(),
            });
        }
        report.files_scanned = 15;
        report.counts.insert("TODO/FIXME".to_string(), 15);

        let summary = report.human_summary();
        assert!(summary.contains("Found 15 items"));
        assert!(summary.contains("... and 5 more"));
    }

    // ========================================================================
    // WorkflowRunner Tests
    // ========================================================================

    #[test]
    fn workflow_runner_config_default_has_sensible_values() {
        let config = WorkflowRunnerConfig::default();

        assert!(config.max_concurrent > 0);
        assert!(config.step_timeout_ms > 0);
        assert!(config.retry_backoff_multiplier >= 1.0);
        assert!(config.max_retries_per_step > 0);
    }

    #[test]
    fn workflow_start_result_variants_serialize() {
        let variants = vec![
            WorkflowStartResult::Started {
                execution_id: "exec-001".to_string(),
                workflow_name: "test_workflow".to_string(),
            },
            WorkflowStartResult::NoMatchingWorkflow {
                rule_id: "test.rule".to_string(),
            },
            WorkflowStartResult::PaneLocked {
                pane_id: 42,
                held_by_workflow: "other_workflow".to_string(),
                held_by_execution: "exec-002".to_string(),
            },
            WorkflowStartResult::Error {
                error: "Something went wrong".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: WorkflowStartResult = serde_json::from_str(&json).unwrap();

            // Verify round-trip
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn workflow_execution_result_variants_serialize() {
        let variants = vec![
            WorkflowExecutionResult::Completed {
                execution_id: "exec-001".to_string(),
                result: serde_json::json!({"success": true}),
                elapsed_ms: 1000,
                steps_executed: 3,
            },
            WorkflowExecutionResult::Aborted {
                execution_id: "exec-002".to_string(),
                reason: "Timeout exceeded".to_string(),
                step_index: 2,
                elapsed_ms: 5000,
            },
            WorkflowExecutionResult::PolicyDenied {
                execution_id: "exec-003".to_string(),
                step_index: 1,
                reason: "Rate limit exceeded".to_string(),
            },
            WorkflowExecutionResult::Error {
                execution_id: Some("exec-004".to_string()),
                error: "Database connection failed".to_string(),
            },
            WorkflowExecutionResult::Error {
                execution_id: None,
                error: "Early failure".to_string(),
            },
        ];

        for variant in variants {
            let json = serde_json::to_string(&variant).unwrap();
            let parsed: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();

            // Verify round-trip
            let json2 = serde_json::to_string(&parsed).unwrap();
            assert_eq!(json, json2);
        }
    }

    #[test]
    fn workflow_start_result_accessors_work() {
        let started = WorkflowStartResult::Started {
            execution_id: "exec-001".to_string(),
            workflow_name: "test".to_string(),
        };
        assert!(started.is_started());
        assert!(!started.is_locked());
        assert!(started.execution_id().is_some());

        let locked = WorkflowStartResult::PaneLocked {
            pane_id: 1,
            held_by_workflow: "other".to_string(),
            held_by_execution: "exec-002".to_string(),
        };
        assert!(!locked.is_started());
        assert!(locked.is_locked());
        assert!(locked.execution_id().is_none());

        let no_match = WorkflowStartResult::NoMatchingWorkflow {
            rule_id: "test".to_string(),
        };
        assert!(!no_match.is_started());
        assert!(!no_match.is_locked());
        assert!(no_match.execution_id().is_none());

        let error = WorkflowStartResult::Error {
            error: "fail".to_string(),
        };
        assert!(!error.is_started());
        assert!(!error.is_locked());
        assert!(error.execution_id().is_none());
    }

    #[test]
    fn workflow_execution_result_accessors_work() {
        let completed = WorkflowExecutionResult::Completed {
            execution_id: "exec-001".to_string(),
            result: serde_json::Value::Null,
            elapsed_ms: 100,
            steps_executed: 2,
        };
        assert!(completed.is_completed());
        assert!(!completed.is_aborted());
        assert_eq!(completed.execution_id(), Some("exec-001"));

        let aborted = WorkflowExecutionResult::Aborted {
            execution_id: "exec-002".to_string(),
            reason: "test".to_string(),
            step_index: 1,
            elapsed_ms: 50,
        };
        assert!(!aborted.is_completed());
        assert!(aborted.is_aborted());
        assert_eq!(aborted.execution_id(), Some("exec-002"));

        let denied = WorkflowExecutionResult::PolicyDenied {
            execution_id: "exec-003".to_string(),
            step_index: 0,
            reason: "rate limit".to_string(),
        };
        assert!(!denied.is_completed());
        assert!(!denied.is_aborted());
        assert_eq!(denied.execution_id(), Some("exec-003"));

        let error_with_id = WorkflowExecutionResult::Error {
            execution_id: Some("exec-004".to_string()),
            error: "fail".to_string(),
        };
        assert!(!error_with_id.is_completed());
        assert!(!error_with_id.is_aborted());
        assert_eq!(error_with_id.execution_id(), Some("exec-004"));

        let error_no_id = WorkflowExecutionResult::Error {
            execution_id: None,
            error: "fail".to_string(),
        };
        assert!(error_no_id.execution_id().is_none());
    }

    // ========================================================================
    // Workflow Selection Tests
    // ========================================================================

    /// Test workflow that handles compaction patterns.
    struct MockCompactionWorkflow;

    impl Workflow for MockCompactionWorkflow {
        fn name(&self) -> &'static str {
            "handle_compaction"
        }

        fn description(&self) -> &'static str {
            "Mock workflow for compaction handling"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("compaction")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("notify", "Send notification")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done_empty(),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test workflow that handles usage limit patterns.
    struct MockUsageLimitWorkflow;

    impl Workflow for MockUsageLimitWorkflow {
        fn name(&self) -> &'static str {
            "handle_usage_limit"
        }

        fn description(&self) -> &'static str {
            "Mock workflow for usage limit handling"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("usage")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("warn", "Send warning")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done_empty(),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test that find_matching_workflow returns the correct workflow for a detection.
    #[test]
    fn workflow_runner_selects_correct_workflow_for_compaction() {
        // Create runner with multiple registered workflows
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());

        // Create mock injector (won't be called in this test)
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                default_wezterm_handle(),
            ),
        ));

        // Create a minimal storage handle using temp file
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        // Register workflows
        runner.register_workflow(Arc::new(MockCompactionWorkflow));
        runner.register_workflow(Arc::new(MockUsageLimitWorkflow));

        // Create compaction detection
        let compaction_detection = Detection {
            rule_id: "claude.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            matched_text: "Auto-compact: compacted".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Should find compaction workflow
        let workflow = runner.find_matching_workflow(&compaction_detection);
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_compaction");

        // Create usage detection
        let usage_detection = Detection {
            rule_id: "codex.usage.warning".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage_warning".to_string(),
            severity: Severity::Info,
            confidence: 0.8,
            matched_text: "less than 25%".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Should find usage limit workflow
        let workflow = runner.find_matching_workflow(&usage_detection);
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_usage_limit");

        // Create unmatched detection
        let unmatched_detection = Detection {
            rule_id: "unknown.pattern".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "unknown".to_string(),
            severity: Severity::Info,
            confidence: 0.5,
            matched_text: "something".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Should not find any workflow
        let workflow = runner.find_matching_workflow(&unmatched_detection);
        assert!(workflow.is_none());

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    /// Test that pane locks prevent concurrent workflow executions.
    #[test]
    fn workflow_runner_lock_prevents_concurrent_runs() {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                default_wezterm_handle(),
            ),
        ));

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        runner.register_workflow(Arc::new(MockCompactionWorkflow));

        let pane_id = 42u64;
        let detection = Detection {
            rule_id: "claude.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "compaction".to_string(),
            severity: Severity::Warning,
            confidence: 0.9,
            matched_text: "compacted".to_string(),
            extracted: serde_json::json!({}),
            span: (0, 0),
        };

        // Create test pane first
        rt.block_on(async {
            let pane = crate::storage::PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: Some(1),
                tab_id: Some(1),
                title: Some("test".to_string()),
                cwd: Some("/tmp".to_string()),
                tty_name: None,
                first_seen_at: now_ms(),
                last_seen_at: now_ms(),
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            };
            storage.upsert_pane(pane).await.unwrap();
        });

        // First handle_detection should start
        let result1 = rt.block_on(runner.handle_detection(pane_id, &detection, None));
        assert!(result1.is_started());

        // Second handle_detection should be blocked by lock
        let result2 = rt.block_on(runner.handle_detection(pane_id, &detection, None));
        assert!(result2.is_locked());

        // Verify the lock info
        if let WorkflowStartResult::PaneLocked {
            held_by_workflow, ..
        } = result2
        {
            assert_eq!(held_by_workflow, "handle_compaction");
        }

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    /// Test that find_workflow_by_name works correctly.
    #[test]
    fn workflow_runner_find_by_name() {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                default_wezterm_handle(),
            ),
        ));

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();
        let storage = rt.block_on(async {
            Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap())
        });

        let runner = WorkflowRunner::new(
            engine,
            lock_manager,
            storage.clone(),
            injector,
            WorkflowRunnerConfig::default(),
        );

        runner.register_workflow(Arc::new(MockCompactionWorkflow));
        runner.register_workflow(Arc::new(MockUsageLimitWorkflow));

        // Find by name
        let workflow = runner.find_workflow_by_name("handle_compaction");
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_compaction");

        let workflow = runner.find_workflow_by_name("handle_usage_limit");
        assert!(workflow.is_some());
        assert_eq!(workflow.unwrap().name(), "handle_usage_limit");

        // Not found
        let workflow = runner.find_workflow_by_name("nonexistent");
        assert!(workflow.is_none());

        // Cleanup
        rt.block_on(async { storage.shutdown().await.unwrap() });
    }

    // ========================================================================
    // Policy Denial Tests (wa-nu4.1.1.5)
    // ========================================================================

    /// Test workflow that attempts to send text and checks policy result.
    /// Kept for future integration tests that need to test workflow execution with policy gates.
    #[allow(dead_code)]
    struct MockTextSendingWorkflow;

    impl Workflow for MockTextSendingWorkflow {
        fn name(&self) -> &'static str {
            "text_sender"
        }

        fn description(&self) -> &'static str {
            "Mock workflow that sends text to test policy gates"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("text_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send_text", "Send text to terminal")]
        }

        fn execute_step(
            &self,
            ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            // Need to move ctx reference into the async block properly
            let pane_id = ctx.pane_id();
            let has_injector = ctx.has_injector();
            let execution_id = ctx.execution_id().to_string();

            // Clone capabilities for use in async block
            let capabilities = ctx.capabilities().clone();

            Box::pin(async move {
                match step_idx {
                    0 => {
                        // Try to send text - policy should deny if command is running
                        if !has_injector {
                            return StepResult::abort("No injector configured");
                        }
                        // We need access to the context to call send_text
                        // This is a limitation of the mock - we'll test via the policy directly
                        StepResult::done(serde_json::json!({
                            "pane_id": pane_id,
                            "has_injector": has_injector,
                            "execution_id": execution_id,
                            "prompt_active": capabilities.prompt_active,
                            "command_running": capabilities.command_running,
                        }))
                    }
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Test that policy denial is properly returned when sending to a running command.
    #[test]
    fn policy_denies_send_when_command_running() {
        use crate::policy::{
            ActionKind, ActorKind, PaneCapabilities, PolicyDecision, PolicyEngine, PolicyInput,
        };

        // Create a strict policy engine (requires prompt active)
        let mut engine = PolicyEngine::strict();

        // Create capabilities where command is running (not at prompt)
        let caps = PaneCapabilities::running();

        // Try to authorize a send - should be denied
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(42)
            .with_capabilities(caps)
            .with_text_summary("test command")
            .with_workflow("wf-test-001");

        let decision = engine.authorize(&input);

        // Verify it's denied with the expected reason
        match decision {
            PolicyDecision::Deny {
                reason, rule_id, ..
            } => {
                assert!(
                    reason.contains("running command") || reason.contains("wait for prompt"),
                    "Expected denial reason about running command, got: {reason}"
                );
                assert_eq!(rule_id, Some("policy.prompt_required".to_string()));
            }
            other => panic!("Expected Deny, got: {other:?}"),
        }
    }

    /// Test that InjectionResult::Denied is returned when policy denies.
    #[tokio::test]
    async fn policy_gated_injector_returns_denied_for_running_command() {
        use crate::policy::{
            ActorKind, InjectionResult, PaneCapabilities, PolicyEngine, PolicyGatedInjector,
        };

        // Create a strict policy engine (requires prompt active)
        let engine = PolicyEngine::strict();
        let client = default_wezterm_handle();
        let mut injector = PolicyGatedInjector::new(engine, client);

        // Create capabilities where command is running (not at prompt)
        let caps = PaneCapabilities::running();

        // Try to send text - should be denied by policy
        let result = injector
            .send_text(
                42,
                "echo test",
                ActorKind::Workflow,
                &caps,
                Some("wf-test-002"),
            )
            .await;

        // Verify it's denied
        assert!(
            result.is_denied(),
            "Expected denied result, got: {result:?}"
        );

        // Verify the rule ID
        if let InjectionResult::Denied { decision, .. } = result {
            assert_eq!(
                decision.rule_id(),
                Some("policy.prompt_required"),
                "Expected policy.prompt_required rule, got: {:?}",
                decision.rule_id()
            );
        }
    }

    /// Test that WorkflowContext has injector access after with_injector is called.
    #[test]
    fn workflow_context_injector_access() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create context without injector
            let ctx =
                WorkflowContext::new(storage.clone(), 42, PaneCapabilities::default(), "exec-001");
            assert!(!ctx.has_injector());

            // Create context with injector
            let engine = crate::policy::PolicyEngine::permissive();
            let client = default_wezterm_handle();
            let injector = Arc::new(crate::runtime_compat::Mutex::new(
                crate::policy::PolicyGatedInjector::new(engine, client),
            ));

            let ctx_with_injector =
                WorkflowContext::new(storage.clone(), 42, PaneCapabilities::default(), "exec-002")
                    .with_injector(injector);

            assert!(ctx_with_injector.has_injector());

            storage.shutdown().await.unwrap();
        });
    }

    // ========================================================================
    // HandleCompaction Workflow Tests (wa-nu4.1.2.1)
    // ========================================================================

    #[test]
    fn handle_compaction_metadata() {
        let workflow = HandleCompaction::new();

        assert_eq!(workflow.name(), "handle_compaction");
        assert_eq!(
            workflow.description(),
            "Re-inject critical context (AGENTS.md) after conversation compaction"
        );

        let steps = workflow.steps();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "stabilize");
        assert_eq!(steps[2].name, "send_prompt");
        assert_eq!(steps[3].name, "verify_send");
    }

    #[test]
    fn handle_compaction_handles_compaction_events() {
        let workflow = HandleCompaction::new();

        // Should handle event_type "session.compaction"
        let detection_event_type = Detection {
            rule_id: "something.other".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.compaction".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&detection_event_type));

        // Should handle rule_id containing "compaction"
        let detection_rule_id = Detection {
            rule_id: "claude_code.compaction".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "other".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        assert!(workflow.handles(&detection_rule_id));

        // Should NOT handle unrelated detections
        let detection_unrelated = Detection {
            rule_id: "prompt.ready".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "prompt".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
            span: (0, 0),
        };
        assert!(!workflow.handles(&detection_unrelated));
    }

    #[test]
    fn handle_compaction_guard_checks() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_guards.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Normal capabilities - should pass guards
            let normal_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_normal =
                WorkflowContext::new(storage.clone(), 42, normal_caps, "exec-guard-normal");
            let result = HandleCompaction::check_pane_guards(&ctx_normal);
            assert!(result.is_ok(), "Normal state should pass guards");

            // Alt-screen active - should fail
            let alt_caps = PaneCapabilities {
                alt_screen: Some(true),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_alt = WorkflowContext::new(storage.clone(), 42, alt_caps, "exec-guard-alt");
            let result = HandleCompaction::check_pane_guards(&ctx_alt);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("alt-screen"));

            // Command running - should fail
            let cmd_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: true,
                has_recent_gap: false,
                ..Default::default()
            };
            let ctx_cmd = WorkflowContext::new(storage.clone(), 42, cmd_caps, "exec-guard-cmd");
            let result = HandleCompaction::check_pane_guards(&ctx_cmd);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("running"));

            // Recent gap - should fail
            let gap_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: true,
                ..Default::default()
            };
            let ctx_gap = WorkflowContext::new(storage.clone(), 42, gap_caps, "exec-guard-gap");
            let result = HandleCompaction::check_pane_guards(&ctx_gap);
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("gap"));

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn handle_compaction_prompts_exist() {
        // Verify all agent-specific prompts are non-empty
        assert!(!compaction_prompts::CLAUDE_CODE.is_empty());
        assert!(!compaction_prompts::CODEX.is_empty());
        assert!(!compaction_prompts::GEMINI.is_empty());
        assert!(!compaction_prompts::UNKNOWN.is_empty());

        // Verify they contain AGENTS.md reference (key context file)
        assert!(compaction_prompts::CLAUDE_CODE.contains("AGENTS.md"));
        assert!(compaction_prompts::CODEX.contains("AGENTS.md"));
        assert!(compaction_prompts::GEMINI.contains("AGENTS.md"));
    }

    #[test]
    fn handle_compaction_builder_pattern() {
        let workflow = HandleCompaction::new()
            .with_stabilization_ms(5000)
            .with_idle_timeout_ms(60_000);

        assert_eq!(workflow.stabilization_ms, 5000);
        assert_eq!(workflow.idle_timeout_ms, 60_000);
    }

    #[test]
    fn handle_compaction_default_values() {
        let workflow = HandleCompaction::default();

        // Defaults should be reasonable values
        assert!(workflow.stabilization_ms > 0);
        assert!(workflow.idle_timeout_ms > 0);
        assert!(workflow.idle_timeout_ms > workflow.stabilization_ms);
    }

    // ========================================================================
    // HandleCompaction Integration Tests (wa-nu4.1.2.4)
    // ========================================================================
    //
    // These tests verify the full workflow execution path with synthetic
    // detections and various pane states.

    /// Test: Synthetic compaction detection + PromptActive state
    /// Expected: Workflow proceeds through guards → step logs show completion path
    #[test]
    fn handle_compaction_integration_prompt_active_passes_guards() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_integration.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create PromptActive capabilities (pane is ready for input)
            let prompt_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-prompt-active-001";
            let pane_id = 42u64;

            // Create context with PromptActive state
            let ctx =
                WorkflowContext::new(storage.clone(), pane_id, prompt_caps.clone(), execution_id);

            // Verify guards pass for PromptActive state
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_ok(),
                "Guard check should pass for PromptActive state, got: {:?}",
                guard_result
            );

            // Create synthetic compaction detection
            let detection = Detection {
                rule_id: "claude_code.compaction".to_string(),
                agent_type: AgentType::ClaudeCode,
                event_type: "session.compaction".to_string(),
                severity: Severity::Info,
                confidence: 1.0,
                extracted: serde_json::json!({
                    "tokens_before": 150_000,
                    "tokens_after": 25_000
                }),
                matched_text: "Auto-compact: compacted 150,000 tokens to 25,000 tokens".to_string(),
                span: (0, 0),
            };

            // Verify HandleCompaction handles this detection
            let workflow = HandleCompaction::new();
            assert!(
                workflow.handles(&detection),
                "HandleCompaction should handle compaction detections"
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: AltScreen active state causes workflow abort
    /// Expected: Guard check fails with "alt-screen" in error message
    #[test]
    fn handle_compaction_integration_alt_screen_aborts() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_altscreen.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create AltScreen capabilities (vim, less, htop, etc.)
            let alt_screen_caps = PaneCapabilities {
                alt_screen: Some(true),
                command_running: false,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-altscreen-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, alt_screen_caps, execution_id);

            // Verify guards fail for AltScreen state
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail for AltScreen state"
            );

            // Verify error message is actionable (contains "alt-screen")
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("alt-screen"),
                "Error message should mention 'alt-screen' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Command running state causes workflow abort
    /// Expected: Guard check fails with "running" in error message
    #[test]
    fn handle_compaction_integration_command_running_aborts() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_cmdrunning.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create capabilities where command is running
            let running_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: true,
                has_recent_gap: false,
                ..Default::default()
            };

            let execution_id = "test-hc-running-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, running_caps, execution_id);

            // Verify guards fail
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail when command is running"
            );

            // Verify error message is actionable
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("running"),
                "Error message should mention 'running' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Recent gap state causes workflow abort
    /// Expected: Guard check fails with "gap" in error message
    #[test]
    fn handle_compaction_integration_recent_gap_aborts() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_hc_gap.db")
            .to_string_lossy()
            .to_string();

        rt.block_on(async {
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

            // Create capabilities with recent gap
            let gap_caps = PaneCapabilities {
                alt_screen: Some(false),
                command_running: false,
                has_recent_gap: true,
                ..Default::default()
            };

            let execution_id = "test-hc-gap-001";
            let pane_id = 42u64;

            let ctx = WorkflowContext::new(storage.clone(), pane_id, gap_caps, execution_id);

            // Verify guards fail
            let guard_result = HandleCompaction::check_pane_guards(&ctx);
            assert!(
                guard_result.is_err(),
                "Guard check should fail with recent gap"
            );

            // Verify error message is actionable
            let err = guard_result.unwrap_err();
            assert!(
                err.contains("gap"),
                "Error message should mention 'gap' for actionable diagnosis, got: {}",
                err
            );

            storage.shutdown().await.unwrap();
        });
    }

    /// Test: Verify step metadata is correct for handle_compaction
    /// Expected: Steps array contains check_guards, stabilize, send_prompt, verify_send
    #[test]
    fn handle_compaction_step_metadata_complete() {
        let workflow = HandleCompaction::new();
        let steps = workflow.steps();

        // Verify all expected steps are present
        assert_eq!(steps.len(), 4, "HandleCompaction should have 4 steps");

        // Verify step names are descriptive (for logging/debugging)
        let step_names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        assert!(
            step_names.contains(&"check_guards"),
            "Should have check_guards step"
        );
        assert!(
            step_names.contains(&"stabilize"),
            "Should have stabilize step"
        );
        assert!(
            step_names.contains(&"send_prompt"),
            "Should have send_prompt step"
        );
        assert!(
            step_names.contains(&"verify_send"),
            "Should have verify_send step"
        );

        // Verify step descriptions are non-empty (for actionable logging)
        for step in &steps {
            assert!(
                !step.description.is_empty(),
                "Step '{}' should have a description for actionable logging",
                step.name
            );
        }
    }

    /// Test: Agent-specific prompt selection is deterministic
    /// Expected: Each agent type gets a consistent, non-empty prompt
    #[test]
    fn handle_compaction_agent_prompt_selection_deterministic() {
        // Test each agent type gets a deterministic prompt
        let agents = vec![
            (AgentType::ClaudeCode, compaction_prompts::CLAUDE_CODE),
            (AgentType::Codex, compaction_prompts::CODEX),
            (AgentType::Gemini, compaction_prompts::GEMINI),
            (AgentType::Unknown, compaction_prompts::UNKNOWN),
        ];

        for (agent_type, expected_prompt) in agents {
            // Verify prompt is non-empty
            assert!(
                !expected_prompt.is_empty(),
                "Prompt for {:?} should not be empty",
                agent_type
            );

            // Verify prompt contains AGENTS.md reference (except Unknown)
            if agent_type != AgentType::Unknown {
                assert!(
                    expected_prompt.contains("AGENTS.md"),
                    "Prompt for {:?} should reference AGENTS.md",
                    agent_type
                );
            }

            // Verify prompt ends with newline (for clean send)
            assert!(
                expected_prompt.ends_with('\n'),
                "Prompt for {:?} should end with newline for clean send",
                agent_type
            );
        }
    }

    #[test]
    fn handle_compaction_prompt_precedence() {
        use crate::config::{CompactionPromptOverride, PaneFilterRule};
        use serde_json::json;

        let mut config = crate::config::CompactionPromptConfig::default();
        config.default = "default".to_string();
        config
            .by_agent
            .insert("codex".to_string(), "agent".to_string());
        config.by_pane.insert(7, "pane".to_string());
        config.by_project.push(CompactionPromptOverride {
            rule: PaneFilterRule::new("project_rule").with_cwd("/repo"),
            prompt: "project".to_string(),
        });

        let workflow = HandleCompaction::new().with_prompt_config(config);

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("prec.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let mut ctx =
                WorkflowContext::new(storage, 7, PaneCapabilities::default(), "exec-prec")
                    .with_trigger(json!({"agent_type": "codex"}));
            ctx.set_pane_meta(PaneMetadata {
                cwd: Some("/repo".to_string()),
                ..Default::default()
            });
            assert_eq!(workflow.resolve_prompt(&ctx), "pane");

            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("prec2.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let mut ctx =
                WorkflowContext::new(storage, 8, PaneCapabilities::default(), "exec-prec2")
                    .with_trigger(json!({"agent_type": "codex"}));
            ctx.set_pane_meta(PaneMetadata {
                cwd: Some("/repo".to_string()),
                ..Default::default()
            });
            assert_eq!(workflow.resolve_prompt(&ctx), "project");

            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("prec3.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let ctx = WorkflowContext::new(storage, 9, PaneCapabilities::default(), "exec-prec3")
                .with_trigger(json!({"agent_type": "codex"}));
            assert_eq!(workflow.resolve_prompt(&ctx), "agent");
        });
    }

    #[test]
    fn handle_compaction_prompt_bounds_and_redaction() {
        use serde_json::json;

        let mut config = crate::config::CompactionPromptConfig::default();
        config.max_prompt_len = 25;
        config.max_snippet_len = 16;
        config.by_agent.clear();
        config.default = "Prompt {{pane_cwd}}".to_string();

        let workflow = HandleCompaction::new().with_prompt_config(config);

        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let temp_dir = tempfile::tempdir().unwrap();
            let db_path = temp_dir
                .path()
                .join("bounds.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(StorageHandle::new(&db_path).await.unwrap());
            let mut ctx =
                WorkflowContext::new(storage, 5, PaneCapabilities::default(), "exec-bounds")
                    .with_trigger(json!({"agent_type": "codex"}));
            ctx.set_pane_meta(PaneMetadata {
                cwd: Some("sk-abc123456789012345678901234567890123456789012345678901".to_string()),
                ..Default::default()
            });

            let prompt = workflow.resolve_prompt(&ctx);
            assert!(prompt.len() <= 25);
            assert!(!prompt.contains("sk-abc"));
            assert!(prompt.contains("[REDACTED]"));
        });
    }

    /// Test: Workflow execution step 0 (check_guards) with PromptActive state
    /// Expected: Step returns Continue (not Abort)
    #[tokio::test]
    async fn handle_compaction_execute_step0_prompt_active_continues() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        // Create PromptActive capabilities
        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step0-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        // Step 0 should return Continue for valid state
        match result {
            StepResult::Continue => {
                // Success - guards passed
            }
            StepResult::Abort { reason } => {
                panic!("Step 0 should not abort for PromptActive state: {}", reason);
            }
            other => {
                panic!("Unexpected step result for step 0: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Workflow execution step 0 (check_guards) with AltScreen state
    /// Expected: Step returns Abort with actionable reason
    #[tokio::test]
    async fn handle_compaction_execute_step0_alt_screen_aborts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step0_alt.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        // Create AltScreen capabilities
        let alt_caps = PaneCapabilities {
            alt_screen: Some(true),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, alt_caps, "test-step0-alt-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 0).await;

        // Step 0 should abort for AltScreen
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("alt-screen"),
                    "Abort reason should mention 'alt-screen': {}",
                    reason
                );
            }
            StepResult::Continue => {
                panic!("Step 0 should abort for AltScreen state, got Continue");
            }
            other => {
                panic!(
                    "Unexpected step result for step 0 with AltScreen: {:?}",
                    other
                );
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Workflow execution step 1 (stabilize) returns Continue after stabilization
    /// Expected: Step returns Continue (no wait-for result)
    #[tokio::test]
    async fn handle_compaction_execute_step1_returns_continue() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step1.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        let mut ctx = WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step1-001");

        let workflow = HandleCompaction::new()
            .with_stabilization_ms(0)
            .with_idle_timeout_ms(50);
        let result = workflow.execute_step(&mut ctx, 1).await;

        // Step 1 should return Continue once stabilized
        match result {
            StepResult::Continue => {}
            StepResult::Abort { reason } => {
                panic!("Step 1 should not abort when stabilization is zero: {reason}");
            }
            other => panic!("Step 1 should return Continue, got: {:?}", other),
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Workflow execution step 2 (send_prompt) without injector
    /// Expected: Step returns Abort (no injector configured)
    #[tokio::test]
    async fn handle_compaction_execute_step2_no_injector_aborts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step2_no_inj.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities {
            alt_screen: Some(false),
            command_running: false,
            has_recent_gap: false,
            ..Default::default()
        };

        // Create context WITHOUT injector
        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-step2-no-inj-001");

        let workflow = HandleCompaction::new();
        let result = workflow.execute_step(&mut ctx, 2).await;

        // Step 2 should abort without injector
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.to_lowercase().contains("injector"),
                    "Abort reason should mention missing injector: {}",
                    reason
                );
            }
            other => {
                panic!("Step 2 should abort without injector, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    /// Test: Unexpected step index returns Abort
    /// Expected: Step indices >= step_count return Abort
    #[tokio::test]
    async fn handle_compaction_execute_invalid_step_aborts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_invalid_step.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());

        let prompt_caps = PaneCapabilities::default();

        let mut ctx =
            WorkflowContext::new(storage.clone(), 42, prompt_caps, "test-invalid-step-001");

        let workflow = HandleCompaction::new();

        // Try to execute step beyond the workflow's steps
        let invalid_step = workflow.step_count() + 1;
        let result = workflow.execute_step(&mut ctx, invalid_step).await;

        // Should abort for invalid step
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("step") || reason.contains("index"),
                    "Abort reason should mention invalid step: {}",
                    reason
                );
            }
            other => {
                panic!("Invalid step should abort, got: {:?}", other);
            }
        }

        storage.shutdown().await.unwrap();
    }

    // ========================================================================
    // Workflow Engine Tests (wa-nu4.1.1.7)
    // Lock Behavior, Step Logging, and Resume Tests
    // ========================================================================

    /// Simple workflow that completes after one step (for testing lock release on success)
    struct SimpleCompletingWorkflow;

    impl Workflow for SimpleCompletingWorkflow {
        fn name(&self) -> &'static str {
            "simple_completing"
        }

        fn description(&self) -> &'static str {
            "Test workflow that completes immediately"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("simple_complete")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("complete", "Complete immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done(serde_json::json!({"completed": true})),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Workflow that aborts after one step (for testing lock release on abort)
    struct AbortingWorkflow {
        abort_reason: String,
    }

    impl AbortingWorkflow {
        fn new(reason: &str) -> Self {
            Self {
                abort_reason: reason.to_string(),
            }
        }
    }

    impl Workflow for AbortingWorkflow {
        fn name(&self) -> &'static str {
            "aborting_workflow"
        }

        fn description(&self) -> &'static str {
            "Test workflow that aborts"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("abort_test")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("abort_step", "Abort immediately")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let reason = self.abort_reason.clone();
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::abort(&reason),
                    _ => StepResult::abort("Unexpected step"),
                }
            })
        }
    }

    /// Multi-step workflow for testing step logging and resume
    struct MultiStepWorkflow {
        fail_at_step: Option<usize>,
    }

    impl MultiStepWorkflow {
        fn new() -> Self {
            Self { fail_at_step: None }
        }

        fn failing_at(step: usize) -> Self {
            Self {
                fail_at_step: Some(step),
            }
        }
    }

    impl Workflow for MultiStepWorkflow {
        fn name(&self) -> &'static str {
            "multi_step"
        }

        fn description(&self) -> &'static str {
            "Test workflow with multiple steps"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("multi_step")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_0", "First step"),
                WorkflowStep::new("step_1", "Second step"),
                WorkflowStep::new("step_2", "Third step"),
                WorkflowStep::new("step_3", "Final step"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let fail_at = self.fail_at_step;
            Box::pin(async move {
                if Some(step_idx) == fail_at {
                    return StepResult::abort("Simulated failure");
                }
                match step_idx {
                    0 | 1 | 2 => StepResult::cont(),
                    3 => StepResult::done(serde_json::json!({"steps_completed": 4})),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Workflow with a single idempotent SendText step.
    struct IdempotentSendWorkflow;

    impl Workflow for IdempotentSendWorkflow {
        fn name(&self) -> &'static str {
            "idempotent_send"
        }

        fn description(&self) -> &'static str {
            "Test workflow with idempotent send step"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("idempotent_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send", "Send test text")]
        }

        fn to_action_plan(
            &self,
            ctx: &WorkflowContext,
            execution_id: &str,
        ) -> Option<crate::plan::ActionPlan> {
            let pane_id = ctx.pane_id();
            let step = crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::SendText {
                    pane_id,
                    text: "hello".to_string(),
                    paste_mode: None,
                },
                "Send test text",
            )
            .idempotent();

            Some(
                crate::plan::ActionPlan::builder(self.description(), "test-workspace")
                    .add_step(step)
                    .metadata(serde_json::json!({
                        "workflow_name": self.name(),
                        "execution_id": execution_id,
                        "pane_id": pane_id,
                    }))
                    .created_at(now_ms())
                    .build(),
            )
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::send_text("hello"),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Workflow that sets prompt capabilities before sending text.
    struct PromptSendWorkflow;

    impl Workflow for PromptSendWorkflow {
        fn name(&self) -> &'static str {
            "prompt_send"
        }

        fn description(&self) -> &'static str {
            "Test workflow that sets prompt capabilities before SendText"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.contains("prompt_send")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("send", "Send test text")]
        }

        fn execute_step(
            &self,
            ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            if step_idx == 0 {
                ctx.update_capabilities(PaneCapabilities::prompt());
            }
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::send_text("hello"),
                    _ => StepResult::abort("Unexpected step index"),
                }
            })
        }
    }

    /// Helper to create a test WorkflowRunner with storage.
    /// Uses a mock WezTerm handle with the given pane IDs pre-registered
    /// so that SendText steps succeed without a real WezTerm binary.
    async fn create_test_runner(
        db_path: &str,
    ) -> (
        WorkflowRunner,
        Arc<crate::storage::StorageHandle>,
        Arc<PaneWorkflowLockManager>,
    ) {
        create_test_runner_with_panes(db_path, &[]).await
    }

    /// Like `create_test_runner` but pre-registers the given pane IDs in the
    /// mock WezTerm backend so that SendText steps succeed for those panes.
    async fn create_test_runner_with_panes(
        db_path: &str,
        pane_ids: &[u64],
    ) -> (
        WorkflowRunner,
        Arc<crate::storage::StorageHandle>,
        Arc<PaneWorkflowLockManager>,
    ) {
        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(db_path).await.unwrap());

        let mock = crate::wezterm::MockWezterm::new();
        for &pid in pane_ids {
            mock.add_default_pane(pid).await;
        }
        let handle: crate::wezterm::WeztermHandle = Arc::new(mock);

        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        (runner, storage, lock_manager)
    }

    /// Helper to create a test pane in storage
    async fn create_test_pane(storage: &crate::storage::StorageHandle, pane_id: u64) {
        let pane = crate::storage::PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some("test".to_string()),
            cwd: Some("/tmp".to_string()),
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        storage.upsert_pane(pane).await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Lock Release Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: Lock is released when workflow completes successfully (Done)
    #[tokio::test]
    async fn lock_released_on_workflow_completion() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_complete.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 42u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(SimpleCompletingWorkflow));

        // Start workflow - acquires lock
        let detection = make_test_detection("simple_complete.test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started(), "Workflow should start");

        // Lock should be held
        assert!(
            lock_manager.is_locked(pane_id).is_some(),
            "Lock should be held after starting workflow"
        );

        // Run the workflow to completion
        let workflow = runner.find_workflow_by_name("simple_completing").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        // Verify workflow completed
        assert!(
            exec_result.is_completed(),
            "Workflow should complete successfully"
        );

        // Lock should be released after completion
        assert!(
            lock_manager.is_locked(pane_id).is_none(),
            "Lock should be released after workflow completion"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: Lock is released when workflow aborts
    #[tokio::test]
    async fn lock_released_on_workflow_abort() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_abort.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 43u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(AbortingWorkflow::new("Test abort reason")));

        // Start workflow - acquires lock
        let detection = make_test_detection("abort_test.trigger");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started(), "Workflow should start");

        // Lock should be held
        assert!(
            lock_manager.is_locked(pane_id).is_some(),
            "Lock should be held after starting workflow"
        );

        // Run the workflow (will abort)
        let workflow = runner.find_workflow_by_name("aborting_workflow").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        // Verify workflow aborted
        assert!(exec_result.is_aborted(), "Workflow should abort");
        if let WorkflowExecutionResult::Aborted { reason, .. } = &exec_result {
            assert!(
                reason.contains("Test abort reason"),
                "Abort should have expected reason"
            );
        }

        // Lock should be released after abort
        assert!(
            lock_manager.is_locked(pane_id).is_none(),
            "Lock should be released after workflow abort"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: Per-pane lock prevents concurrent workflow execution
    #[tokio::test]
    async fn per_pane_lock_prevents_concurrent_workflows() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_lock_concurrent.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 44u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start first workflow
        let detection1 = make_test_detection("multi_step.first");
        let start_result1 = runner.handle_detection(pane_id, &detection1, None).await;
        assert!(start_result1.is_started(), "First workflow should start");

        // Verify lock is held by first workflow
        let lock_info = lock_manager.is_locked(pane_id);
        assert!(lock_info.is_some(), "Lock should be held");
        let info = lock_info.unwrap();
        assert_eq!(info.workflow_name, "multi_step");

        // Try to start second workflow on same pane
        let detection2 = make_test_detection("multi_step.second");
        let start_result2 = runner.handle_detection(pane_id, &detection2, None).await;

        // Second workflow should be blocked
        assert!(
            start_result2.is_locked(),
            "Second workflow should be blocked by lock"
        );
        if let WorkflowStartResult::PaneLocked {
            held_by_workflow, ..
        } = start_result2
        {
            assert_eq!(
                held_by_workflow, "multi_step",
                "Lock should be held by first workflow"
            );
        }

        // Complete first workflow to release lock
        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_id = start_result1.execution_id().unwrap();
        let _ = runner
            .run_workflow(pane_id, workflow.clone(), exec_id, 0)
            .await;

        // Now second workflow can start
        let start_result3 = runner.handle_detection(pane_id, &detection2, None).await;
        assert!(
            start_result3.is_started(),
            "Workflow should start after lock released"
        );

        storage.shutdown().await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Step Logging Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: Step logs are written correctly during workflow execution
    #[tokio::test]
    async fn step_logs_written_correctly() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step_logs.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 45u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start and run workflow
        let detection = make_test_detection("multi_step.log_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(
            exec_result.is_completed(),
            "Workflow should complete: {exec_result:?}"
        );

        // Verify step logs were written
        let step_logs = storage.get_step_logs(execution_id).await.unwrap();

        // Multi-step workflow has 4 steps (0, 1, 2, 3)
        assert_eq!(step_logs.len(), 4, "Should have 4 step log entries");

        // Verify each step log
        for (i, log) in step_logs.iter().enumerate() {
            assert_eq!(log.workflow_id, execution_id);
            assert_eq!(log.step_index, i);
            assert_eq!(log.step_name, format!("step_{i}"));
            assert!(log.started_at > 0, "Started timestamp should be set");
            assert!(log.completed_at >= log.started_at, "Completed >= started");
            assert!(log.duration_ms >= 0, "Duration should be non-negative");
        }

        // First 3 steps should be "continue", last should be "done"
        assert_eq!(step_logs[0].result_type, "continue");
        assert_eq!(step_logs[1].result_type, "continue");
        assert_eq!(step_logs[2].result_type, "continue");
        assert_eq!(step_logs[3].result_type, "done");

        storage.shutdown().await.unwrap();
    }

    /// Test: SendText step logs capture audit_action_id and join into action_history
    #[tokio::test]
    async fn send_text_step_logs_audit_action_id() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_send_text_audit.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let wezterm: crate::wezterm::WeztermHandle = Arc::new(MockWezterm::default());
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::with_storage(
                crate::policy::PolicyEngine::permissive(),
                wezterm,
                storage.as_ref().clone(),
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        let pane_id = 60u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(PromptSendWorkflow));

        let detection = make_test_detection("prompt_send.audit");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("prompt_send").unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;
        assert!(
            exec_result.is_completed(),
            "Workflow should complete: {exec_result:?}"
        );

        let step_logs = storage.get_step_logs(execution_id).await.unwrap();
        let send_log = step_logs
            .iter()
            .find(|log| log.step_name == "send")
            .expect("send_text step log missing");
        let audit_action_id = send_log.audit_action_id.expect("audit_action_id missing");

        let history = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("send_text".to_string()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        let entry = history
            .iter()
            .find(|row| row.id == audit_action_id)
            .expect("action_history entry missing");

        assert_eq!(entry.workflow_id.as_deref(), Some(execution_id));
        assert_eq!(entry.step_name.as_deref(), Some("send"));

        storage.shutdown().await.unwrap();
    }

    /// Test: workflow completion updates undo metadata and records workflow actions.
    #[tokio::test]
    async fn workflow_completion_updates_undo_metadata() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_workflow_completion_audit.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 61u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        let detection = make_test_detection("multi_step.audit");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;
        assert!(
            exec_result.is_completed(),
            "Workflow should complete: {exec_result:?}"
        );

        let start_actions = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_start".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        let start = start_actions
            .first()
            .expect("workflow_start action missing");
        assert_eq!(start.undoable, Some(false));
        assert_eq!(start.undo_strategy.as_deref(), Some("workflow_abort"));
        assert_eq!(
            start.undo_hint.as_deref(),
            Some("workflow no longer running")
        );

        let completed = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_completed".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!completed.is_empty(), "workflow_completed action missing");

        let steps = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_step".to_string()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            steps
                .iter()
                .any(|row| row.step_name.as_deref() == Some("step_0")),
            "workflow_step entries should include step_name"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: workflow abort updates undo metadata and records abort action.
    #[tokio::test]
    async fn workflow_abort_updates_undo_metadata() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_workflow_abort_audit.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 62u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::failing_at(2)));

        let detection = make_test_detection("multi_step.abort_audit");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;
        assert!(
            exec_result.is_aborted(),
            "Workflow should abort: {exec_result:?}"
        );

        let start_actions = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_start".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        let start = start_actions
            .first()
            .expect("workflow_start action missing");
        assert_eq!(start.undoable, Some(false));
        assert_eq!(start.undo_strategy.as_deref(), Some("workflow_abort"));
        assert_eq!(
            start.undo_hint.as_deref(),
            Some("workflow no longer running")
        );

        let aborted = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_aborted".to_string()),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!aborted.is_empty(), "workflow_aborted action missing");

        let steps = storage
            .get_action_history(crate::storage::ActionHistoryQuery {
                actor_id: Some(execution_id.to_string()),
                action_kind: Some("workflow_step".to_string()),
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            steps.iter().any(|row| {
                row.input_summary.as_ref().is_some_and(|summary| {
                    serde_json::from_str::<serde_json::Value>(summary)
                        .ok()
                        .and_then(|value| value.get("parent_action_id").and_then(|v| v.as_i64()))
                        == Some(start.id)
                })
            }),
            "workflow_step input_summary should include parent_action_id"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: idempotent steps are skipped on retry to avoid double-apply.
    #[tokio::test]
    async fn idempotent_step_skip_prevents_double_send() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_idempotent_skip.db")
            .to_string_lossy()
            .to_string();

        let engine = WorkflowEngine::default();
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let wezterm: crate::wezterm::WeztermHandle = Arc::new(
            crate::wezterm::WeztermClient::with_socket("/tmp/wa-test-nonexistent.sock"),
        );
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                wezterm,
            ),
        ));

        let runner = WorkflowRunner::new(
            engine,
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );

        let pane_id = 47u64;
        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(IdempotentSendWorkflow));

        let detection = make_test_detection("idempotent_send.test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap();

        let workflow = runner.find_workflow_by_name("idempotent_send").unwrap();
        let ctx = WorkflowContext::new(
            Arc::clone(&storage),
            pane_id,
            PaneCapabilities::default(),
            execution_id,
        );
        let plan = workflow
            .to_action_plan(&ctx, execution_id)
            .expect("plan required");
        let step = &plan.steps[0];

        let result_data = serde_json::json!({
            "idempotency_key": step.step_id.0,
        })
        .to_string();

        storage
            .insert_step_log(
                execution_id,
                None,
                0,
                "send",
                Some(step.step_id.0.clone()),
                Some(step.action.action_type_name().to_string()),
                "continue",
                Some(result_data),
                None,
                None,
                None,
                now_ms(),
                now_ms(),
            )
            .await
            .unwrap();

        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(
            exec_result.is_completed(),
            "Workflow should complete when idempotent step is skipped"
        );

        storage.shutdown().await.unwrap();
    }

    /// Test: Step logs record abort correctly
    #[tokio::test]
    async fn step_logs_record_abort() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_step_logs_abort.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, _lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 46u64;

        create_test_pane(&storage, pane_id).await;
        // Workflow that fails at step 2
        runner.register_workflow(Arc::new(MultiStepWorkflow::failing_at(2)));

        // Start and run workflow
        let detection = make_test_detection("multi_step.abort_log_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());

        let workflow = runner.find_workflow_by_name("multi_step").unwrap();
        let execution_id = start_result.execution_id().unwrap();
        let exec_result = runner
            .run_workflow(pane_id, workflow, execution_id, 0)
            .await;

        assert!(exec_result.is_aborted(), "Workflow should abort");

        // Verify step logs
        let step_logs = storage.get_step_logs(execution_id).await.unwrap();

        // Should have 3 step logs (steps 0, 1, 2 where 2 aborts)
        assert_eq!(step_logs.len(), 3, "Should have 3 step log entries");

        // Steps 0 and 1 should be "continue"
        assert_eq!(step_logs[0].result_type, "continue");
        assert_eq!(step_logs[1].result_type, "continue");
        // Step 2 should be "abort"
        assert_eq!(step_logs[2].result_type, "abort");

        storage.shutdown().await.unwrap();
    }

    // ------------------------------------------------------------------------
    // Resume Tests (wa-nu4.1.1.7)
    // ------------------------------------------------------------------------

    /// Test: WorkflowEngine.resume computes correct next step from logs
    #[tokio::test]
    async fn engine_resume_finds_correct_step() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_resume.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create a test pane
        create_test_pane(&storage, 50).await;

        // Start a workflow
        let execution = engine
            .start(&storage, "test_workflow", 50, None, None)
            .await
            .unwrap();

        // Manually insert step logs to simulate partial execution
        // Steps 0 and 1 completed, step 2 was in progress
        storage
            .insert_step_log(
                &execution.id,
                None,
                0,
                "step_0",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1000,
                1100,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                &execution.id,
                None,
                1,
                "step_1",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1100,
                1200,
            )
            .await
            .unwrap();

        // Resume should find next step is 2
        let resume_result = engine.resume(&storage, &execution.id).await.unwrap();
        assert!(resume_result.is_some(), "Should find incomplete workflow");

        let (resumed_exec, next_step) = resume_result.unwrap();
        assert_eq!(resumed_exec.id, execution.id);
        assert_eq!(next_step, 2, "Next step should be 2 (after steps 0, 1)");

        storage.shutdown().await.unwrap();
    }

    /// Test: find_incomplete_workflows returns workflows with running/waiting status
    #[tokio::test]
    async fn find_incomplete_workflows_returns_running_and_waiting() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_find_incomplete.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create test panes
        create_test_pane(&storage, 51).await;
        create_test_pane(&storage, 52).await;
        create_test_pane(&storage, 53).await;

        // Start multiple workflows in different states
        let exec1 = engine
            .start(&storage, "workflow_1", 51, None, None)
            .await
            .unwrap();
        let exec2 = engine
            .start(&storage, "workflow_2", 52, None, None)
            .await
            .unwrap();
        let exec3 = engine
            .start(&storage, "workflow_3", 53, None, None)
            .await
            .unwrap();

        // Mark exec2 as waiting
        engine
            .update_status(
                &storage,
                &exec2.id,
                ExecutionStatus::Waiting,
                1,
                Some(&WaitCondition::pane_idle(1000)),
                None,
            )
            .await
            .unwrap();

        // Mark exec3 as completed (should not be returned)
        engine
            .update_status(
                &storage,
                &exec3.id,
                ExecutionStatus::Completed,
                2,
                None,
                None,
            )
            .await
            .unwrap();

        // Find incomplete workflows
        let incomplete = storage.find_incomplete_workflows().await.unwrap();

        // Should find exec1 (running) and exec2 (waiting), not exec3 (completed)
        assert_eq!(incomplete.len(), 2, "Should find 2 incomplete workflows");

        let incomplete_ids: std::collections::HashSet<_> =
            incomplete.iter().map(|w| w.id.as_str()).collect();
        assert!(incomplete_ids.contains(exec1.id.as_str()));
        assert!(incomplete_ids.contains(exec2.id.as_str()));
        assert!(!incomplete_ids.contains(exec3.id.as_str()));

        storage.shutdown().await.unwrap();
    }

    /// Test: resume_incomplete resumes workflows from last completed step
    #[tokio::test]
    async fn resume_incomplete_resumes_from_last_step() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_resume_incomplete.db")
            .to_string_lossy()
            .to_string();

        let (runner, storage, lock_manager) = create_test_runner(&db_path).await;
        let pane_id = 54u64;

        create_test_pane(&storage, pane_id).await;
        runner.register_workflow(Arc::new(MultiStepWorkflow::new()));

        // Start workflow and simulate partial execution
        let detection = make_test_detection("multi_step.resume_test");
        let start_result = runner.handle_detection(pane_id, &detection, None).await;
        assert!(start_result.is_started());
        let execution_id = start_result.execution_id().unwrap().to_string();

        // Insert step logs for steps 0 and 1 (completed)
        storage
            .insert_step_log(
                &execution_id,
                None,
                0,
                "step_0",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1000,
                1100,
            )
            .await
            .unwrap();
        storage
            .insert_step_log(
                &execution_id,
                None,
                1,
                "step_1",
                None,
                None,
                "continue",
                None,
                None,
                None,
                None,
                1100,
                1200,
            )
            .await
            .unwrap();

        // Release the lock to simulate a restart scenario
        lock_manager.force_release(pane_id);

        // Call resume_incomplete
        let results = runner.resume_incomplete().await;

        // Should have resumed and completed the workflow
        assert_eq!(results.len(), 1, "Should resume 1 workflow");
        assert!(
            results[0].is_completed(),
            "Resumed workflow should complete"
        );

        // Verify step logs show resumed execution (steps 2 and 3)
        let step_logs = storage.get_step_logs(&execution_id).await.unwrap();

        // Should have 4 step logs total now
        assert_eq!(step_logs.len(), 4, "Should have 4 step logs after resume");

        // Steps 0, 1 were from before, steps 2, 3 from resume
        assert_eq!(step_logs[2].step_index, 2);
        assert_eq!(step_logs[3].step_index, 3);
        assert_eq!(step_logs[3].result_type, "done");

        storage.shutdown().await.unwrap();
    }

    /// Test: Aborted workflows are not resumed
    #[tokio::test]
    async fn aborted_workflows_not_resumed() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir
            .path()
            .join("test_aborted_not_resumed.db")
            .to_string_lossy()
            .to_string();

        let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
        let engine = WorkflowEngine::new(3);

        // Create test pane
        create_test_pane(&storage, 55).await;

        // Start a workflow and mark it aborted
        let execution = engine
            .start(&storage, "test_workflow", 55, None, None)
            .await
            .unwrap();

        engine
            .update_status(
                &storage,
                &execution.id,
                ExecutionStatus::Aborted,
                1,
                None,
                Some("Test abort"),
            )
            .await
            .unwrap();

        // Find incomplete - should not include aborted workflow
        let incomplete = storage.find_incomplete_workflows().await.unwrap();
        assert!(
            incomplete.is_empty(),
            "Aborted workflow should not be in incomplete list"
        );

        // Resume should return None
        let resume_result = engine.resume(&storage, &execution.id).await.unwrap();
        assert!(
            resume_result.is_none(),
            "Aborted workflow should not be resumable"
        );

        storage.shutdown().await.unwrap();
    }

    // ====================================================================
    // Codex Exit Step Tests (wa-nu4.1.3.2)
    // ====================================================================

    #[derive(Clone)]
    struct TestTextSource {
        sequence: Arc<Vec<String>>,
        index: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TestTextSource {
        fn new(sequence: Vec<&str>) -> Self {
            Self {
                sequence: Arc::new(sequence.into_iter().map(str::to_string).collect()),
                index: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl PaneTextSource for TestTextSource {
        type Fut<'a> = Pin<Box<dyn Future<Output = crate::Result<String>> + Send + 'a>>;

        fn get_text(&self, _pane_id: u64, _escapes: bool) -> Self::Fut<'_> {
            let idx = self.index.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = self
                .sequence
                .get(idx)
                .cloned()
                .or_else(|| self.sequence.last().cloned())
                .unwrap_or_default();
            Box::pin(async move { Ok(text) })
        }
    }

    fn allowed_ctrl_c_result() -> InjectionResult {
        InjectionResult::Allowed {
            decision: crate::policy::PolicyDecision::allow(),
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        }
    }

    fn wait_options_single_poll() -> WaitOptions {
        WaitOptions {
            tail_lines: 200,
            escapes: false,
            poll_initial: Duration::from_millis(0),
            poll_max: Duration::from_millis(0),
            max_polls: 1,
        }
    }

    #[tokio::test]
    async fn codex_exit_sends_one_ctrl_c_when_summary_present() {
        let source = TestTextSource::new(vec![
            "Token usage: total=10 input=5 (+ 0 cached) output=5\nTo resume, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let result = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect("exit should succeed");

        assert_eq!(result.ctrl_c_count, 1);
        assert!(result.summary.matched);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn codex_exit_sends_second_ctrl_c_when_grace_times_out() {
        let source = TestTextSource::new(vec![
            "still running...",
            "Token usage: total=10 input=5 (+ 0 cached) output=5\nTo resume, run: codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let result = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect("exit should succeed");

        assert_eq!(result.ctrl_c_count, 2);
        assert!(result.summary.matched);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn codex_exit_errors_when_summary_never_appears() {
        let source = TestTextSource::new(vec!["no summary", "still no summary"]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(allowed_ctrl_c_result())
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let err = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect_err("expected failure");
        assert!(err.contains("Session summary not found"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn codex_exit_aborts_on_policy_denial() {
        let source = TestTextSource::new(vec![
            "Token usage: total=1 input=1 (+ 0 cached) output=0 codex resume 123e4567-e89b-12d3-a456-426614174000",
        ]);
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let send_ctrl_c = move || {
            let counter = Arc::clone(&counter_clone);
            async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(InjectionResult::Denied {
                    decision: crate::policy::PolicyDecision::deny("blocked"),
                    summary: "ctrl-c".to_string(),
                    pane_id: 1,
                    action: crate::policy::ActionKind::SendCtrlC,
                    audit_action_id: None,
                })
            }
        };

        let options = CodexExitOptions {
            grace_timeout_ms: 0,
            summary_timeout_ms: 0,
            wait_options: wait_options_single_poll(),
        };

        let err = codex_exit_and_wait_for_summary(1, &source, send_ctrl_c, &options)
            .await
            .expect_err("expected denial");
        assert!(err.contains("denied"));
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    // ========================================================================
    // Codex Session Summary Parsing Tests (wa-nu4.1.3.3)
    // ========================================================================

    #[test]
    fn parse_codex_session_summary_succeeds_on_valid_fixture() {
        let tail = r"
You've reached your usage limit.
Token usage: total=1,234 input=500 (+ 200 cached) output=534 (reasoning 100)
To continue this session, run: codex resume 123e4567-e89b-12d3-a456-426614174000
Try again at 3:00 PM UTC.
";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(result.token_usage.total, Some(1234));
        assert_eq!(result.token_usage.input, Some(500));
        assert_eq!(result.token_usage.cached, Some(200));
        assert_eq!(result.token_usage.output, Some(534));
        assert_eq!(result.token_usage.reasoning, Some(100));
        assert_eq!(result.reset_time.as_deref(), Some("3:00 PM UTC"));
    }

    #[test]
    fn parse_codex_session_summary_handles_minimal_valid_input() {
        let tail = "Token usage: total=100\ncodex resume abc12345-1234-1234-1234-123456789abc";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "abc12345-1234-1234-1234-123456789abc");
        assert_eq!(result.token_usage.total, Some(100));
        assert!(result.token_usage.input.is_none());
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn parse_codex_session_summary_handles_numbers_with_commas() {
        let tail = "Token usage: total=1,234,567 input=999,999\ncodex resume abcd1234-5678-90ab-cdef-1234567890ab";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.token_usage.total, Some(1_234_567));
        assert_eq!(result.token_usage.input, Some(999_999));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_session_id_missing() {
        let tail = "Token usage: total=100 input=50";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"session_id"));
        assert!(!err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_token_usage_missing() {
        let tail = "codex resume 123e4567-e89b-12d3-a456-426614174000";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"token_usage"));
        assert!(!err.missing.contains(&"session_id"));
    }

    #[test]
    fn parse_codex_session_summary_fails_when_both_missing() {
        let tail = "Some random text without markers";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_error_does_not_leak_raw_content() {
        let tail = "secret_api_key=sk-12345 some sensitive data";
        let err = parse_codex_session_summary(tail).expect_err("should fail");

        // Error should contain hash and length, not raw content
        let err_string = err.to_string();
        assert!(err_string.contains("tail_hash="));
        assert!(err_string.contains("tail_len="));
        assert!(!err_string.contains("secret_api_key"));
        assert!(!err_string.contains("sk-12345"));
    }

    #[test]
    fn parse_codex_session_summary_extracts_reset_time_variations() {
        // Various reset time formats
        let cases = [
            (
                "Token usage: total=1\ncodex resume abcd1234\ntry again at 2:30 PM",
                Some("2:30 PM"),
            ),
            (
                "Token usage: total=1\ncodex resume abcd1234\nTry again at tomorrow 9am.",
                Some("tomorrow 9am"),
            ),
            ("Token usage: total=1\ncodex resume abcd1234", None),
        ];

        for (tail, expected_reset) in cases {
            let result = parse_codex_session_summary(tail).expect("should parse");
            assert_eq!(
                result.reset_time.as_deref(),
                expected_reset,
                "Failed for: {tail}"
            );
        }
    }

    #[test]
    fn parse_codex_session_summary_uses_last_session_id_when_multiple() {
        // If multiple resume hints appear, use the last one
        let tail = "codex resume 11111111-1111-1111-1111-111111111111\nToken usage: total=1\ncodex resume 22222222-2222-2222-2222-222222222222";
        let result = parse_codex_session_summary(tail).expect("should parse");

        assert_eq!(result.session_id, "22222222-2222-2222-2222-222222222222");
    }

    // ========================================================================
    // Account Selection Step Tests (wa-nu4.1.3.4)
    // ========================================================================
    //
    // Note: The core selection logic (determinism, threshold filtering, LRU tie-break)
    // is tested in accounts.rs (9 tests). The `refresh_and_select_account` function
    // wires caut + storage + selection together.
    //
    // Full integration tests with a real database should be added to verify:
    // - caut refresh results are correctly persisted to the accounts table
    // - selection uses the refreshed data from DB
    // - last_used_at updates work correctly after successful failover
    //
    // The tests below verify the error types and result structures.

    #[test]
    fn account_selection_step_error_displays_caut_error() {
        let caut_err = crate::caut::CautError::NotInstalled;
        let step_err = AccountSelectionStepError::Caut(caut_err);
        let display = step_err.to_string();
        assert!(display.contains("caut error"));
        assert!(display.contains("not installed"));
    }

    #[test]
    fn account_selection_step_error_displays_storage_error() {
        let step_err = AccountSelectionStepError::Storage("connection failed".to_string());
        let display = step_err.to_string();
        assert!(display.contains("storage error"));
        assert!(display.contains("connection failed"));
    }

    #[test]
    fn account_selection_step_result_can_be_constructed() {
        use crate::accounts::SelectionExplanation;

        // Verify the step result structure is correct
        let explanation = SelectionExplanation {
            total_considered: 2,
            filtered_out: vec![],
            candidates: vec![],
            selection_reason: "Test reason".to_string(),
        };

        let result = AccountSelectionStepResult {
            selected: None,
            explanation,
            quota_advisory: crate::accounts::AccountQuotaAdvisory {
                availability: crate::accounts::QuotaAvailability::Exhausted,
                low_quota_threshold_percent: crate::accounts::DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
                selected_percent_remaining: None,
                warning: Some("Test reason".to_string()),
            },
            accounts_refreshed: 2,
        };

        assert!(result.selected.is_none());
        assert_eq!(result.accounts_refreshed, 2);
        assert_eq!(result.explanation.total_considered, 2);
    }

    #[test]
    fn account_selection_step_result_with_selected_account() {
        use crate::accounts::{AccountRecord, SelectionExplanation};

        let account = AccountRecord {
            id: 1,
            account_id: "acc-123".to_string(),
            service: "openai".to_string(),
            name: Some("Test Account".to_string()),
            percent_remaining: 75.0,
            reset_at: None,
            tokens_used: Some(1000),
            tokens_remaining: Some(3000),
            tokens_limit: Some(4000),
            last_refreshed_at: 1000,
            last_used_at: None,
            created_at: 1000,
            updated_at: 1000,
        };

        let explanation = SelectionExplanation {
            total_considered: 1,
            filtered_out: vec![],
            candidates: vec![],
            selection_reason: "Only eligible account".to_string(),
        };

        let result = AccountSelectionStepResult {
            selected: Some(account),
            explanation,
            quota_advisory: crate::accounts::AccountQuotaAdvisory {
                availability: crate::accounts::QuotaAvailability::Available,
                low_quota_threshold_percent: crate::accounts::DEFAULT_LOW_QUOTA_THRESHOLD_PERCENT,
                selected_percent_remaining: Some(75.0),
                warning: None,
            },
            accounts_refreshed: 1,
        };

        assert!(result.selected.is_some());
        assert_eq!(result.selected.as_ref().unwrap().account_id, "acc-123");
        assert_eq!(result.accounts_refreshed, 1);
    }

    // ── Device code parsing tests ───────────────────────────────────────

    #[test]
    fn parse_device_code_direct_format() {
        // parse_device_code expects "code" or "enter" directly before the code value
        let tail = "Your one-time code: ABCD-12345 (visit https://auth.openai.com/device)";
        let result = parse_device_code(tail).expect("should parse");
        assert_eq!(result.code, "ABCD-12345");
        assert!(result.url.is_some());
        assert!(result.url.unwrap().contains("auth.openai.com"));
    }

    #[test]
    fn parse_device_code_v2_format() {
        let tail = "Please open https://auth.openai.com/codex/device and enter this one-time code: WXYZ-98765";
        let result = parse_device_code(tail).expect("should parse");
        assert_eq!(result.code, "WXYZ-98765");
        assert!(result.url.is_some());
    }

    #[test]
    fn parse_device_code_lowercase_prompt() {
        let tail = "enter code: AAAA-BBBB";
        let result = parse_device_code(tail).expect("should parse");
        assert_eq!(result.code, "AAAA-BBBB");
        assert!(result.url.is_none());
    }

    #[test]
    fn parse_device_code_no_code_returns_error() {
        let tail = "Some random output with no device code";
        let err = parse_device_code(tail).unwrap_err();
        assert!(err.to_string().contains("device code"));
        // Must not contain raw tail content (safe diagnostics)
        assert!(!err.to_string().contains("random output"));
        assert!(err.tail_hash != 0);
        assert_eq!(err.tail_len, tail.len());
    }

    #[test]
    fn parse_device_code_empty_input() {
        let err = parse_device_code("").unwrap_err();
        assert_eq!(err.tail_len, 0);
    }

    #[test]
    fn parse_device_code_url_only_no_code() {
        let tail = "Visit https://auth.openai.com/device to continue";
        let err = parse_device_code(tail).unwrap_err();
        assert!(err.tail_len > 0);
    }

    #[test]
    fn parse_device_code_mixed_case() {
        let tail = "CODE: abcd-efgh";
        let result = parse_device_code(tail).expect("should parse case-insensitive");
        assert_eq!(result.code, "ABCD-EFGH");
    }

    #[test]
    fn validate_device_code_valid_formats() {
        assert!(validate_device_code("ABCD-1234"));
        assert!(validate_device_code("ABCD-12345"));
        assert!(validate_device_code("WXYZ-EFGH"));
        assert!(validate_device_code("1234-5678"));
    }

    #[test]
    fn validate_device_code_invalid_formats() {
        assert!(!validate_device_code(""));
        assert!(!validate_device_code("ABCD"));
        assert!(!validate_device_code("AB-CD"));
        assert!(!validate_device_code("ABCD-"));
        assert!(!validate_device_code("-ABCD"));
        assert!(!validate_device_code("ABCD-EF-GH"));
    }

    #[test]
    fn device_auth_login_command_is_correct() {
        assert_eq!(DEVICE_AUTH_LOGIN_COMMAND, "cod login --device-auth\n");
        assert!(DEVICE_AUTH_LOGIN_COMMAND.ends_with('\n'));
    }

    #[test]
    fn device_code_parse_error_display_is_safe() {
        let sensitive_tail = "secret-token: sk-abc123 and code somewhere";
        let err = parse_device_code(sensitive_tail).unwrap_err();
        let display = err.to_string();
        // Display must not leak raw tail content
        assert!(!display.contains("sk-abc123"));
        assert!(!display.contains("secret-token"));
        // But should contain diagnostic info
        assert!(display.contains("device code"));
    }

    // ========================================================================
    // HandleSessionEnd Tests (wa-nu4.2.2.3)
    // ========================================================================

    #[test]
    fn handle_session_end_metadata() {
        let wf = HandleSessionEnd::new();
        assert_eq!(wf.name(), "handle_session_end");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 2);
        assert_eq!(wf.steps()[0].name, "extract_summary");
        assert_eq!(wf.steps()[1].name, "persist_record");
    }

    #[test]
    fn handle_session_end_handles_session_summary() {
        let wf = HandleSessionEnd::new();
        let detection = Detection {
            rule_id: "codex.session.token_usage".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Token usage: total=100".to_string(),
            span: (0, 20),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_session_end_handles_session_end() {
        let wf = HandleSessionEnd::new();
        let detection = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Session ended".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_session_end_ignores_other_events() {
        let wf = HandleSessionEnd::new();
        let detection = Detection {
            rule_id: "codex.usage.warning".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "usage warning".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_session_end_record_from_codex_detection() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "1500",
                "input": "1000",
                "output": "500",
                "cached": "200",
                "reasoning": "50",
            }
        });
        let record = HandleSessionEnd::record_from_detection(42, &trigger);
        assert_eq!(record.pane_id, 42);
        assert_eq!(record.agent_type, "codex");
        assert_eq!(record.total_tokens, Some(1500));
        assert_eq!(record.input_tokens, Some(1000));
        assert_eq!(record.output_tokens, Some(500));
        assert_eq!(record.cached_tokens, Some(200));
        assert_eq!(record.reasoning_tokens, Some(50));
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
        assert!(record.ended_at.is_some());
    }

    #[test]
    fn handle_session_end_record_from_claude_code_detection() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.summary",
            "extracted": {
                "cost": "2.50",
            }
        });
        let record = HandleSessionEnd::record_from_detection(99, &trigger);
        assert_eq!(record.pane_id, 99);
        assert_eq!(record.agent_type, "claude_code");
        assert_eq!(record.estimated_cost_usd, Some(2.50));
        assert!(record.total_tokens.is_none());
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
    }

    #[test]
    fn handle_session_end_record_from_gemini_detection() {
        let trigger = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "session.summary",
            "extracted": {
                "session_id": "abcdef12-3456-7890-abcd-ef1234567890",
                "tool_calls": "7",
            }
        });
        let record = HandleSessionEnd::record_from_detection(10, &trigger);
        assert_eq!(record.pane_id, 10);
        assert_eq!(record.agent_type, "gemini");
        assert_eq!(
            record.session_id.as_deref(),
            Some("abcdef12-3456-7890-abcd-ef1234567890")
        );
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
    }

    #[test]
    fn handle_session_end_record_from_session_end_event() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.end",
            "extracted": {}
        });
        let record = HandleSessionEnd::record_from_detection(5, &trigger);
        assert_eq!(record.agent_type, "claude_code");
        assert_eq!(record.end_reason.as_deref(), Some("completed"));
        assert!(record.ended_at.is_some());
        // No token or cost data expected from a bare session.end
        assert!(record.total_tokens.is_none());
        assert!(record.estimated_cost_usd.is_none());
    }

    #[test]
    fn handle_session_end_record_missing_extracted() {
        let trigger = serde_json::json!({
            "agent_type": "unknown",
            "event_type": "session.end",
        });
        let record = HandleSessionEnd::record_from_detection(1, &trigger);
        assert_eq!(record.agent_type, "unknown");
        assert!(record.session_id.is_none());
        assert!(record.total_tokens.is_none());
        assert!(record.estimated_cost_usd.is_none());
    }

    #[test]
    fn handle_session_end_record_comma_numbers() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "1,500,000",
                "input": "1,000,000",
                "output": "500,000",
            }
        });
        let record = HandleSessionEnd::record_from_detection(42, &trigger);
        assert_eq!(record.total_tokens, Some(1_500_000));
        assert_eq!(record.input_tokens, Some(1_000_000));
        assert_eq!(record.output_tokens, Some(500_000));
    }

    #[test]
    fn handle_session_end_trigger_event_types() {
        let wf = HandleSessionEnd::new();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"session.summary"));
        assert!(types.contains(&"session.end"));
    }

    #[test]
    fn handle_session_end_supported_agents() {
        let wf = HandleSessionEnd::new();
        let agents = wf.supported_agent_types();
        assert!(agents.contains(&"codex"));
        assert!(agents.contains(&"claude_code"));
        assert!(agents.contains(&"gemini"));
    }

    #[test]
    fn handle_session_end_not_destructive() {
        let wf = HandleSessionEnd::new();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[tokio::test]
    async fn handle_session_end_persist_roundtrip() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path =
            std::env::temp_dir().join(format!("wa_test_session_end_{}_{n}.db", std::process::id()));
        let db = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        // Insert a pane record first (FK constraint)
        let pane = crate::storage::PaneRecord {
            pane_id: 77,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "5000",
                "input": "3000",
                "output": "2000",
                "session_id": "abc-def-123",
            }
        });
        let record = HandleSessionEnd::record_from_detection(77, &trigger);
        let db_id = db.upsert_agent_session(record).await.expect("upsert");
        assert!(db_id > 0);

        // Query back by DB id
        let session = db
            .get_agent_session(db_id)
            .await
            .expect("query")
            .expect("session should exist");
        assert_eq!(session.agent_type, "codex");
        assert_eq!(session.session_id.as_deref(), Some("abc-def-123"));
        assert_eq!(session.total_tokens, Some(5000));
        assert_eq!(session.input_tokens, Some(3000));
        assert_eq!(session.output_tokens, Some(2000));
        assert!(session.ended_at.is_some());
        assert_eq!(session.end_reason.as_deref(), Some("completed"));
    }

    #[tokio::test]
    async fn persist_caut_refresh_accounts_records_metrics() {
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_caut_metrics_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let storage = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        let refresh = crate::caut::CautRefresh {
            service: Some("openai".to_string()),
            refreshed_at: Some("2026-02-06T00:00:00Z".to_string()),
            accounts: vec![
                crate::caut::CautAccountUsage {
                    id: Some("acct-1".to_string()),
                    name: Some("Account 1".to_string()),
                    percent_remaining: Some(42.0),
                    limit_hours: None,
                    reset_at: Some("2026-02-06T01:00:00Z".to_string()),
                    tokens_used: Some(1000),
                    tokens_remaining: Some(2000),
                    tokens_limit: Some(3000),
                    extra: HashMap::new(),
                },
                crate::caut::CautAccountUsage {
                    id: Some("acct-2".to_string()),
                    name: Some("Account 2".to_string()),
                    percent_remaining: Some(7.0),
                    limit_hours: None,
                    reset_at: None,
                    tokens_used: Some(10),
                    tokens_remaining: Some(20),
                    tokens_limit: Some(30),
                    extra: HashMap::new(),
                },
            ],
            extra: HashMap::new(),
        };

        let now = 10_000_i64;
        let refreshed = super::persist_caut_refresh_accounts(
            &storage,
            crate::caut::CautService::OpenAI,
            &refresh,
            now,
        )
        .await
        .expect("persist refresh");
        assert_eq!(refreshed, 2);

        // Query by account_id to avoid relying on agent_type filtering (we store agent_type=None here).
        let acct1 = storage
            .query_usage_metrics(crate::storage::MetricQuery {
                metric_type: Some(crate::storage::MetricType::TokenUsage),
                agent_type: None,
                account_id: Some("acct-1".to_string()),
                since: Some(0),
                until: None,
                limit: Some(10),
            })
            .await
            .expect("query metrics");
        assert_eq!(acct1.len(), 1);
        assert_eq!(acct1[0].tokens, Some(1000));
        assert_eq!(acct1[0].amount, None);

        storage.shutdown().await.expect("shutdown");
        let _ = std::fs::remove_file(&db_path);
    }

    // ========================================================================
    // HandleProcessTriageLifecycle Tests (ft-2vuw7.5.4.1.2.1.1)
    // ========================================================================

    #[test]
    fn handle_process_triage_lifecycle_metadata() {
        let wf = HandleProcessTriageLifecycle::new();
        assert_eq!(wf.name(), "handle_process_triage_lifecycle");
        assert_eq!(wf.steps().len(), 6);
        assert_eq!(wf.steps()[0].name, "snapshot");
        assert_eq!(wf.steps()[5].name, "session");
        assert_eq!(wf.trigger_event_types(), ["process_triage.lifecycle"]);
        assert_eq!(wf.trigger_rule_ids(), ["process_triage.lifecycle"]);
        assert!(!wf.is_destructive());
    }

    #[test]
    fn handle_process_triage_lifecycle_handles_expected_detection() {
        let wf = HandleProcessTriageLifecycle::new();

        let detection = Detection {
            rule_id: "process_triage.lifecycle".to_string(),
            agent_type: AgentType::Codex,
            event_type: "process_triage.lifecycle".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "triage lifecycle requested".to_string(),
            span: (0, 10),
        };
        assert!(wf.handles(&detection));

        let non_matching = Detection {
            rule_id: "codex.session.token_usage".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "Token usage".to_string(),
            span: (0, 10),
        };
        assert!(!wf.handles(&non_matching));
    }

    #[tokio::test]
    async fn handle_process_triage_lifecycle_step0_aborts_on_alt_screen() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_alt_screen.db");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let mut caps = PaneCapabilities::default();
        caps.alt_screen = Some(true);
        let mut ctx = WorkflowContext::new(storage, 7, caps, "exec-triage-alt");

        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 0).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(reason.contains("alt-screen"), "unexpected reason: {reason}");
            }
            other => panic!("expected abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_process_triage_lifecycle_step2_aborts_on_protected_destructive_action() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_protected_abort.db");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let trigger = serde_json::json!({
            "process_triage": {
                "plan": {
                    "entries": [
                        {
                            "category": "system_process",
                            "action": { "action": "force_kill" }
                        }
                    ],
                    "auto_safe_count": 0,
                    "review_count": 0,
                    "protected_count": 1
                }
            }
        });

        let mut ctx = WorkflowContext::new(
            storage,
            9,
            PaneCapabilities::default(),
            "exec-triage-protected",
        )
        .with_trigger(trigger);

        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 2).await;
        match result {
            StepResult::Abort { reason } => {
                assert!(
                    reason.contains("protected category includes destructive action"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected abort, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_process_triage_lifecycle_session_step_emits_all_artifacts() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("triage_lifecycle_session.db");
        let storage = Arc::new(
            crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let trigger = serde_json::json!({
            "process_triage": {
                "ft_session_id": "ft-abc",
                "pt_session_id": "pt-xyz",
                "provider": "pt_cli",
                "plan": {
                    "entries": [
                        {
                            "category": "stuck_cli",
                            "action": { "action": "graceful_kill" }
                        },
                        {
                            "category": "active_agent",
                            "action": { "action": "protect" }
                        }
                    ],
                    "auto_safe_count": 1,
                    "review_count": 0,
                    "protected_count": 1
                }
            }
        });

        let mut ctx = WorkflowContext::new(
            storage,
            42,
            PaneCapabilities::default(),
            "exec-triage-session",
        )
        .with_trigger(trigger);
        let wf = HandleProcessTriageLifecycle::new();
        let result = wf.execute_step(&mut ctx, 5).await;

        match result {
            StepResult::Done { result } => {
                assert_eq!(result["status"], "completed");
                assert_eq!(result["workflow"], "handle_process_triage_lifecycle");
                assert!(result["snapshot"].is_object());
                assert!(result["plan"].is_object());
                assert!(result["apply"].is_object());
                assert!(result["verify"].is_object());
                assert!(result["diff"].is_object());
                assert_eq!(result["session"]["ft_session_id"], "ft-abc");
                assert_eq!(result["session"]["pt_session_id"], "pt-xyz");
                assert_eq!(result["session"]["provider"], "pt_cli");
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

    // ========================================================================
    // HandleAuthRequired Tests (wa-nu4.2.2.4)
    // ========================================================================

    #[test]
    fn handle_auth_required_metadata() {
        let wf = HandleAuthRequired::new();
        assert_eq!(wf.name(), "handle_auth_required");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.steps()[0].name, "check_cooldown");
        assert_eq!(wf.steps()[1].name, "classify_auth");
        assert_eq!(wf.steps()[2].name, "record_and_plan");
    }

    #[test]
    fn handle_auth_required_handles_device_code() {
        let wf = HandleAuthRequired::new();
        let detection = Detection {
            rule_id: "codex.auth.device_code_prompt".to_string(),
            agent_type: AgentType::Codex,
            event_type: "auth.device_code".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Enter code".to_string(),
            span: (0, 10),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_auth_required_handles_auth_error() {
        let wf = HandleAuthRequired::new();
        let detection = Detection {
            rule_id: "claude_code.auth.api_key_error".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "auth.error".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "API key invalid".to_string(),
            span: (0, 15),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_auth_required_ignores_session_events() {
        let wf = HandleAuthRequired::new();
        let detection = Detection {
            rule_id: "codex.session.token_usage".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::default(),
            matched_text: "Token usage".to_string(),
            span: (0, 11),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn auth_strategy_device_code_from_detection() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "auth.device_code",
            "rule_id": "codex.auth.device_code_prompt",
            "extracted": {
                "code": "ABCD-12345",
            }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "device_code");
        match &strategy {
            AuthRecoveryStrategy::DeviceCode { code, url } => {
                assert_eq!(code.as_deref(), Some("ABCD-12345"));
                assert!(url.is_none());
            }
            _ => panic!("Expected DeviceCode strategy"),
        }
    }

    #[test]
    fn auth_strategy_api_key_error_from_detection() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "auth.error",
            "rule_id": "claude_code.auth.api_key_error",
            "extracted": {}
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "api_key_error");
    }

    #[test]
    fn auth_strategy_manual_intervention_fallback() {
        let trigger = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "auth.unknown",
            "rule_id": "gemini.auth.something",
            "extracted": {}
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "manual_intervention");
        match &strategy {
            AuthRecoveryStrategy::ManualIntervention { agent_type, hint } => {
                assert_eq!(agent_type, "gemini");
                assert!(hint.contains("gemini"));
            }
            _ => panic!("Expected ManualIntervention strategy"),
        }
    }

    #[test]
    fn handle_auth_required_trigger_event_types() {
        let wf = HandleAuthRequired::new();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"auth.device_code"));
        assert!(types.contains(&"auth.error"));
    }

    #[test]
    fn handle_auth_required_supported_agents() {
        let wf = HandleAuthRequired::new();
        let agents = wf.supported_agent_types();
        assert!(agents.contains(&"codex"));
        assert!(agents.contains(&"claude_code"));
        assert!(agents.contains(&"gemini"));
    }

    #[test]
    fn handle_auth_required_not_destructive() {
        let wf = HandleAuthRequired::new();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn auth_strategy_serializes() {
        let strategy = AuthRecoveryStrategy::DeviceCode {
            code: Some("ABCD-12345".to_string()),
            url: Some("https://auth.openai.com/device".to_string()),
        };
        let json = serde_json::to_value(&strategy).unwrap();
        assert_eq!(json["strategy"], "DeviceCode");
        assert_eq!(json["code"], "ABCD-12345");
        assert_eq!(json["url"], "https://auth.openai.com/device");
    }

    #[test]
    fn handle_auth_required_normalized_cass_query_prefers_matched_text() {
        let trigger = serde_json::json!({
            "matched_text": "   invalid   api   key   from provider   ",
            "event_type": "auth.error",
            "agent_type": "codex"
        });
        let query = HandleAuthRequired::normalized_cass_query(&trigger)
            .expect("query should be derived from matched text");
        assert_eq!(query, "invalid api key from provider");
    }

    #[test]
    fn handle_auth_required_build_recovery_prompt_includes_cass_hints() {
        let strategy = AuthRecoveryStrategy::ApiKeyError {
            key_hint: Some("OPENAI_API_KEY".to_string()),
        };
        let trigger = serde_json::json!({
            "matched_text": "Invalid API key",
        });
        let lookup = AuthCassHintsLookup {
            query: Some("Invalid API key".to_string()),
            workspace: Some("/repo".to_string()),
            hints: vec![
                "/repo/sessions/a.jsonl:42 - rotate key and retry".to_string(),
                "/repo/sessions/b.jsonl:13 - export OPENAI_API_KEY before launch".to_string(),
            ],
            error: None,
        };

        let prompt = HandleAuthRequired::build_recovery_prompt(&strategy, &trigger, &lookup);
        assert!(prompt.contains("Strategy: api_key_error"));
        assert!(prompt.contains("Related fixes from past sessions (cass):"));
        assert!(prompt.contains("OPENAI_API_KEY"));
        assert!(prompt.contains("Cass query: Invalid API key"));
        assert!(prompt.contains("Cass workspace filter: /repo"));
    }

    #[tokio::test]
    async fn handle_auth_required_audit_roundtrip() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path =
            std::env::temp_dir().join(format!("wa_test_auth_req_{}_{n}.db", std::process::id()));
        let db = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        // Insert pane record
        let pane = crate::storage::PaneRecord {
            pane_id: 88,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        // Record an auth event
        let audit = crate::storage::AuditActionRecord {
            id: 0,
            ts: now_ms(),
            actor_kind: "workflow".to_string(),
            actor_id: Some("test-exec-1".to_string()),
            correlation_id: None,
            pane_id: Some(88),
            domain: None,
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: Some("codex.auth.device_code_prompt".to_string()),
            input_summary: Some("Auth required for codex: device_code".to_string()),
            verification_summary: None,
            decision_context: None,
            result: "recorded".to_string(),
        };
        let audit_id = db.record_audit_action(audit).await.expect("record");
        assert!(audit_id > 0);

        // Query back
        let query = crate::storage::AuditQuery {
            pane_id: Some(88),
            action_kind: Some("auth_required".to_string()),
            limit: Some(10),
            ..Default::default()
        };
        let results = db.get_audit_actions(query).await.expect("query");
        assert!(!results.is_empty());
        assert_eq!(results[0].action_kind, "auth_required");
        assert_eq!(results[0].pane_id, Some(88));
    }

    #[tokio::test]
    async fn handle_auth_required_cooldown_blocks_repeat() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR2: AtomicU64 = AtomicU64::new(0);
        let n = CTR2.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_auth_cooldown_{}_{n}.db",
            std::process::id()
        ));
        let db = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("temp DB");

        // Insert pane
        let pane = crate::storage::PaneRecord {
            pane_id: 89,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: None,
            cwd: None,
            tty_name: None,
            first_seen_at: now_ms(),
            last_seen_at: now_ms(),
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        };
        db.upsert_pane(pane).await.expect("insert pane");

        // Insert a recent auth event (within cooldown)
        let audit = crate::storage::AuditActionRecord {
            id: 0,
            ts: now_ms(), // Just now
            actor_kind: "workflow".to_string(),
            actor_id: Some("test-exec-2".to_string()),
            correlation_id: None,
            pane_id: Some(89),
            domain: None,
            action_kind: "auth_required".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "recorded".to_string(),
        };
        db.record_audit_action(audit).await.expect("record");

        // Now check cooldown: query for recent auth events within default window
        let since = now_ms() - AUTH_COOLDOWN_MS;
        let query = crate::storage::AuditQuery {
            pane_id: Some(89),
            action_kind: Some("auth_required".to_string()),
            since: Some(since),
            limit: Some(1),
            ..Default::default()
        };
        let results = db.get_audit_actions(query).await.expect("query");
        assert!(
            !results.is_empty(),
            "Should find recent auth event within cooldown window"
        );
    }

    // ========================================================================
    // HandleClaudeCodeLimits Tests (wa-03j, wa-nu4.2.2.1)
    // ========================================================================

    #[test]
    fn handle_claude_code_limits_metadata() {
        let wf = HandleClaudeCodeLimits::new();
        assert_eq!(wf.name(), "handle_claude_code_limits");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.steps()[0].name, "check_guards");
        assert_eq!(wf.steps()[1].name, "check_cooldown");
        assert_eq!(wf.steps()[2].name, "classify_and_record");
    }

    #[test]
    fn handle_claude_code_limits_handles_usage_warning() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.usage.warning".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({"remaining": "10"}),
            matched_text: "usage limit".to_string(),
            span: (0, 11),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_claude_code_limits_handles_usage_reached() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "limit reached".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_claude_code_limits_ignores_codex_usage() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "limit reached".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection), "Should ignore Codex usage events");
    }

    #[test]
    fn handle_claude_code_limits_ignores_session_events() {
        let wf = HandleClaudeCodeLimits::new();
        let detection = Detection {
            rule_id: "claude_code.session.end".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: "Session ended".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection), "Should ignore session.end events");
    }

    #[test]
    fn handle_claude_code_limits_trigger_event_types() {
        let wf = HandleClaudeCodeLimits::new();
        let types = wf.trigger_event_types();
        assert!(types.contains(&"usage.warning"));
        assert!(types.contains(&"usage.reached"));
    }

    #[test]
    fn handle_claude_code_limits_supported_agents() {
        let wf = HandleClaudeCodeLimits::new();
        let agents = wf.supported_agent_types();
        assert_eq!(agents, &["claude_code"]);
    }

    #[test]
    fn handle_claude_code_limits_not_destructive() {
        let wf = HandleClaudeCodeLimits::new();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn handle_claude_code_limits_classify_usage_warning() {
        let trigger = serde_json::json!({
            "event_type": "usage.warning",
            "extracted": { "remaining": "10" }
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "usage_warning");
        assert!(reset_time.is_none());
    }

    #[test]
    fn handle_claude_code_limits_classify_usage_reached() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": { "reset_time": "30 minutes" }
        });
        let (limit_type, reset_time) = HandleClaudeCodeLimits::classify_limit(&trigger);
        assert_eq!(limit_type, "usage_reached");
        assert_eq!(reset_time.as_deref(), Some("30 minutes"));
    }

    #[test]
    fn handle_claude_code_limits_recovery_plan_warning() {
        let plan = HandleClaudeCodeLimits::build_recovery_plan("usage_warning", None, 42);
        assert_eq!(plan["limit_type"], "usage_warning");
        assert_eq!(plan["pane_id"], 42);
        assert_eq!(plan["safe_to_send"], true);
        assert!(!plan["next_steps"].as_array().unwrap().is_empty());
    }

    #[test]
    fn handle_claude_code_limits_recovery_plan_reached() {
        let reset = Some("2 hours".to_string());
        let plan =
            HandleClaudeCodeLimits::build_recovery_plan("usage_reached", reset.as_deref(), 7);
        assert_eq!(plan["limit_type"], "usage_reached");
        assert_eq!(plan["safe_to_send"], false);
        assert_eq!(plan["reset_time"], "2 hours");
    }

    #[test]
    fn handle_claude_code_limits_custom_cooldown() {
        let wf = HandleClaudeCodeLimits::with_cooldown_ms(30_000);
        assert_eq!(wf.cooldown_ms, 30_000);
    }

    // ========================================================================
    // HandleGeminiQuota Unit Tests (wa-smm)
    // ========================================================================

    #[test]
    fn handle_gemini_quota_metadata() {
        let wf = HandleGeminiQuota::default();
        assert_eq!(wf.name(), "handle_gemini_quota");
        assert!(!wf.description().is_empty());
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.steps()[0].name, "check_guards");
        assert_eq!(wf.steps()[1].name, "check_cooldown");
        assert_eq!(wf.steps()[2].name, "classify_and_record");
    }

    #[test]
    fn handle_gemini_quota_handles_usage_warning() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.usage.reached".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "usage.warning".to_string(),
            severity: Severity::Warning,
            confidence: 0.95,
            extracted: serde_json::json!({"remaining": "15"}),
            matched_text: "usage warning".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_handles_usage_reached() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.usage.reached".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({"remaining": "0"}),
            matched_text: "usage reached".to_string(),
            span: (0, 13),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_ignores_codex_usage() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "codex.usage.reached".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "usage reached".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_ignores_claude_code_usage() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "usage reached".to_string(),
            span: (0, 13),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_ignores_session_events() {
        let wf = HandleGeminiQuota::default();
        let detection = Detection {
            rule_id: "gemini.session.end".to_string(),
            agent_type: AgentType::Gemini,
            event_type: "session.end".to_string(),
            severity: Severity::Info,
            confidence: 0.95,
            extracted: serde_json::json!({}),
            matched_text: "session end".to_string(),
            span: (0, 11),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_gemini_quota_trigger_event_types() {
        let wf = HandleGeminiQuota::default();
        let types = wf.trigger_event_types();
        assert_eq!(types, &["usage.warning", "usage.reached"]);
    }

    #[test]
    fn handle_gemini_quota_supported_agents() {
        let wf = HandleGeminiQuota::default();
        assert_eq!(wf.supported_agent_types(), &["gemini"]);
    }

    #[test]
    fn handle_gemini_quota_not_destructive() {
        let wf = HandleGeminiQuota::default();
        assert!(!wf.is_destructive());
        assert!(!wf.requires_approval());
        assert!(wf.requires_pane());
    }

    #[test]
    fn handle_gemini_quota_classify_usage_warning() {
        let trigger = serde_json::json!({
            "event_type": "usage.warning",
            "extracted": { "remaining": "15" }
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_warning");
        assert_eq!(remaining, Some("15".to_string()));
    }

    #[test]
    fn handle_gemini_quota_classify_usage_reached() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": { "remaining": "0" }
        });
        let (quota_type, remaining) = HandleGeminiQuota::classify_quota(&trigger);
        assert_eq!(quota_type, "quota_reached");
        assert_eq!(remaining, Some("0".to_string()));
    }

    #[test]
    fn handle_gemini_quota_recovery_plan_warning() {
        let plan = HandleGeminiQuota::build_recovery_plan("quota_warning", Some("15"), 42);
        assert_eq!(plan["quota_type"], "quota_warning");
        assert_eq!(plan["pane_id"], 42);
        assert_eq!(plan["safe_to_send"], true);
        assert!(plan["next_steps"].is_array());
    }

    #[test]
    fn handle_gemini_quota_recovery_plan_reached() {
        let plan = HandleGeminiQuota::build_recovery_plan("quota_reached", Some("0"), 42);
        assert_eq!(plan["quota_type"], "quota_reached");
        assert_eq!(plan["safe_to_send"], false);
        assert!(plan["next_steps"].is_array());
    }

    #[test]
    fn handle_gemini_quota_custom_cooldown() {
        let wf = HandleGeminiQuota::with_cooldown_ms(60_000);
        assert_eq!(wf.cooldown_ms, 60_000);
    }

    // ========================================================================
    // Workflow Regression Tests (wa-nu4.2.2.5)
    // ========================================================================

    /// Helper: build a detection with specific event_type, agent_type, and extracted data.
    fn make_session_detection(
        rule_id: &str,
        agent_type: AgentType,
        event_type: &str,
        extracted: serde_json::Value,
    ) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type,
            event_type: event_type.to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted,
            matched_text: "test fixture".to_string(),
            span: (0, 12),
        }
    }

    #[test]
    fn regression_session_end_selects_for_codex_summary() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::json!({"total": "5000"}),
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match codex session.summary"
        );
    }

    #[test]
    fn regression_session_end_selects_for_claude_summary() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "claude_code.session.cost_summary",
            AgentType::ClaudeCode,
            "session.summary",
            serde_json::json!({"cost": "3.50"}),
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match claude_code session.summary"
        );
    }

    #[test]
    fn regression_session_end_selects_for_gemini_summary() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "gemini.session.summary",
            AgentType::Gemini,
            "session.summary",
            serde_json::json!({"session_id": "abc-123"}),
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match gemini session.summary"
        );
    }

    #[test]
    fn regression_session_end_selects_for_session_end_event() {
        let wf = HandleSessionEnd::new();
        let det = make_session_detection(
            "claude_code.session.end",
            AgentType::ClaudeCode,
            "session.end",
            serde_json::Value::Null,
        );
        assert!(
            wf.handles(&det),
            "HandleSessionEnd should match session.end event"
        );
    }

    #[test]
    fn regression_auth_required_selects_for_device_code() {
        let wf = HandleAuthRequired::new();
        let det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({"code": "ABCD-12345"}),
        );
        assert!(
            wf.handles(&det),
            "HandleAuthRequired should match auth.device_code"
        );
    }

    #[test]
    fn regression_auth_required_selects_for_api_key_error() {
        let wf = HandleAuthRequired::new();
        let det = make_session_detection(
            "claude_code.auth.api_key_error",
            AgentType::ClaudeCode,
            "auth.error",
            serde_json::Value::Null,
        );
        assert!(
            wf.handles(&det),
            "HandleAuthRequired should match auth.error"
        );
    }

    #[test]
    fn regression_no_cross_trigger_session_to_auth() {
        let session_wf = HandleSessionEnd::new();
        let auth_wf = HandleAuthRequired::new();

        // session.summary should NOT trigger auth workflow
        let session_det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::json!({}),
        );
        assert!(
            !auth_wf.handles(&session_det),
            "Auth workflow should NOT match session.summary"
        );
        assert!(
            session_wf.handles(&session_det),
            "Session workflow should match session.summary"
        );

        // auth.device_code should NOT trigger session workflow
        let auth_det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({}),
        );
        assert!(
            !session_wf.handles(&auth_det),
            "Session workflow should NOT match auth.device_code"
        );
        assert!(
            auth_wf.handles(&auth_det),
            "Auth workflow should match auth.device_code"
        );
    }

    #[test]
    fn regression_runner_selects_session_end_workflow() {
        let rt = crate::runtime_compat::RuntimeBuilder::multi_thread()
            .build()
            .unwrap();
        let db_path =
            std::env::temp_dir().join(format!("wa_test_reg_sel_{}.db", std::process::id()));
        let db_path_str = db_path.to_string_lossy().to_string();

        rt.block_on(async {
            let (runner, _storage, _lock) = create_test_runner(&db_path_str).await;
            runner.register_workflow(Arc::new(HandleSessionEnd::new()));
            runner.register_workflow(Arc::new(HandleAuthRequired::new()));

            // Session summary → session end workflow
            let det = make_session_detection(
                "codex.session.token_usage",
                AgentType::Codex,
                "session.summary",
                serde_json::json!({"total": "1000"}),
            );
            let wf = runner.find_matching_workflow(&det);
            assert!(
                wf.is_some(),
                "Should find matching workflow for session.summary"
            );
            assert_eq!(wf.unwrap().name(), "handle_session_end");

            // Auth device code → auth required workflow
            let det = make_session_detection(
                "codex.auth.device_code_prompt",
                AgentType::Codex,
                "auth.device_code",
                serde_json::json!({"code": "TEST-12345"}),
            );
            let wf = runner.find_matching_workflow(&det);
            assert!(
                wf.is_some(),
                "Should find matching workflow for auth.device_code"
            );
            assert_eq!(wf.unwrap().name(), "handle_auth_required");

            // Unrelated detection → no workflow
            let det = make_session_detection(
                "some.other.rule",
                AgentType::Codex,
                "something.else",
                serde_json::Value::Null,
            );
            let wf = runner.find_matching_workflow(&det);
            assert!(wf.is_none(), "No workflow should match unrelated detection");
        });
    }

    #[tokio::test]
    async fn regression_session_end_full_execution() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_exec_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner(&db_path_str).await;
        runner.register_workflow(Arc::new(HandleSessionEnd::new()));

        let pane_id = 200u64;
        create_test_pane(&storage, pane_id).await;

        // Create a Codex session.summary detection
        let det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::json!({
                "total": "5000",
                "input": "3000",
                "output": "2000",
            }),
        );

        // Start the workflow
        let start = runner.handle_detection(pane_id, &det, None).await;
        assert!(
            start.is_started(),
            "Workflow should start for session.summary"
        );
        let execution_id = start.execution_id().unwrap().to_string();

        // Run the workflow
        let wf = runner.find_workflow_by_name("handle_session_end").unwrap();
        let result = runner.run_workflow(pane_id, wf, &execution_id, 0).await;
        assert!(
            result.is_completed(),
            "Session end workflow should complete: {result:?}"
        );

        // Verify step logs recorded
        let logs = storage.get_step_logs(&execution_id).await.unwrap();
        assert_eq!(logs.len(), 2, "Should have 2 step logs (extract + persist)");
        assert_eq!(logs[0].step_name, "extract_summary");
        assert_eq!(logs[1].step_name, "persist_record");

        // Verify session persisted
        // (The record was persisted via upsert_agent_session; we verify via step log result data)
        assert_eq!(logs[1].result_type, "done");

        // Verify usage metrics were recorded (token usage + duration)
        let metrics = storage
            .query_usage_metrics(crate::storage::MetricQuery {
                metric_type: None,
                agent_type: Some("codex".to_string()),
                account_id: None,
                since: Some(0),
                until: None,
                limit: Some(50),
            })
            .await
            .expect("query usage metrics");
        assert!(
            metrics
                .iter()
                .any(|m| m.metric_type == crate::storage::MetricType::TokenUsage),
            "Expected token usage metric"
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.metric_type == crate::storage::MetricType::SessionDuration),
            "Expected session duration metric"
        );
    }

    #[tokio::test]
    async fn regression_auth_required_full_execution() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_auth_exec_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner_with_panes(&db_path_str, &[201]).await;
        runner.register_workflow(Arc::new(HandleAuthRequired::new()));

        let pane_id = 201u64;
        create_test_pane(&storage, pane_id).await;

        // Create a device code detection
        let det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({
                "code": "ABCD-12345",
            }),
        );

        // Start the workflow
        let start = runner.handle_detection(pane_id, &det, None).await;
        assert!(
            start.is_started(),
            "Workflow should start for auth.device_code"
        );
        let execution_id = start.execution_id().unwrap().to_string();

        // Run the workflow
        let wf = runner
            .find_workflow_by_name("handle_auth_required")
            .unwrap();
        let result = runner.run_workflow(pane_id, wf, &execution_id, 0).await;
        assert!(
            result.is_completed(),
            "Auth required workflow should complete: {result:?}"
        );

        // Verify step logs
        let logs = storage.get_step_logs(&execution_id).await.unwrap();
        assert_eq!(
            logs.len(),
            3,
            "Should have 3 step logs (cooldown + classify + record)"
        );
        assert_eq!(logs[0].step_name, "check_cooldown");
        assert_eq!(logs[1].step_name, "classify_auth");
        assert_eq!(logs[2].step_name, "record_and_plan");

        // Verify audit record created
        let query = crate::storage::AuditQuery {
            pane_id: Some(pane_id),
            action_kind: Some("auth_required".to_string()),
            limit: Some(10),
            ..Default::default()
        };
        let audits = storage.get_audit_actions(query).await.unwrap();
        assert!(
            !audits.is_empty(),
            "Auth event should be recorded in audit log"
        );
        assert_eq!(audits[0].action_kind, "auth_required");
        assert_eq!(audits[0].pane_id, Some(pane_id));
    }

    #[tokio::test]
    async fn regression_auth_cooldown_skips_repeat() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_cooldown_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner_with_panes(&db_path_str, &[202]).await;
        runner.register_workflow(Arc::new(HandleAuthRequired::new()));

        let pane_id = 202u64;
        create_test_pane(&storage, pane_id).await;

        let det = make_session_detection(
            "codex.auth.device_code_prompt",
            AgentType::Codex,
            "auth.device_code",
            serde_json::json!({"code": "ABCD-12345"}),
        );

        // First run: should complete normally
        let start1 = runner.handle_detection(pane_id, &det, None).await;
        assert!(start1.is_started());
        let exec_id1 = start1.execution_id().unwrap().to_string();
        let wf = runner
            .find_workflow_by_name("handle_auth_required")
            .unwrap();
        let result1 = runner.run_workflow(pane_id, wf.clone(), &exec_id1, 0).await;
        assert!(result1.is_completed(), "First auth run should complete");

        // Second run: cooldown check should cause early completion (step 0 returns Done)
        let start2 = runner.handle_detection(pane_id, &det, None).await;
        assert!(start2.is_started());
        let exec_id2 = start2.execution_id().unwrap().to_string();
        let result2 = runner.run_workflow(pane_id, wf, &exec_id2, 0).await;
        assert!(
            result2.is_completed(),
            "Second auth run should complete (via cooldown skip)"
        );

        // Verify second run has fewer step logs (only 1 step: cooldown check → Done)
        let logs2 = storage.get_step_logs(&exec_id2).await.unwrap();
        assert_eq!(
            logs2.len(),
            1,
            "Cooldown-skipped run should have only 1 step log, got {}",
            logs2.len()
        );
        assert_eq!(logs2[0].step_name, "check_cooldown");
        assert_eq!(logs2[0].result_type, "done");
    }

    #[tokio::test]
    async fn regression_session_end_null_trigger_produces_sparse_record() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let db_path = std::env::temp_dir().join(format!(
            "wa_test_reg_no_trigger_{}_{}_{n}.db",
            std::process::id(),
            line!()
        ));
        let db_path_str = db_path.to_string_lossy().to_string();

        let (runner, storage, _lock) = create_test_runner(&db_path_str).await;
        runner.register_workflow(Arc::new(HandleSessionEnd::new()));

        let pane_id = 203u64;
        create_test_pane(&storage, pane_id).await;

        // Detection that matches session.summary (runner doesn't populate trigger in context)
        let det = make_session_detection(
            "codex.session.token_usage",
            AgentType::Codex,
            "session.summary",
            serde_json::Value::Null,
        );

        let start = runner.handle_detection(pane_id, &det, None).await;
        assert!(start.is_started());
        let exec_id = start.execution_id().unwrap().to_string();
        let wf = runner.find_workflow_by_name("handle_session_end").unwrap();
        let result = runner.run_workflow(pane_id, wf, &exec_id, 0).await;

        // The workflow completes even without trigger data — it produces a sparse record
        // with agent_type="unknown" and no extracted fields
        assert!(
            result.is_completed(),
            "Session end should complete even without trigger data: {result:?}"
        );

        let logs = storage.get_step_logs(&exec_id).await.unwrap();
        assert_eq!(
            logs.len(),
            2,
            "Should have 2 step logs even with sparse data"
        );
    }

    #[test]
    fn regression_codex_session_fixture_drift_check() {
        // Verify the Codex session parser produces correct records from known formats.
        // If Codex output drifts, this test fails and points to the exact field.
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "total": "12345",
                "input": "8000",
                "output": "4345",
                "cached": "2000",
                "reasoning": "1500",
                "session_id": "abc-def-123-456",
            }
        });
        let record = HandleSessionEnd::record_from_detection(100, &trigger);

        // Each field checked individually for actionable diagnostics
        assert_eq!(record.agent_type, "codex", "agent_type drift");
        assert_eq!(record.total_tokens, Some(12345), "total_tokens drift");
        assert_eq!(record.input_tokens, Some(8000), "input_tokens drift");
        assert_eq!(record.output_tokens, Some(4345), "output_tokens drift");
        assert_eq!(record.cached_tokens, Some(2000), "cached_tokens drift");
        assert_eq!(
            record.reasoning_tokens,
            Some(1500),
            "reasoning_tokens drift"
        );
        assert_eq!(
            record.session_id.as_deref(),
            Some("abc-def-123-456"),
            "session_id drift"
        );
        assert_eq!(
            record.end_reason.as_deref(),
            Some("completed"),
            "end_reason drift"
        );
    }

    #[test]
    fn regression_claude_code_session_fixture_drift_check() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "session.summary",
            "extracted": {
                "cost": "7.25",
            }
        });
        let record = HandleSessionEnd::record_from_detection(101, &trigger);

        assert_eq!(record.agent_type, "claude_code", "agent_type drift");
        assert_eq!(record.estimated_cost_usd, Some(7.25), "cost drift");
        assert!(
            record.total_tokens.is_none(),
            "Claude Code should not have tokens"
        );
        assert_eq!(
            record.end_reason.as_deref(),
            Some("completed"),
            "end_reason drift"
        );
    }

    #[test]
    fn regression_gemini_session_fixture_drift_check() {
        let trigger = serde_json::json!({
            "agent_type": "gemini",
            "event_type": "session.summary",
            "extracted": {
                "session_id": "aaaa-bbbb-cccc-dddd",
                "tool_calls": "42",
            }
        });
        let record = HandleSessionEnd::record_from_detection(102, &trigger);

        assert_eq!(record.agent_type, "gemini", "agent_type drift");
        assert_eq!(
            record.session_id.as_deref(),
            Some("aaaa-bbbb-cccc-dddd"),
            "session_id drift"
        );
        assert!(
            record.total_tokens.is_none(),
            "Gemini fixture should not have tokens"
        );
        assert!(
            record.estimated_cost_usd.is_none(),
            "Gemini fixture should not have cost"
        );
        assert_eq!(
            record.end_reason.as_deref(),
            Some("completed"),
            "end_reason drift"
        );
    }

    #[test]
    fn regression_auth_strategy_device_code_fixture_drift() {
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "auth.device_code",
            "rule_id": "codex.auth.device_code_prompt",
            "extracted": {
                "code": "WXYZ-98765",
                "url": "https://auth.openai.com/device",
            }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        match &strategy {
            AuthRecoveryStrategy::DeviceCode { code, url } => {
                assert_eq!(code.as_deref(), Some("WXYZ-98765"), "code drift");
                assert_eq!(
                    url.as_deref(),
                    Some("https://auth.openai.com/device"),
                    "url drift"
                );
            }
            other => panic!("Expected DeviceCode strategy, got: {other:?}"),
        }
    }

    #[test]
    fn regression_auth_strategy_api_key_fixture_drift() {
        let trigger = serde_json::json!({
            "agent_type": "claude_code",
            "event_type": "auth.error",
            "rule_id": "claude_code.auth.api_key_error",
            "extracted": {
                "key_name": "ANTHROPIC_API_KEY",
            }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        match &strategy {
            AuthRecoveryStrategy::ApiKeyError { key_hint } => {
                assert_eq!(
                    key_hint.as_deref(),
                    Some("ANTHROPIC_API_KEY"),
                    "key_hint drift"
                );
            }
            other => panic!("Expected ApiKeyError strategy, got: {other:?}"),
        }
    }

    #[test]
    fn regression_broken_fixture_produces_readable_error() {
        // Intentionally broken: mismatched agent_type field name in extracted
        let trigger = serde_json::json!({
            "agent_type": "codex",
            "event_type": "session.summary",
            "extracted": {
                "WRONG_total": "5000",
                "WRONG_input": "3000",
            }
        });
        let record = HandleSessionEnd::record_from_detection(999, &trigger);
        // Fields should gracefully be None, not crash
        assert!(
            record.total_tokens.is_none(),
            "Wrong field name should not parse as total"
        );
        assert!(
            record.input_tokens.is_none(),
            "Wrong field name should not parse as input"
        );
        // Agent type still correctly extracted from top-level
        assert_eq!(record.agent_type, "codex");
    }

    // ========================================================================
    // Device Auth Step Tests (wa-nu4.1.3.6)
    // ========================================================================

    #[cfg(feature = "browser")]
    mod device_auth_step_tests {
        use super::*;

        // -- DeviceAuthStepOutcome serde tests --

        #[test]
        fn outcome_authenticated_serde() {
            let outcome = DeviceAuthStepOutcome::Authenticated {
                elapsed_ms: 5432,
                account: "work".into(),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"authenticated""#));
            assert!(json.contains(r#""elapsed_ms":5432"#));
            assert!(json.contains(r#""account":"work""#));

            let parsed: DeviceAuthStepOutcome = serde_json::from_str(&json).unwrap();
            match parsed {
                DeviceAuthStepOutcome::Authenticated {
                    elapsed_ms,
                    account,
                } => {
                    assert_eq!(elapsed_ms, 5432);
                    assert_eq!(account, "work");
                }
                _ => panic!("Expected Authenticated variant"),
            }
        }

        #[test]
        fn outcome_bootstrap_required_serde() {
            let outcome = DeviceAuthStepOutcome::BootstrapRequired {
                reason: "MFA required".into(),
                account: "default".into(),
                artifacts_dir: Some(std::path::PathBuf::from("/tmp/artifacts")),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"bootstrap_required""#));
            assert!(json.contains(r#""reason":"MFA required""#));
            assert!(json.contains("artifacts_dir"));

            let parsed: DeviceAuthStepOutcome = serde_json::from_str(&json).unwrap();
            match parsed {
                DeviceAuthStepOutcome::BootstrapRequired {
                    reason,
                    account,
                    artifacts_dir,
                } => {
                    assert_eq!(reason, "MFA required");
                    assert_eq!(account, "default");
                    assert!(artifacts_dir.is_some());
                }
                _ => panic!("Expected BootstrapRequired variant"),
            }
        }

        #[test]
        fn outcome_bootstrap_required_omits_none_artifacts() {
            let outcome = DeviceAuthStepOutcome::BootstrapRequired {
                reason: "Password needed".into(),
                account: "default".into(),
                artifacts_dir: None,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(!json.contains("artifacts_dir"));
        }

        #[test]
        fn outcome_failed_serde_batch2() {
            let outcome = DeviceAuthStepOutcome::Failed {
                error: "Playwright crashed".into(),
                error_kind: Some("PlaywrightError".into()),
                artifacts_dir: None,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"failed""#));
            assert!(json.contains(r#""error":"Playwright crashed""#));
            assert!(json.contains(r#""error_kind":"PlaywrightError""#));
            assert!(!json.contains("artifacts_dir"));
        }

        #[test]
        fn outcome_failed_omits_none_kind() {
            let outcome = DeviceAuthStepOutcome::Failed {
                error: "unknown".into(),
                error_kind: None,
                artifacts_dir: None,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(!json.contains("error_kind"));
        }

        // -- execute_device_auth_step: code validation --

        #[test]
        fn step_rejects_invalid_device_code() {
            let tmp =
                std::env::temp_dir().join(format!("wa_device_auth_test_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("not-valid", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed {
                    error, error_kind, ..
                } => {
                    assert!(error.contains("Invalid device code"));
                    assert_eq!(error_kind.as_deref(), Some("invalid_code"));
                }
                other => panic!("Expected Failed, got {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_rejects_empty_device_code() {
            let tmp = std::env::temp_dir()
                .join(format!("wa_device_auth_test_empty_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed { error_kind, .. } => {
                    assert_eq!(error_kind.as_deref(), Some("invalid_code"));
                }
                other => panic!("Expected Failed, got {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_rejects_short_parts() {
            let tmp = std::env::temp_dir()
                .join(format!("wa_device_auth_test_short_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("AB-CD", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed { error_kind, .. } => {
                    assert_eq!(error_kind.as_deref(), Some("invalid_code"));
                }
                other => panic!("Expected Failed, got {other:?}"),
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_accepts_valid_code_format_then_fails_browser() {
            // A valid code format will pass validation but fail at browser init
            // (Playwright not installed in test env). This verifies the step
            // progresses past validation to the browser phase.
            let tmp = std::env::temp_dir()
                .join(format!("wa_device_auth_test_valid_{}", std::process::id()));
            let _ = std::fs::create_dir_all(&tmp);

            let outcome = execute_device_auth_step("ABCD-EFGH", "default", &tmp, None, true);
            match outcome {
                DeviceAuthStepOutcome::Failed {
                    error, error_kind, ..
                } => {
                    // Should fail at browser init, not code validation
                    assert_ne!(error_kind.as_deref(), Some("invalid_code"));
                    assert!(
                        error.contains("Browser")
                            || error.contains("browser")
                            || error.contains("Playwright")
                            || error.contains("playwright")
                            || error_kind.as_deref() == Some("browser_not_ready"),
                        "Expected browser-related error, got: {error}"
                    );
                }
                // If somehow browser init succeeds (unlikely in CI), that's also fine
                DeviceAuthStepOutcome::Authenticated { .. } => {}
                DeviceAuthStepOutcome::BootstrapRequired { .. } => {}
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[test]
        fn step_with_alphanumeric_code() {
            let tmp = std::env::temp_dir().join(format!(
                "wa_device_auth_test_alphanum_{}",
                std::process::id()
            ));
            let _ = std::fs::create_dir_all(&tmp);

            // Alphanumeric codes (including digits) should pass validation
            let outcome = execute_device_auth_step("AB12-CD34", "default", &tmp, None, true);
            // Should NOT fail with invalid_code
            match &outcome {
                DeviceAuthStepOutcome::Failed { error_kind, .. } => {
                    assert_ne!(error_kind.as_deref(), Some("invalid_code"));
                }
                _ => {} // Authenticated or BootstrapRequired are both fine
            }

            let _ = std::fs::remove_dir_all(&tmp);
        }

        // -- device_auth_outcome_to_step_result mapping --

        #[test]
        fn outcome_to_step_result_authenticated() {
            let outcome = DeviceAuthStepOutcome::Authenticated {
                elapsed_ms: 1000,
                account: "default".into(),
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            match step {
                StepResult::Done { result } => {
                    assert!(result.get("status").is_some());
                    assert_eq!(
                        result.get("status").and_then(|v| v.as_str()),
                        Some("authenticated")
                    );
                }
                other => panic!("Expected Done, got {other:?}"),
            }
        }

        #[test]
        fn outcome_to_step_result_bootstrap_required() {
            let outcome = DeviceAuthStepOutcome::BootstrapRequired {
                reason: "MFA".into(),
                account: "work".into(),
                artifacts_dir: None,
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            match step {
                StepResult::Abort { reason } => {
                    assert!(reason.contains("bootstrap"));
                    assert!(reason.contains("work"));
                    assert!(reason.contains("MFA"));
                }
                other => panic!("Expected Abort, got {other:?}"),
            }
        }

        #[test]
        fn outcome_to_step_result_failed() {
            let outcome = DeviceAuthStepOutcome::Failed {
                error: "Selector mismatch".into(),
                error_kind: Some("SelectorMismatch".into()),
                artifacts_dir: None,
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            match step {
                StepResult::Abort { reason } => {
                    assert!(reason.contains("Selector mismatch"));
                }
                other => panic!("Expected Abort, got {other:?}"),
            }
        }

        #[test]
        fn outcome_to_step_result_authenticated_contains_elapsed() {
            let outcome = DeviceAuthStepOutcome::Authenticated {
                elapsed_ms: 7890,
                account: "test".into(),
            };
            let step = device_auth_outcome_to_step_result(&outcome);
            if let StepResult::Done { result } = step {
                assert_eq!(
                    result.get("elapsed_ms").and_then(|v| v.as_u64()),
                    Some(7890)
                );
            }
        }
    }

    // ========================================================================
    // Resume Session Step Tests (wa-nu4.1.3.7)
    // ========================================================================

    mod resume_session_step_tests {
        use super::*;

        // -- ResumeSessionConfig --

        #[test]
        fn config_defaults() {
            let config = ResumeSessionConfig::default();
            assert!(config.resume_command_template.contains("{session_id}"));
            assert_eq!(config.proceed_text, "proceed.\n");
            assert_eq!(config.post_resume_stable_ms, 3_000);
            assert_eq!(config.post_proceed_stable_ms, 5_000);
            assert_eq!(config.resume_timeout_ms, 30_000);
            assert_eq!(config.proceed_timeout_ms, 30_000);
        }

        #[test]
        fn config_serde_round_trip() {
            let config = ResumeSessionConfig::default();
            let json = serde_json::to_string(&config).unwrap();
            let parsed: ResumeSessionConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed.resume_command_template,
                config.resume_command_template
            );
            assert_eq!(parsed.proceed_text, config.proceed_text);
            assert_eq!(parsed.post_resume_stable_ms, config.post_resume_stable_ms);
        }

        // -- format_resume_command --

        #[test]
        fn format_resume_command_default() {
            let config = ResumeSessionConfig::default();
            let cmd = format_resume_command("abc123-def456", &config);
            assert_eq!(cmd, "cod resume abc123-def456\n");
        }

        #[test]
        fn format_resume_command_custom_template() {
            let config = ResumeSessionConfig {
                resume_command_template: "codex resume {session_id} --continue\n".into(),
                ..Default::default()
            };
            let cmd = format_resume_command("session-99", &config);
            assert_eq!(cmd, "codex resume session-99 --continue\n");
        }

        #[test]
        fn format_resume_command_no_placeholder() {
            let config = ResumeSessionConfig {
                resume_command_template: "fixed-command\n".into(),
                ..Default::default()
            };
            let cmd = format_resume_command("ignored-id", &config);
            assert_eq!(cmd, "fixed-command\n");
        }

        // -- validate_session_id --

        #[test]
        fn validate_session_id_valid_uuid() {
            assert!(validate_session_id("a1b2c3d4-e5f6-7890-abcd-ef1234567890"));
        }

        #[test]
        fn validate_session_id_valid_short_hex() {
            assert!(validate_session_id("abcdef01"));
        }

        #[test]
        fn validate_session_id_valid_with_hyphens() {
            assert!(validate_session_id("abc-def-123"));
        }

        #[test]
        fn validate_session_id_rejects_too_short() {
            assert!(!validate_session_id("abc"));
            assert!(!validate_session_id("1234567")); // 7 chars
        }

        #[test]
        fn validate_session_id_rejects_empty() {
            assert!(!validate_session_id(""));
        }

        #[test]
        fn validate_session_id_rejects_non_hex() {
            assert!(!validate_session_id("zzzzzzzz"));
            assert!(!validate_session_id("abc!defg"));
        }

        #[test]
        fn validate_session_id_trims_whitespace() {
            assert!(validate_session_id("  abcdef01  "));
        }

        // -- ResumeSessionOutcome serde --

        #[test]
        fn outcome_ready_serde() {
            let outcome = ResumeSessionOutcome::Ready {
                session_id: "abc-123".into(),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"ready""#));
            assert!(json.contains(r#""session_id":"abc-123""#));

            let parsed: ResumeSessionOutcome = serde_json::from_str(&json).unwrap();
            match parsed {
                ResumeSessionOutcome::Ready { session_id } => {
                    assert_eq!(session_id, "abc-123");
                }
                _ => panic!("Expected Ready variant"),
            }
        }

        #[test]
        fn outcome_verify_timeout_serde() {
            let outcome = ResumeSessionOutcome::VerifyTimeout {
                session_id: "def-456".into(),
                phase: "proceed".into(),
                waited_ms: 30_000,
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"timeout""#));
            assert!(json.contains(r#""phase":"proceed""#));
            assert!(json.contains(r#""waited_ms":30000"#));
        }

        #[test]
        fn outcome_failed_serde() {
            let outcome = ResumeSessionOutcome::Failed {
                error: "Policy denied".into(),
            };
            let json = serde_json::to_string(&outcome).unwrap();
            assert!(json.contains(r#""status":"failed""#));
            assert!(json.contains(r#""error":"Policy denied""#));
        }

        // -- build_resume_step_result --

        #[test]
        fn resume_step_result_is_send_text() {
            let config = ResumeSessionConfig::default();
            let result = build_resume_step_result("abc-def-123", &config);
            assert!(result.is_send_text());

            if let StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            } = result
            {
                assert_eq!(text, "cod resume abc-def-123\n");
                assert!(wait_for.is_some());
                match wait_for.unwrap() {
                    WaitCondition::StableTail {
                        pane_id,
                        stable_for_ms,
                    } => {
                        assert_eq!(pane_id, None);
                        assert_eq!(stable_for_ms, 3_000);
                    }
                    other => panic!("Expected StableTail, got {other:?}"),
                }
                assert_eq!(wait_timeout_ms, Some(30_000));
            }
        }

        // -- build_proceed_step_result --

        #[test]
        fn proceed_step_result_is_send_text() {
            let config = ResumeSessionConfig::default();
            let result = build_proceed_step_result(&config);
            assert!(result.is_send_text());

            if let StepResult::SendText {
                text,
                wait_for,
                wait_timeout_ms,
            } = result
            {
                assert_eq!(text, "proceed.\n");
                assert!(wait_for.is_some());
                match wait_for.unwrap() {
                    WaitCondition::StableTail { stable_for_ms, .. } => {
                        assert_eq!(stable_for_ms, 5_000);
                    }
                    other => panic!("Expected StableTail, got {other:?}"),
                }
                assert_eq!(wait_timeout_ms, Some(30_000));
            }
        }

        // -- resume_outcome_to_step_result mapping --

        #[test]
        fn outcome_to_step_result_ready_is_done() {
            let outcome = ResumeSessionOutcome::Ready {
                session_id: "abc".into(),
            };
            let step = resume_outcome_to_step_result(&outcome);
            assert!(step.is_done());

            if let StepResult::Done { result } = step {
                assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("ready"));
            }
        }

        #[test]
        fn outcome_to_step_result_timeout_is_done() {
            // Timeouts are soft failures — still Done, not Abort
            let outcome = ResumeSessionOutcome::VerifyTimeout {
                session_id: "abc".into(),
                phase: "resume".into(),
                waited_ms: 30_000,
            };
            let step = resume_outcome_to_step_result(&outcome);
            assert!(step.is_done());

            if let StepResult::Done { result } = step {
                assert_eq!(
                    result.get("status").and_then(|v| v.as_str()),
                    Some("timeout")
                );
            }
        }

        #[test]
        fn outcome_to_step_result_failed_is_abort() {
            let outcome = ResumeSessionOutcome::Failed {
                error: "Policy denied send".into(),
            };
            let step = resume_outcome_to_step_result(&outcome);
            assert!(step.is_terminal());

            match step {
                StepResult::Abort { reason } => {
                    assert!(reason.contains("Policy denied send"));
                }
                other => panic!("Expected Abort, got {other:?}"),
            }
        }

        // -- Custom config --

        #[test]
        fn custom_config_affects_step_results() {
            let config = ResumeSessionConfig {
                resume_command_template: "codex resume {session_id}\n".into(),
                proceed_text: "continue.\n".into(),
                post_resume_stable_ms: 1_000,
                post_proceed_stable_ms: 2_000,
                resume_timeout_ms: 10_000,
                proceed_timeout_ms: 15_000,
            };

            let resume = build_resume_step_result("session-42", &config);
            if let StepResult::SendText {
                text,
                wait_timeout_ms,
                ..
            } = resume
            {
                assert_eq!(text, "codex resume session-42\n");
                assert_eq!(wait_timeout_ms, Some(10_000));
            }

            let proceed = build_proceed_step_result(&config);
            if let StepResult::SendText {
                text,
                wait_timeout_ms,
                ..
            } = proceed
            {
                assert_eq!(text, "continue.\n");
                assert_eq!(wait_timeout_ms, Some(15_000));
            }
        }
    }

    // ========================================================================
    // Safe Fallback Path Tests (wa-nu4.1.3.8)
    // ========================================================================
    mod fallback_path_tests {
        use super::*;

        // -- FallbackReason --

        #[test]
        fn fallback_reason_needs_human_auth_display() {
            let reason = FallbackReason::NeedsHumanAuth {
                account: "openai-team".into(),
                detail: "MFA required".into(),
            };
            let s = reason.to_string();
            assert!(s.contains("openai-team"), "should contain account: {s}");
            assert!(s.contains("MFA required"), "should contain detail: {s}");
        }

        #[test]
        fn fallback_reason_failover_disabled_display() {
            let reason = FallbackReason::FailoverDisabled;
            assert!(reason.to_string().contains("disabled"));
        }

        #[test]
        fn fallback_reason_tool_missing_display() {
            let reason = FallbackReason::ToolMissing {
                tool: "playwright".into(),
            };
            assert!(reason.to_string().contains("playwright"));
        }

        #[test]
        fn fallback_reason_policy_denied_display() {
            let reason = FallbackReason::PolicyDenied {
                rule: "alt_screen_active".into(),
            };
            assert!(reason.to_string().contains("alt_screen_active"));
        }

        #[test]
        fn fallback_reason_all_accounts_exhausted_display() {
            let reason = FallbackReason::AllAccountsExhausted {
                accounts_checked: 3,
            };
            let s = reason.to_string();
            assert!(s.contains("3"), "should contain count: {s}");
        }

        #[test]
        fn fallback_reason_other_display() {
            let reason = FallbackReason::Other {
                detail: "unexpected error".into(),
            };
            assert!(reason.to_string().contains("unexpected error"));
        }

        #[test]
        fn fallback_reason_serde_round_trip() {
            let reasons = vec![
                FallbackReason::NeedsHumanAuth {
                    account: "acct-1".into(),
                    detail: "password required".into(),
                },
                FallbackReason::FailoverDisabled,
                FallbackReason::ToolMissing {
                    tool: "caut".into(),
                },
                FallbackReason::PolicyDenied {
                    rule: "recent_gap".into(),
                },
                FallbackReason::AllAccountsExhausted {
                    accounts_checked: 5,
                },
                FallbackReason::Other {
                    detail: "test".into(),
                },
            ];

            for reason in &reasons {
                let json = serde_json::to_string(reason).unwrap();
                let parsed: FallbackReason = serde_json::from_str(&json).unwrap();
                // Verify the kind tag survived round-trip
                let json2 = serde_json::to_string(&parsed).unwrap();
                assert_eq!(json, json2, "Round-trip mismatch for {reason:?}");
            }
        }

        // -- FallbackNextStepPlan --

        #[test]
        fn plan_version_is_current() {
            assert_eq!(FallbackNextStepPlan::CURRENT_VERSION, 1);
        }

        #[test]
        fn needs_human_auth_plan_structure() {
            let plan = build_needs_human_auth_plan(
                42,
                "openai-team",
                "MFA required",
                Some("sess-abc123"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            assert_eq!(plan.version, 1);
            assert_eq!(plan.pane_id, 42);
            assert!(matches!(plan.reason, FallbackReason::NeedsHumanAuth { .. }));
            assert_eq!(plan.resume_session_id.as_deref(), Some("sess-abc123"));
            assert_eq!(plan.account_id.as_deref(), Some("openai-team"));
            assert_eq!(plan.retry_after_ms, Some(1_700_000_000_000));
            assert!(!plan.operator_steps.is_empty());
            assert!(!plan.suggested_commands.is_empty());
            assert_eq!(plan.created_at_ms, 1_699_999_000_000);

            // Operator steps should mention bootstrap and resume
            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("bootstrap"),
                "steps should mention bootstrap: {steps_text}"
            );
            assert!(
                steps_text.contains("sess-abc123"),
                "steps should mention session ID: {steps_text}"
            );
        }

        #[test]
        fn needs_human_auth_plan_without_session_id() {
            let plan = build_needs_human_auth_plan(10, "acct", "SSO needed", None, None, 1_000_000);

            assert!(plan.resume_session_id.is_none());
            assert!(plan.retry_after_ms.is_none());
            // Should not mention resume in steps
            let steps_text = plan.operator_steps.join(" ");
            assert!(
                !steps_text.contains("resume"),
                "should not mention resume without session ID: {steps_text}"
            );
        }

        #[test]
        fn failover_disabled_plan_structure() {
            let plan = build_failover_disabled_plan(
                99,
                Some("sess-xyz"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            assert!(matches!(plan.reason, FallbackReason::FailoverDisabled));
            assert_eq!(plan.pane_id, 99);
            assert_eq!(plan.resume_session_id.as_deref(), Some("sess-xyz"));

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("disabled"),
                "should mention disabled: {steps_text}"
            );
        }

        #[test]
        fn tool_missing_plan_structure() {
            let plan = build_tool_missing_plan(7, "playwright", 1_000_000);

            assert!(matches!(plan.reason, FallbackReason::ToolMissing { .. }));
            assert_eq!(plan.pane_id, 7);
            assert!(plan.resume_session_id.is_none());
            assert!(plan.retry_after_ms.is_none());

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("playwright"),
                "should mention missing tool: {steps_text}"
            );
        }

        #[test]
        fn all_accounts_exhausted_plan_with_retry() {
            let plan = build_all_accounts_exhausted_plan(
                55,
                4,
                Some("sess-111"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            assert!(matches!(
                plan.reason,
                FallbackReason::AllAccountsExhausted {
                    accounts_checked: 4
                }
            ));
            assert_eq!(plan.pane_id, 55);

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("4"),
                "should mention account count: {steps_text}"
            );
            assert!(
                steps_text.contains("reset"),
                "should mention reset when retry_after is set: {steps_text}"
            );
        }

        #[test]
        fn all_accounts_exhausted_plan_without_retry() {
            let plan = build_all_accounts_exhausted_plan(55, 2, None, None, 1_000_000);

            let steps_text = plan.operator_steps.join(" ");
            assert!(
                steps_text.contains("ft accounts status"),
                "should suggest checking accounts: {steps_text}"
            );
        }

        // -- Step result conversion --

        #[test]
        fn fallback_plan_to_step_result_is_done() {
            let plan = build_needs_human_auth_plan(1, "acct", "MFA", None, None, 1_000_000);
            let result = fallback_plan_to_step_result(&plan);
            assert!(result.is_done(), "fallback should produce Done, not Abort");
            assert!(result.is_terminal());
        }

        #[test]
        fn fallback_plan_to_step_result_has_fallback_flag() {
            let plan = build_failover_disabled_plan(1, None, None, 1_000_000);
            let result = fallback_plan_to_step_result(&plan);
            assert!(is_fallback_result(&result), "should be tagged as fallback");
        }

        #[test]
        fn is_fallback_result_false_for_normal_done() {
            let result = StepResult::done(serde_json::json!({"status": "ok"}));
            assert!(
                !is_fallback_result(&result),
                "normal Done should not be fallback"
            );
        }

        #[test]
        fn is_fallback_result_false_for_abort() {
            let result = StepResult::abort("test");
            assert!(!is_fallback_result(&result), "Abort should not be fallback");
        }

        #[test]
        fn fallback_handled_status_is_paused() {
            assert_eq!(FALLBACK_HANDLED_STATUS, "paused");
        }

        // -- Serde round-trip for full plan --

        #[test]
        fn plan_serde_round_trip() {
            let plan = build_needs_human_auth_plan(
                42,
                "openai-team",
                "MFA required",
                Some("sess-abc123"),
                Some(1_700_000_000_000),
                1_699_999_000_000,
            );

            let json = serde_json::to_string(&plan).unwrap();
            let parsed: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();

            assert_eq!(parsed.version, plan.version);
            assert_eq!(parsed.pane_id, plan.pane_id);
            assert_eq!(parsed.resume_session_id, plan.resume_session_id);
            assert_eq!(parsed.account_id, plan.account_id);
            assert_eq!(parsed.retry_after_ms, plan.retry_after_ms);
            assert_eq!(parsed.operator_steps.len(), plan.operator_steps.len());
            assert_eq!(
                parsed.suggested_commands.len(),
                plan.suggested_commands.len()
            );
            assert_eq!(parsed.created_at_ms, plan.created_at_ms);
        }

        #[test]
        fn plan_skips_none_fields_in_json() {
            let plan = build_tool_missing_plan(1, "caut", 1_000_000);
            let json = serde_json::to_string(&plan).unwrap();

            // Optional None fields should be absent
            assert!(
                !json.contains("retry_after_ms"),
                "None fields should be skipped: {json}"
            );
            assert!(
                !json.contains("resume_session_id"),
                "None fields should be skipped: {json}"
            );
            assert!(
                !json.contains("account_id"),
                "None fields should be skipped: {json}"
            );
        }

        #[test]
        fn fallback_step_result_preserves_plan_fields() {
            let plan =
                build_needs_human_auth_plan(42, "acct", "MFA", Some("sess-1"), None, 1_000_000);
            let result = fallback_plan_to_step_result(&plan);

            if let StepResult::Done { result } = result {
                // The plan fields plus the "fallback" flag should all be present
                let map = result.as_object().unwrap();
                assert!(map.contains_key("version"));
                assert!(map.contains_key("reason"));
                assert!(map.contains_key("pane_id"));
                assert!(map.contains_key("operator_steps"));
                assert!(map.contains_key("fallback"));
                assert_eq!(map["fallback"], true);
                assert_eq!(map["pane_id"], 42);
            } else {
                panic!("Expected Done variant");
            }
        }
    }

    // ========================================================================
    // Usage-limit Workflow Tests (wa-nu4.1.3.9)
    // ========================================================================

    #[test]
    fn usage_limit_workflow_has_correct_name_and_steps() {
        let wf = HandleUsageLimits::new();
        assert_eq!(wf.name(), "handle_usage_limits");
        assert_eq!(
            wf.description(),
            "Exit agent, persist session summary, and select new account for failover"
        );

        let steps = wf.steps();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "exit_and_persist");
        assert_eq!(steps[2].name, "select_account");
    }

    #[test]
    fn usage_limit_workflow_handles_codex_usage_events() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "codex.usage_limit".to_string(),
            agent_type: AgentType::Codex,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "You've hit your usage limit".to_string(),
            span: (0, 0),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn usage_limit_workflow_ignores_non_codex_agents() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: AgentType::ClaudeCode,
            event_type: "usage.reached".to_string(),
            severity: Severity::Critical,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "rate limit".to_string(),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn usage_limit_workflow_ignores_non_usage_events() {
        let wf = HandleUsageLimits::new();

        let detection = Detection {
            rule_id: "codex.session.end".to_string(),
            agent_type: AgentType::Codex,
            event_type: "session.summary".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "Session ended normally".to_string(),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    // -- Fixture: Real-world usage limit transcript --

    const FIXTURE_FULL_USAGE_LIMIT: &str = r"
$ cod start
Starting Codex session...
Working directory: /data/projects/my-app

You've hit your usage limit. Try again at 3:00 PM UTC.

Token usage: total=1,234,567 input=500,000 (+ 200,000 cached) output=534,567 (reasoning 100,000)
To continue this session, run: codex resume 123e4567-e89b-12d3-a456-426614174000
$";

    const FIXTURE_MINIMAL_USAGE_LIMIT: &str =
        "Token usage: total=42\ncodex resume aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    const FIXTURE_NO_RESET_TIME: &str = r"
You've hit your usage limit.
Token usage: total=100 input=50 output=50
codex resume 11111111-2222-3333-4444-555555555555";

    const FIXTURE_MISSING_SESSION_ID: &str = r"
Token usage: total=100 input=50
You've hit your usage limit. Try again at 5:00 PM.";

    const FIXTURE_MISSING_TOKEN_USAGE: &str =
        "codex resume 11111111-2222-3333-4444-555555555555\nSome other output";

    const FIXTURE_EMPTY: &str = "";

    const FIXTURE_GARBAGE: &str = "random noise\n123\nno markers here";

    #[test]
    fn fixture_full_usage_limit_parses_all_fields() {
        let result = parse_codex_session_summary(FIXTURE_FULL_USAGE_LIMIT).expect("should parse");
        assert_eq!(result.session_id, "123e4567-e89b-12d3-a456-426614174000");
        assert_eq!(result.token_usage.total, Some(1_234_567));
        assert_eq!(result.token_usage.input, Some(500_000));
        assert_eq!(result.token_usage.cached, Some(200_000));
        assert_eq!(result.token_usage.output, Some(534_567));
        assert_eq!(result.token_usage.reasoning, Some(100_000));
        assert_eq!(result.reset_time.as_deref(), Some("3:00 PM UTC"));
    }

    #[test]
    fn fixture_minimal_parses_required_fields() {
        let result =
            parse_codex_session_summary(FIXTURE_MINIMAL_USAGE_LIMIT).expect("should parse");
        assert_eq!(result.session_id, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(result.token_usage.total, Some(42));
        assert!(result.token_usage.input.is_none());
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn fixture_no_reset_time_parses_without_reset() {
        let result = parse_codex_session_summary(FIXTURE_NO_RESET_TIME).expect("should parse");
        assert_eq!(result.session_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(result.token_usage.total, Some(100));
        assert!(result.reset_time.is_none());
    }

    #[test]
    fn fixture_missing_session_id_fails_gracefully() {
        let err = parse_codex_session_summary(FIXTURE_MISSING_SESSION_ID).expect_err("should fail");
        assert!(err.missing.contains(&"session_id"));
        assert!(!err.missing.contains(&"token_usage"));
        assert!(err.tail_len > 0);
        assert!(err.tail_hash != 0);
    }

    #[test]
    fn fixture_missing_token_usage_fails_gracefully() {
        let err =
            parse_codex_session_summary(FIXTURE_MISSING_TOKEN_USAGE).expect_err("should fail");
        assert!(err.missing.contains(&"token_usage"));
        assert!(!err.missing.contains(&"session_id"));
    }

    #[test]
    fn fixture_empty_fails_with_both_missing() {
        let err = parse_codex_session_summary(FIXTURE_EMPTY).expect_err("should fail");
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn fixture_garbage_fails_with_both_missing() {
        let err = parse_codex_session_summary(FIXTURE_GARBAGE).expect_err("should fail");
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn token_usage_extraction_handles_partial_line() {
        let line = "Token usage: total=500 output=300";
        let usage = extract_token_usage(line);
        assert_eq!(usage.total, Some(500));
        assert_eq!(usage.output, Some(300));
        assert!(usage.input.is_none());
        assert!(usage.cached.is_none());
        assert!(usage.reasoning.is_none());
    }

    #[test]
    fn token_usage_extraction_handles_empty_line() {
        let usage = extract_token_usage("");
        assert!(usage.total.is_none());
        assert!(!usage.has_any());
    }

    #[test]
    fn session_record_from_summary_maps_all_fields() {
        let summary = CodexSessionSummary {
            session_id: "test-session-1234".to_string(),
            token_usage: CodexTokenUsage {
                total: Some(1000),
                input: Some(400),
                output: Some(500),
                cached: Some(100),
                reasoning: Some(50),
            },
            reset_time: Some("5:00 PM".to_string()),
        };

        let record = codex_session_record_from_summary(42, &summary);
        assert_eq!(record.pane_id, 42);
        assert_eq!(record.agent_type, "codex");
        assert_eq!(record.session_id.as_deref(), Some("test-session-1234"));
        assert_eq!(record.total_tokens, Some(1000));
        assert_eq!(record.input_tokens, Some(400));
        assert_eq!(record.output_tokens, Some(500));
        assert_eq!(record.cached_tokens, Some(100));
        assert_eq!(record.reasoning_tokens, Some(50));
    }

    #[test]
    fn usage_limit_default_impl() {
        let wf = HandleUsageLimits::default();
        assert_eq!(wf.name(), "handle_usage_limits");
    }

    #[test]
    fn parse_error_display_is_actionable() {
        let err = CodexSessionParseError {
            missing: vec!["session_id", "token_usage"],
            tail_hash: 0xdeadbeef,
            tail_len: 42,
        };
        let display = err.to_string();
        assert!(display.contains("session_id"));
        assert!(display.contains("token_usage"));
        assert!(display.contains("tail_hash="));
        assert!(display.contains("tail_len=42"));
    }

    #[test]
    fn find_session_id_prefers_last_occurrence() {
        let tail = "codex resume aaaa1234\nsome text\ncodex resume bbbb5678";
        let id = find_session_id(tail).unwrap();
        assert_eq!(id, "bbbb5678");
    }

    #[test]
    fn find_session_id_returns_none_on_no_match() {
        assert!(find_session_id("no resume hint here").is_none());
    }

    #[test]
    fn find_reset_time_extracts_from_try_again_at() {
        let tail = "Try again at 10:30 AM EST.";
        let time = find_reset_time(tail).unwrap();
        assert_eq!(time, "10:30 AM EST");
    }

    #[test]
    fn find_reset_time_returns_none_when_absent() {
        assert!(find_reset_time("no reset time here").is_none());
    }

    #[test]
    fn find_token_usage_line_finds_last_occurrence() {
        let tail = "Token usage: first\nsome middle\nToken usage: second";
        let line = find_token_usage_line(tail).unwrap();
        assert!(line.contains("second"));
    }

    #[test]
    fn parse_number_handles_commas_and_edge_cases() {
        assert_eq!(parse_number("1,234,567"), Some(1_234_567));
        assert_eq!(parse_number("42"), Some(42));
        assert_eq!(parse_number(""), None);
        assert_eq!(parse_number("abc"), None);
    }

    // =========================================================================
    // RubyBeaver wa-1u90p.7.1 — additional pure unit tests
    // =========================================================================

    #[test]
    fn parse_number_negative() {
        assert_eq!(parse_number("-1"), Some(-1));
    }

    #[test]
    fn parse_number_zero() {
        assert_eq!(parse_number("0"), Some(0));
    }

    #[test]
    fn parse_number_large_with_commas() {
        assert_eq!(parse_number("1,000,000,000"), Some(1_000_000_000));
    }

    #[test]
    fn capture_number_from_total_regex() {
        assert_eq!(capture_number(&CODEX_TOTAL_RE, "total=42"), Some(42));
    }

    #[test]
    fn capture_number_from_input_regex() {
        assert_eq!(capture_number(&CODEX_INPUT_RE, "input=100"), Some(100));
    }

    #[test]
    fn capture_number_from_output_regex() {
        assert_eq!(capture_number(&CODEX_OUTPUT_RE, "output=200"), Some(200));
    }

    #[test]
    fn capture_number_no_match_returns_none() {
        assert_eq!(capture_number(&CODEX_TOTAL_RE, "nothing here"), None);
    }

    #[test]
    fn capture_number_cached_with_comma() {
        assert_eq!(
            capture_number(&CODEX_CACHED_RE, "(+ 1,234 cached)"),
            Some(1234)
        );
    }

    #[test]
    fn capture_number_reasoning() {
        assert_eq!(
            capture_number(&CODEX_REASONING_RE, "(reasoning 500)"),
            Some(500)
        );
    }

    #[test]
    fn extract_token_usage_full_line() {
        let line = "Token usage: total=1,000 input=400 output=500 (+ 50 cached) (reasoning 100)";
        let usage = extract_token_usage(line);
        assert_eq!(usage.total, Some(1000));
        assert_eq!(usage.input, Some(400));
        assert_eq!(usage.output, Some(500));
        assert_eq!(usage.cached, Some(50));
        assert_eq!(usage.reasoning, Some(100));
        assert!(usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_true_with_total_only() {
        let usage = CodexTokenUsage {
            total: Some(100),
            input: None,
            output: None,
            cached: None,
            reasoning: None,
        };
        assert!(usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_false_all_none() {
        let usage = CodexTokenUsage {
            total: None,
            input: None,
            output: None,
            cached: None,
            reasoning: None,
        };
        assert!(!usage.has_any());
    }

    #[test]
    fn codex_token_usage_has_any_true_with_cached_only() {
        let usage = CodexTokenUsage {
            total: None,
            input: None,
            output: None,
            cached: Some(42),
            reasoning: None,
        };
        assert!(usage.has_any());
    }

    #[test]
    fn codex_exit_options_default_grace_timeout() {
        let opts = CodexExitOptions::default();
        assert_eq!(opts.grace_timeout_ms, 2_000);
    }

    #[test]
    fn codex_exit_options_default_summary_timeout() {
        let opts = CodexExitOptions::default();
        assert_eq!(opts.summary_timeout_ms, 20_000);
    }

    #[test]
    fn codex_session_parse_error_display_includes_fields() {
        let err = CodexSessionParseError {
            missing: vec!["session_id"],
            tail_hash: 0xCAFE,
            tail_len: 100,
        };
        let msg = err.to_string();
        assert!(msg.contains("session_id"));
        assert!(msg.contains("tail_len=100"));
    }

    #[test]
    fn codex_session_parse_error_is_error_trait() {
        let err = CodexSessionParseError {
            missing: vec!["token_usage"],
            tail_hash: 0,
            tail_len: 0,
        };
        // Verify it implements std::error::Error
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn step_result_cont_builder() {
        assert!(matches!(StepResult::cont(), StepResult::Continue));
    }

    #[test]
    fn step_result_done_builder_with_value() {
        let result = StepResult::done(serde_json::json!({"key": "value"}));
        assert!(result.is_done());
        assert!(result.is_terminal());
    }

    #[test]
    fn step_result_done_empty_is_null() {
        if let StepResult::Done { result } = StepResult::done_empty() {
            assert!(result.is_null());
        } else {
            panic!("expected Done");
        }
    }

    #[test]
    fn step_result_retry_builder() {
        if let StepResult::Retry { delay_ms } = StepResult::retry(5000) {
            assert_eq!(delay_ms, 5000);
        } else {
            panic!("expected Retry");
        }
    }

    #[test]
    fn step_result_abort_builder() {
        if let StepResult::Abort { reason } = StepResult::abort("failed") {
            assert_eq!(reason, "failed");
        } else {
            panic!("expected Abort");
        }
    }

    #[test]
    fn step_result_wait_for_builder_default_timeout() {
        let result = StepResult::wait_for(WaitCondition::external("key"));
        if let StepResult::WaitFor { timeout_ms, .. } = result {
            assert!(timeout_ms.is_none());
        } else {
            panic!("expected WaitFor");
        }
    }

    #[test]
    fn step_result_wait_for_with_timeout_builder() {
        let result = StepResult::wait_for_with_timeout(WaitCondition::external("key"), 3000);
        if let StepResult::WaitFor { timeout_ms, .. } = result {
            assert_eq!(timeout_ms, Some(3000));
        } else {
            panic!("expected WaitFor");
        }
    }

    #[test]
    fn step_result_send_text_builder() {
        let result = StepResult::send_text("hello");
        assert!(result.is_send_text());
        if let StepResult::SendText {
            text,
            wait_for,
            wait_timeout_ms,
        } = result
        {
            assert_eq!(text, "hello");
            assert!(wait_for.is_none());
            assert!(wait_timeout_ms.is_none());
        } else {
            panic!("expected SendText");
        }
    }

    #[test]
    fn step_result_send_text_and_wait_builder() {
        let result = StepResult::send_text_and_wait("hello", WaitCondition::pane_idle(1000), 5000);
        assert!(result.is_send_text());
        if let StepResult::SendText {
            wait_for,
            wait_timeout_ms,
            ..
        } = result
        {
            assert!(wait_for.is_some());
            assert_eq!(wait_timeout_ms, Some(5000));
        } else {
            panic!("expected SendText");
        }
    }

    #[test]
    fn step_result_is_continue_only_for_continue() {
        assert!(StepResult::cont().is_continue());
        assert!(!StepResult::done_empty().is_continue());
        assert!(!StepResult::abort("e").is_continue());
        assert!(!StepResult::retry(1).is_continue());
    }

    #[test]
    fn step_result_is_send_text_only_for_send_text() {
        assert!(StepResult::send_text("x").is_send_text());
        assert!(!StepResult::cont().is_send_text());
        assert!(!StepResult::done_empty().is_send_text());
    }

    #[test]
    fn step_result_is_terminal_done_and_abort_only() {
        assert!(StepResult::done_empty().is_terminal());
        assert!(StepResult::abort("e").is_terminal());
        assert!(!StepResult::cont().is_terminal());
        assert!(!StepResult::retry(1).is_terminal());
        assert!(!StepResult::send_text("x").is_terminal());
        assert!(!StepResult::wait_for(WaitCondition::external("k")).is_terminal());
    }

    #[test]
    fn text_match_substring_builder() {
        let m = TextMatch::substring("hello");
        assert!(matches!(m, TextMatch::Substring { value } if value == "hello"));
    }

    #[test]
    fn text_match_regex_builder() {
        let m = TextMatch::regex(r"\d+");
        assert!(matches!(m, TextMatch::Regex { pattern } if pattern == r"\d+"));
    }

    #[test]
    fn text_match_description_substring() {
        let m = TextMatch::substring("test");
        let desc = m.description();
        assert!(desc.starts_with("substring(len=4"));
    }

    #[test]
    fn text_match_description_regex() {
        let m = TextMatch::regex(r"pat");
        let desc = m.description();
        assert!(desc.starts_with("regex(len=3"));
    }

    #[test]
    fn execution_status_all_variants_serde() {
        for (variant, expected) in [
            (ExecutionStatus::Running, "\"running\""),
            (ExecutionStatus::Waiting, "\"waiting\""),
            (ExecutionStatus::Completed, "\"completed\""),
            (ExecutionStatus::Aborted, "\"aborted\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let back: ExecutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    fn workflow_engine_default_max_concurrent() {
        let engine = WorkflowEngine::default();
        assert_eq!(engine.max_concurrent(), 3);
    }

    #[test]
    fn workflow_engine_custom_max_concurrent() {
        let engine = WorkflowEngine::new(10);
        assert_eq!(engine.max_concurrent(), 10);
    }

    #[test]
    fn lock_acquisition_result_acquired_methods() {
        let result = LockAcquisitionResult::Acquired;
        assert!(result.is_acquired());
        assert!(!result.is_already_locked());
    }

    #[test]
    fn lock_acquisition_result_already_locked_methods() {
        let result = LockAcquisitionResult::AlreadyLocked {
            held_by_workflow: "test_wf".to_string(),
            held_by_execution: "exec-1".to_string(),
            locked_since_ms: 1000,
        };
        assert!(!result.is_acquired());
        assert!(result.is_already_locked());
    }

    #[test]
    fn pane_lock_info_serde_roundtrip() {
        let info = PaneLockInfo {
            pane_id: 42,
            workflow_name: "test_wf".to_string(),
            execution_id: "exec-1".to_string(),
            locked_at_ms: 1234567890,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: PaneLockInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pane_id, 42);
        assert_eq!(back.workflow_name, "test_wf");
    }

    #[test]
    fn step_error_code_abort_returns_ft5002() {
        let result = StepResult::abort("some error");
        assert_eq!(
            step_error_code_from_result(&result),
            Some("FT-5002".to_string())
        );
    }

    #[test]
    fn step_error_code_continue_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::cont()), None);
    }

    #[test]
    fn step_error_code_done_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::done_empty()), None);
    }

    #[test]
    fn step_error_code_retry_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::retry(100)), None);
    }

    #[test]
    fn step_error_code_send_text_returns_none() {
        assert_eq!(
            step_error_code_from_result(&StepResult::send_text("x")),
            None
        );
    }

    #[test]
    fn redact_text_for_log_short_unchanged() {
        let result = redact_text_for_log("hello", 100);
        assert_eq!(result, "hello");
    }

    #[test]
    fn redact_text_for_log_truncates_long() {
        let long = "a".repeat(200);
        let result = redact_text_for_log(&long, 10);
        assert!(result.len() <= 13); // 10 chars + "..."
        assert!(result.ends_with("..."));
    }

    #[test]
    fn redact_text_for_log_exact_boundary() {
        let result = redact_text_for_log("12345", 5);
        assert_eq!(result, "12345"); // exactly at limit, no truncation
    }

    #[test]
    fn redacted_step_result_for_logging_preserves_continue() {
        let result = StepResult::cont();
        let redacted = redacted_step_result_for_logging(&result);
        assert!(matches!(redacted, StepResult::Continue));
    }

    #[test]
    fn redacted_step_result_for_logging_preserves_abort() {
        let result = StepResult::abort("reason");
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::Abort { reason } = redacted {
            assert_eq!(reason, "reason");
        } else {
            panic!("expected Abort");
        }
    }

    #[test]
    fn redacted_step_result_for_logging_truncates_send_text() {
        let long_text = "x".repeat(500);
        let result = StepResult::send_text(long_text);
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::SendText { text, .. } = redacted {
            assert!(text.len() <= 163); // 160 + "..."
        } else {
            panic!("expected SendText");
        }
    }

    #[test]
    fn fallback_reason_display_needs_human_auth() {
        let reason = FallbackReason::NeedsHumanAuth {
            account: "openai".to_string(),
            detail: "MFA required".to_string(),
        };
        let msg = reason.to_string();
        assert!(msg.contains("openai"));
        assert!(msg.contains("MFA required"));
    }

    #[test]
    fn fallback_reason_display_failover_disabled() {
        let reason = FallbackReason::FailoverDisabled;
        assert!(reason.to_string().contains("disabled"));
    }

    #[test]
    fn fallback_reason_display_tool_missing() {
        let reason = FallbackReason::ToolMissing {
            tool: "playwright".to_string(),
        };
        assert!(reason.to_string().contains("playwright"));
    }

    #[test]
    fn fallback_reason_display_policy_denied() {
        let reason = FallbackReason::PolicyDenied {
            rule: "alt_screen".to_string(),
        };
        assert!(reason.to_string().contains("alt_screen"));
    }

    #[test]
    fn fallback_reason_display_all_accounts_exhausted() {
        let reason = FallbackReason::AllAccountsExhausted {
            accounts_checked: 3,
        };
        assert!(reason.to_string().contains("3"));
    }

    #[test]
    fn fallback_reason_display_other() {
        let reason = FallbackReason::Other {
            detail: "custom detail".to_string(),
        };
        assert_eq!(reason.to_string(), "custom detail");
    }

    #[test]
    fn fallback_reason_serde_needs_human_auth() {
        let reason = FallbackReason::NeedsHumanAuth {
            account: "test".to_string(),
            detail: "details".to_string(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FallbackReason::NeedsHumanAuth { .. }));
    }

    #[test]
    fn fallback_reason_serde_failover_disabled() {
        let reason = FallbackReason::FailoverDisabled;
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FallbackReason::FailoverDisabled));
    }

    #[test]
    fn fallback_reason_serde_tool_missing() {
        let reason = FallbackReason::ToolMissing {
            tool: "caut".to_string(),
        };
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, FallbackReason::ToolMissing { .. }));
    }

    #[test]
    fn fallback_reason_serde_all_accounts_exhausted() {
        let reason = FallbackReason::AllAccountsExhausted {
            accounts_checked: 5,
        };
        let json = serde_json::to_string(&reason).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        if let FallbackReason::AllAccountsExhausted { accounts_checked } = back {
            assert_eq!(accounts_checked, 5);
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn fallback_next_step_plan_serde_roundtrip() {
        let plan = FallbackNextStepPlan {
            version: FallbackNextStepPlan::CURRENT_VERSION,
            reason: FallbackReason::FailoverDisabled,
            pane_id: 42,
            operator_steps: vec!["Step 1".to_string()],
            retry_after_ms: Some(1000),
            resume_session_id: Some("abc-123".to_string()),
            account_id: Some("acct-1".to_string()),
            suggested_commands: vec!["ft auth bootstrap".to_string()],
            created_at_ms: 9999,
        };
        let json = serde_json::to_string(&plan).unwrap();
        let back: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back.version, FallbackNextStepPlan::CURRENT_VERSION);
        assert_eq!(back.pane_id, 42);
        assert_eq!(back.operator_steps.len(), 1);
        assert_eq!(back.retry_after_ms, Some(1000));
        assert_eq!(back.resume_session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn fallback_next_step_plan_current_version_is_one() {
        assert_eq!(FallbackNextStepPlan::CURRENT_VERSION, 1);
    }

    #[test]
    fn fallback_next_step_plan_optional_fields_skip_none() {
        let plan = FallbackNextStepPlan {
            version: 1,
            reason: FallbackReason::FailoverDisabled,
            pane_id: 1,
            operator_steps: vec![],
            retry_after_ms: None,
            resume_session_id: None,
            account_id: None,
            suggested_commands: vec![],
            created_at_ms: 0,
        };
        let json = serde_json::to_string(&plan).unwrap();
        // skip_serializing_if = "Option::is_none" should omit these
        assert!(!json.contains("retry_after_ms"));
        assert!(!json.contains("resume_session_id"));
        assert!(!json.contains("account_id"));
        assert!(!json.contains("suggested_commands"));
    }

    #[test]
    fn now_ms_returns_plausible_timestamp() {
        let ms = now_ms();
        // Should be after 2020-01-01
        assert!(ms > 1_577_836_800_000);
    }

    #[test]
    fn generate_workflow_id_contains_workflow_name() {
        let id = generate_workflow_id("handle_usage");
        assert!(id.starts_with("handle_usage-"));
    }

    #[test]
    fn generate_workflow_id_has_three_parts() {
        let id = generate_workflow_id("test");
        assert_eq!(id.splitn(3, '-').count(), 3, "expected name-timestamp-hex");
    }

    #[test]
    fn find_token_usage_line_returns_none_when_absent() {
        assert!(find_token_usage_line("no usage here").is_none());
    }

    #[test]
    fn find_session_id_extracts_uuid_style() {
        let tail = "codex resume a1b2c3d4-e5f6-7890-abcd-1234567890ab";
        let id = find_session_id(tail).unwrap();
        assert!(id.contains("a1b2c3d4"));
    }

    #[test]
    fn find_reset_time_multiple_prefers_last() {
        let tail = "try again at 3:00 PM.\ntry again at 5:00 PM.";
        let time = find_reset_time(tail).unwrap();
        assert!(time.contains("5:00 PM"));
    }

    #[test]
    fn ctrl_c_injection_ok_allowed() {
        let result = InjectionResult::Allowed {
            decision: crate::policy::PolicyDecision::Allow {
                rule_id: Some("test".to_string()),
                context: None,
            },
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        assert!(ctrl_c_injection_ok(result).is_ok());
    }

    #[test]
    fn ctrl_c_injection_ok_denied() {
        let result = InjectionResult::Denied {
            decision: crate::policy::PolicyDecision::Deny {
                reason: "blocked".to_string(),
                rule_id: None,
                context: None,
            },
            summary: "ctrl-c".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let err = ctrl_c_injection_ok(result).unwrap_err();
        assert!(err.contains("denied"));
    }

    #[test]
    fn ctrl_c_injection_ok_error() {
        let result = InjectionResult::Error {
            error: "connection lost".to_string(),
            pane_id: 1,
            action: crate::policy::ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let err = ctrl_c_injection_ok(result).unwrap_err();
        assert!(err.contains("connection lost"));
    }

    #[test]
    fn wait_condition_stable_tail_serde() {
        let cond = WaitCondition::StableTail {
            pane_id: None,
            stable_for_ms: 2000,
        };
        let json = serde_json::to_string(&cond).unwrap();
        let back: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(back, cond);
    }

    #[test]
    fn wait_condition_stable_tail_with_pane() {
        let cond = WaitCondition::StableTail {
            pane_id: Some(10),
            stable_for_ms: 500,
        };
        assert_eq!(cond.pane_id(), Some(10));
    }

    #[test]
    fn text_match_to_wait_matcher_substring() {
        let m = TextMatch::substring("hello");
        let wm = m.to_wait_matcher().unwrap();
        assert!(format!("{:?}", wm).contains("hello"));
    }

    #[test]
    fn text_match_to_wait_matcher_regex_valid() {
        let m = TextMatch::regex(r"\d+");
        assert!(m.to_wait_matcher().is_ok());
    }

    #[test]
    fn text_match_to_wait_matcher_regex_invalid() {
        let m = TextMatch::regex(r"(unclosed");
        assert!(m.to_wait_matcher().is_err());
    }

    #[test]
    fn build_verification_refs_empty_for_continue() {
        let result = StepResult::cont();
        assert!(build_verification_refs(&result, None).is_none());
    }

    #[test]
    fn build_verification_refs_populated_for_wait_for() {
        let result = StepResult::wait_for_with_timeout(WaitCondition::external("signal"), 5000);
        let refs = build_verification_refs(&result, None).unwrap();
        assert!(refs.contains("wait_for"));
        assert!(refs.contains("5000"));
    }

    #[test]
    fn build_verification_refs_populated_for_send_text_wait() {
        let result = StepResult::send_text_and_wait("hello", WaitCondition::pane_idle(1000), 3000);
        let refs = build_verification_refs(&result, None).unwrap();
        assert!(refs.contains("post_send_wait"));
    }

    #[test]
    fn parse_codex_session_summary_full() {
        let tail = "codex resume abc12345\nToken usage: total=500 input=200 output=300\ntry again at 5:00 PM.";
        let summary = parse_codex_session_summary(tail).unwrap();
        assert_eq!(summary.session_id, "abc12345");
        assert_eq!(summary.token_usage.total, Some(500));
        assert_eq!(summary.reset_time.as_deref(), Some("5:00 PM"));
    }

    #[test]
    fn parse_codex_session_summary_missing_session_id() {
        let tail = "Token usage: total=500";
        let err = parse_codex_session_summary(tail).unwrap_err();
        assert!(err.missing.contains(&"session_id"));
    }

    #[test]
    fn parse_codex_session_summary_missing_token_usage() {
        let tail = "codex resume abc12345";
        let err = parse_codex_session_summary(tail).unwrap_err();
        assert!(err.missing.contains(&"token_usage"));
    }

    // =========================================================================
    // RubyBeaver wa-1u90p.7.1 batch 2
    // =========================================================================

    #[test]
    fn parse_codex_session_summary_missing_both() {
        let tail = "nothing useful";
        let err = parse_codex_session_summary(tail).unwrap_err();
        assert!(err.missing.contains(&"session_id"));
        assert!(err.missing.contains(&"token_usage"));
    }

    #[test]
    fn parse_codex_session_summary_no_reset_time() {
        let tail = "codex resume abc12345\nToken usage: total=500 input=200 output=300";
        let summary = parse_codex_session_summary(tail).unwrap();
        assert!(summary.reset_time.is_none());
    }

    #[test]
    fn codex_session_parse_error_tail_hash_nonzero() {
        let err = parse_codex_session_summary("some content").unwrap_err();
        // tail_hash should be deterministic and non-zero for non-empty input
        assert!(err.tail_hash != 0 || err.tail_len == 0);
        assert_eq!(err.tail_len, "some content".len());
    }

    #[test]
    fn descriptor_limits_default_max_steps() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_steps > 0);
        assert!(limits.max_steps <= 100);
    }

    #[test]
    fn descriptor_limits_default_max_wait_timeout() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_wait_timeout_ms > 0);
    }

    #[test]
    fn descriptor_limits_default_max_sleep() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_sleep_ms > 0);
    }

    #[test]
    fn descriptor_limits_default_max_text_len() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_text_len > 0);
    }

    #[test]
    fn descriptor_limits_default_max_match_len() {
        let limits = DescriptorLimits::default();
        assert!(limits.max_match_len > 0);
    }

    #[test]
    fn descriptor_failure_handler_interpolate_notify() {
        let handler = DescriptorFailureHandler::Notify {
            message: "Step ${failed_step} failed".to_string(),
        };
        let msg = handler.interpolate_message("send_ctrl_c");
        assert_eq!(msg, "Step send_ctrl_c failed");
    }

    #[test]
    fn descriptor_failure_handler_interpolate_log() {
        let handler = DescriptorFailureHandler::Log {
            message: "Error in ${failed_step}".to_string(),
        };
        let msg = handler.interpolate_message("wait_idle");
        assert_eq!(msg, "Error in wait_idle");
    }

    #[test]
    fn descriptor_failure_handler_interpolate_abort_no_placeholder() {
        let handler = DescriptorFailureHandler::Abort {
            message: "workflow failed".to_string(),
        };
        let msg = handler.interpolate_message("step1");
        assert_eq!(msg, "workflow failed");
    }

    #[test]
    fn descriptor_matcher_to_text_match_substring() {
        let dm = DescriptorMatcher::Substring {
            value: "hello".to_string(),
        };
        let tm = dm.to_text_match();
        assert!(matches!(tm, TextMatch::Substring { value } if value == "hello"));
    }

    #[test]
    fn descriptor_matcher_to_text_match_regex() {
        let dm = DescriptorMatcher::Regex {
            pattern: r"\d+".to_string(),
        };
        let tm = dm.to_text_match();
        assert!(matches!(tm, TextMatch::Regex { pattern } if pattern == r"\d+"));
    }

    #[test]
    fn descriptor_trigger_serde_roundtrip() {
        let trigger = DescriptorTrigger {
            event_types: vec!["session.end".to_string()],
            agent_types: vec!["codex".to_string()],
            rule_ids: vec!["compaction.detected".to_string()],
        };
        let json = serde_json::to_string(&trigger).unwrap();
        let back: DescriptorTrigger = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event_types, vec!["session.end"]);
        assert_eq!(back.agent_types, vec!["codex"]);
    }

    #[test]
    fn descriptor_trigger_empty_defaults() {
        let json = "{}";
        let trigger: DescriptorTrigger = serde_json::from_str(json).unwrap();
        assert!(trigger.event_types.is_empty());
        assert!(trigger.agent_types.is_empty());
        assert!(trigger.rule_ids.is_empty());
    }

    #[test]
    fn device_code_struct_fields() {
        let dc = DeviceCode {
            code: "ABCD-1234".to_string(),
            url: Some("https://example.com/device".to_string()),
        };
        assert_eq!(dc.code, "ABCD-1234");
        assert!(dc.url.unwrap().contains("device"));
    }

    #[test]
    fn device_code_parse_error_display_format() {
        let err = DeviceCodeParseError {
            expected: "XXXX-YYYY",
            tail_hash: 0xBEEF,
            tail_len: 50,
        };
        let msg = err.to_string();
        assert!(msg.contains("XXXX-YYYY"));
        assert!(msg.contains("tail_len=50"));
    }

    #[test]
    fn device_code_parse_error_is_std_error() {
        let err = DeviceCodeParseError {
            expected: "test",
            tail_hash: 0,
            tail_len: 0,
        };
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn validate_device_code_empty_string() {
        assert!(!validate_device_code(""));
    }

    #[test]
    fn validate_device_code_no_dash() {
        assert!(!validate_device_code("ABCD1234"));
    }

    #[test]
    fn validate_device_code_too_short_parts() {
        assert!(!validate_device_code("AB-CD"));
    }

    #[test]
    fn parse_device_code_from_text() {
        let tail = "Enter code: ABCD-1234";
        let dc = parse_device_code(tail).unwrap();
        assert_eq!(dc.code, "ABCD-1234");
    }

    #[test]
    fn parse_device_code_no_match() {
        let err = parse_device_code("nothing here").unwrap_err();
        assert!(err.expected.contains("device code"));
    }

    #[test]
    fn account_selection_step_error_display_caut() {
        let err = AccountSelectionStepError::Storage("db locked".to_string());
        let msg = err.to_string();
        assert!(msg.contains("db locked"));
    }

    #[test]
    fn account_selection_step_error_is_std_error() {
        let err = AccountSelectionStepError::Storage("test".to_string());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn pane_group_strategy_serde_by_domain() {
        let s = PaneGroupStrategy::ByDomain;
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_strategy_serde_by_agent() {
        let s = PaneGroupStrategy::ByAgent;
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_strategy_serde_by_project() {
        let s = PaneGroupStrategy::ByProject;
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_strategy_serde_explicit() {
        let s = PaneGroupStrategy::Explicit {
            pane_ids: vec![1, 2, 3],
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: PaneGroupStrategy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pane_group_new_and_accessors() {
        let g = PaneGroup::new("test", vec![1, 2], PaneGroupStrategy::ByDomain);
        assert_eq!(g.name, "test");
        assert_eq!(g.len(), 2);
        assert!(!g.is_empty());
    }

    #[test]
    fn pane_group_empty() {
        let g = PaneGroup::new("empty", vec![], PaneGroupStrategy::ByAgent);
        assert_eq!(g.len(), 0);
        assert!(g.is_empty());
    }

    #[test]
    fn wait_condition_result_satisfied_elapsed() {
        let r = WaitConditionResult::Satisfied {
            elapsed_ms: 100,
            polls: 5,
            context: Some("matched".to_string()),
        };
        assert!(r.is_satisfied());
        assert!(!r.is_timed_out());
        assert_eq!(r.elapsed_ms(), Some(100));
    }

    #[test]
    fn wait_condition_result_timed_out_elapsed() {
        let r = WaitConditionResult::TimedOut {
            elapsed_ms: 5000,
            polls: 50,
            last_observed: None,
        };
        assert!(!r.is_satisfied());
        assert!(r.is_timed_out());
        assert_eq!(r.elapsed_ms(), Some(5000));
    }

    #[test]
    fn wait_condition_result_unsupported_no_elapsed() {
        let r = WaitConditionResult::Unsupported {
            reason: "not impl".to_string(),
        };
        assert!(!r.is_satisfied());
        assert!(!r.is_timed_out());
        assert_eq!(r.elapsed_ms(), None);
    }

    #[test]
    fn wait_condition_options_default_has_sensible_values() {
        let opts = WaitConditionOptions::default();
        assert!(opts.tail_lines > 0);
        assert!(opts.max_polls > 0);
        assert!(opts.poll_initial > Duration::ZERO);
        assert!(opts.poll_max >= opts.poll_initial);
    }

    #[test]
    fn auth_recovery_strategy_label_device_code() {
        let s = AuthRecoveryStrategy::DeviceCode {
            code: None,
            url: None,
        };
        assert_eq!(s.label(), "device_code");
    }

    #[test]
    fn auth_recovery_strategy_label_api_key_error() {
        let s = AuthRecoveryStrategy::ApiKeyError { key_hint: None };
        assert_eq!(s.label(), "api_key_error");
    }

    #[test]
    fn auth_recovery_strategy_label_manual() {
        let s = AuthRecoveryStrategy::ManualIntervention {
            agent_type: "codex".to_string(),
            hint: "login".to_string(),
        };
        assert_eq!(s.label(), "manual_intervention");
    }

    #[test]
    fn auth_recovery_strategy_from_detection_device_code() {
        let trigger = serde_json::json!({
            "event_type": "auth.device_code",
            "extracted": { "code": "ABCD-1234", "url": "https://example.com" }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "device_code");
    }

    #[test]
    fn auth_recovery_strategy_from_detection_api_key() {
        let trigger = serde_json::json!({
            "event_type": "auth.error",
            "extracted": { "key_name": "OPENAI_API_KEY" }
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "api_key_error");
    }

    #[test]
    fn auth_recovery_strategy_from_detection_manual_fallback() {
        let trigger = serde_json::json!({
            "event_type": "auth.unknown",
            "agent_type": "claude_code"
        });
        let strategy = AuthRecoveryStrategy::from_detection(&trigger);
        assert_eq!(strategy.label(), "manual_intervention");
    }

    #[test]
    fn build_needs_human_auth_plan_includes_account() {
        let plan = build_needs_human_auth_plan(
            42,
            "openai-team",
            "MFA required",
            Some("sess-123"),
            Some(9999),
            1000,
        );
        assert_eq!(plan.version, FallbackNextStepPlan::CURRENT_VERSION);
        assert_eq!(plan.pane_id, 42);
        assert!(
            plan.operator_steps
                .iter()
                .any(|s| s.contains("openai-team"))
        );
        assert_eq!(plan.retry_after_ms, Some(9999));
        assert_eq!(plan.resume_session_id.as_deref(), Some("sess-123"));
    }

    #[test]
    fn build_needs_human_auth_plan_no_resume() {
        let plan = build_needs_human_auth_plan(1, "acct", "detail", None, None, 0);
        assert!(plan.resume_session_id.is_none());
        assert!(plan.retry_after_ms.is_none());
    }

    #[test]
    fn validate_session_id_accepts_long_hex() {
        assert!(validate_session_id("a1b2c3d4e5f60000"));
    }

    #[test]
    fn validate_session_id_rejects_special_chars() {
        assert!(!validate_session_id("abc!@#$"));
    }

    #[test]
    fn format_resume_command_with_session_id() {
        let config = ResumeSessionConfig::default();
        let cmd = format_resume_command("test-session-123", &config);
        assert!(cmd.contains("test-session-123"));
    }

    #[test]
    fn elapsed_ms_is_zero_for_recent_instant() {
        let start = Instant::now();
        let ms = elapsed_ms(start);
        assert!(ms < 1000); // Should be near-zero
    }

    #[test]
    fn workflow_step_new() {
        let step = WorkflowStep::new("step1", "First step");
        assert_eq!(step.name, "step1");
        assert_eq!(step.description, "First step");
    }

    #[test]
    fn wait_condition_pane_id_pattern() {
        let cond = WaitCondition::pattern("rule");
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_pattern_on_pane() {
        let cond = WaitCondition::pattern_on_pane(42, "rule");
        assert_eq!(cond.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_id_pane_idle() {
        let cond = WaitCondition::pane_idle(1000);
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_pane_idle_on() {
        let cond = WaitCondition::pane_idle_on(10, 1000);
        assert_eq!(cond.pane_id(), Some(10));
    }

    #[test]
    fn wait_condition_pane_id_stable_tail() {
        let cond = WaitCondition::stable_tail(2000);
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_stable_tail_on() {
        let cond = WaitCondition::stable_tail_on(5, 2000);
        assert_eq!(cond.pane_id(), Some(5));
    }

    #[test]
    fn wait_condition_pane_id_text_match() {
        let cond = WaitCondition::text_match(TextMatch::substring("x"));
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_text_match_on_pane() {
        let cond = WaitCondition::text_match_on_pane(99, TextMatch::substring("x"));
        assert_eq!(cond.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_pane_id_sleep() {
        let cond = WaitCondition::sleep(100);
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn wait_condition_pane_id_external() {
        let cond = WaitCondition::external("signal");
        assert_eq!(cond.pane_id(), None);
    }

    #[test]
    fn descriptor_loop_execution() {
        let descriptor = WorkflowDescriptor {
            workflow_schema_version: 1,
            name: "test_loop".to_string(),
            description: None,
            triggers: vec![],
            steps: vec![DescriptorStep::Loop {
                id: "loop".to_string(),
                description: None,
                count: 3,
                body: vec![DescriptorStep::Log {
                    id: "log".to_string(),
                    description: None,
                    message: "iteration".to_string(),
                }],
            }],
            on_failure: None,
        };
        let _workflow = DescriptorWorkflow::new(descriptor);
    }
}
