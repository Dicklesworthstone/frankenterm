//! Workflow execution state and engine.
//!
//! Provides WorkflowExecution, ExecutionStatus, WorkflowEngine, and
//! WorkflowExecutionResult for managing workflow lifecycle state.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

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
pub(super) fn compute_next_step(step_logs: &[crate::storage::WorkflowStepLogRecord]) -> usize {
    if step_logs.is_empty() {
        return 0;
    }

    // Find the log with the highest ID (most recent chronologically)
    let last_log = step_logs.iter().max_by_key(|log| log.id);

    match last_log {
        Some(log) => match log.result_type.as_str() {
            "continue" | "done" => log.step_index + 1,
            "jump_to" => {
                if let Some(data) = &log.result_data {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(step) = json.get("step").and_then(|v| v.as_u64()) {
                            return step as usize;
                        }
                    }
                }
                log.step_index // Fallback if data is missing or malformed
            }
            // For retry, wait_for, send_text, abort, we re-execute or stay on the same step
            _ => log.step_index,
        },
        None => 0,
    }
}

/// Generate a unique workflow execution ID
pub(super) fn generate_workflow_id(workflow_name: &str) -> String {
    let timestamp = now_ms();
    let random: u32 = rand::random();
    format!("{workflow_name}-{timestamp}-{random:08x}")
}

/// Get current timestamp in milliseconds
pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub(super) fn build_verification_refs(
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

pub fn redact_text_for_log(text: &str, max_len: usize) -> String {
    let redactor = Redactor::new();
    let redacted = redactor.redact(text);
    if redacted.len() <= max_len {
        return redacted;
    }
    let mut truncated = redacted.chars().take(max_len).collect::<String>();
    truncated.push_str("...");
    truncated
}

pub(super) fn redacted_step_result_for_logging(step_result: &StepResult) -> StepResult {
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

pub(super) fn step_error_code_from_result(step_result: &StepResult) -> Option<String> {
    match step_result {
        StepResult::Abort { .. } => Some("FT-5002".to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepPolicyDecision {
    Allow,
    Deny,
    RequireApproval,
    Error,
}

impl WorkflowStepPolicyDecision {
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowStepPolicySummary {
    pub decision: WorkflowStepPolicyDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<crate::policy::ActionKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_context: Option<crate::policy::DecisionContext>,
}

impl WorkflowStepPolicySummary {
    #[must_use]
    pub fn from_injection(result: &crate::policy::InjectionResult) -> Self {
        use crate::policy::InjectionResult;

        match result {
            InjectionResult::Allowed {
                decision,
                summary,
                action,
                ..
            } => Self {
                decision: WorkflowStepPolicyDecision::Allow,
                action: Some(*action),
                rule_id: decision.rule_id().map(str::to_string),
                reason: None,
                summary: Some(summary.clone()),
                error: None,
                decision_context: decision.context().cloned(),
            },
            InjectionResult::Denied {
                decision,
                summary,
                action,
                ..
            } => Self {
                decision: WorkflowStepPolicyDecision::Deny,
                action: Some(*action),
                rule_id: decision.rule_id().map(str::to_string),
                reason: decision.reason().map(str::to_string),
                summary: Some(summary.clone()),
                error: None,
                decision_context: decision.context().cloned(),
            },
            InjectionResult::RequiresApproval {
                decision,
                summary,
                action,
                ..
            } => Self {
                decision: WorkflowStepPolicyDecision::RequireApproval,
                action: Some(*action),
                rule_id: decision.rule_id().map(str::to_string),
                reason: decision.reason().map(str::to_string),
                summary: Some(summary.clone()),
                error: None,
                decision_context: decision.context().cloned(),
            },
            InjectionResult::Error {
                decision,
                error,
                action,
                ..
            } => Self {
                decision: WorkflowStepPolicyDecision::Error,
                action: Some(*action),
                rule_id: decision.rule_id().map(str::to_string),
                reason: decision.reason().map(str::to_string),
                summary: None,
                error: Some(error.clone()),
                decision_context: decision.context().cloned(),
            },
        }
    }

    #[must_use]
    pub fn parse(serialized: &str) -> Option<Self> {
        serde_json::from_str(serialized).ok()
    }

    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        self.decision.is_allowed()
    }
}

#[must_use]
pub fn policy_summary_decision_is_allow(summary: &str) -> Option<bool> {
    WorkflowStepPolicySummary::parse(summary)
        .map(|parsed| parsed.is_allowed())
        .or_else(|| {
            serde_json::from_str::<serde_json::Value>(summary)
                .ok()
                .and_then(|data| {
                    data.get("decision")
                        .and_then(|value| value.as_str())
                        .map(|decision| decision == "allow")
                })
        })
}

pub(super) fn policy_summary_from_injection(
    result: &crate::policy::InjectionResult,
) -> Option<String> {
    serde_json::to_string(&WorkflowStepPolicySummary::from_injection(result))
        .inspect_err(|e| tracing::warn!(error = %e, "workflow policy summary serialization failed"))
        .ok()
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

pub(super) fn policy_error_code_from_injection(
    result: &crate::policy::InjectionResult,
) -> Option<String> {
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
    workflow_name: &str,
    input_summary: Option<String>,
    result: &str,
    decision_reason: Option<String>,
) -> Option<i64> {
    let timestamp_ms = now_ms();
    let decision_context = build_workflow_audit_decision_context(
        action_kind,
        execution_id,
        pane_id,
        workflow_name,
        input_summary.as_deref(),
        result,
        decision_reason.as_deref(),
        timestamp_ms,
    );
    let action = crate::storage::AuditActionRecord {
        id: 0,
        ts: timestamp_ms,
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
        decision_context,
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

fn build_workflow_audit_decision_context(
    action_kind: &str,
    execution_id: &str,
    pane_id: u64,
    workflow_name: &str,
    input_summary: Option<&str>,
    result: &str,
    decision_reason: Option<&str>,
    timestamp_ms: i64,
) -> Option<String> {
    let mut context = crate::policy::DecisionContext::new_audit(
        timestamp_ms,
        crate::policy::ActionKind::WorkflowRun,
        crate::policy::ActorKind::Workflow,
        crate::policy::PolicySurface::Workflow,
        Some(pane_id),
        None,
        input_summary.map(str::to_string),
        Some(execution_id.to_string()),
    );
    let determining_rule = format!("audit.{action_kind}");
    context.record_rule(
        &determining_rule,
        true,
        Some("allow"),
        Some("workflow audit action recorded".to_string()),
    );
    context.set_determining_rule(&determining_rule);
    context.add_evidence("stage", "workflow_audit");
    context.add_evidence("workflow_action_kind", action_kind);
    context.add_evidence(
        "workflow_surface",
        crate::policy::PolicySurface::Workflow.as_str(),
    );
    context.add_evidence("workflow_name", workflow_name);
    context.add_evidence("execution_id", execution_id);
    context.add_evidence("workflow_result", result);
    if let Some(decision_reason) = decision_reason {
        context.add_evidence("decision_reason", decision_reason);
    }
    serde_json::to_string(&context).ok()
}

pub(super) async fn record_workflow_start_action(
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

pub(super) async fn fetch_workflow_start_action_id(
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

pub(super) async fn record_workflow_step_action(
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

pub(super) async fn record_workflow_terminal_action(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_compat::{CompatRuntime, RuntimeBuilder as CompatRuntimeBuilder};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    fn temp_db_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("workflow-engine-audit.db");
        (dir, db_path)
    }

    fn latest_audit_action(db_path: &Path, action_kind: &str) -> crate::storage::AuditActionRecord {
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .unwrap();
            let rows = storage
                .get_audit_actions(crate::storage::AuditQuery {
                    limit: Some(1),
                    action_kind: Some(action_kind.to_string()),
                    ..crate::storage::AuditQuery::default()
                })
                .await
                .unwrap();
            rows.into_iter()
                .next()
                .unwrap_or_else(|| panic!("missing audit row for {action_kind}"))
        })
    }

    // ========================================================================
    // ExecutionStatus serde + equality
    // ========================================================================

    #[test]
    fn execution_status_serde_roundtrip() {
        let statuses = [
            ExecutionStatus::Running,
            ExecutionStatus::Waiting,
            ExecutionStatus::Completed,
            ExecutionStatus::Aborted,
        ];
        for status in &statuses {
            let json = serde_json::to_string(status).unwrap();
            let parsed: ExecutionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(*status, parsed);
        }
    }

    #[test]
    fn execution_status_rename_all_snake_case() {
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Waiting).unwrap(),
            "\"waiting\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Completed).unwrap(),
            "\"completed\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Aborted).unwrap(),
            "\"aborted\""
        );
    }

    #[test]
    fn execution_status_is_copy() {
        let s = ExecutionStatus::Running;
        let s2 = s; // copy
        assert_eq!(s, s2);
    }

    // ========================================================================
    // WorkflowExecution serde
    // ========================================================================

    #[test]
    fn workflow_execution_serde_roundtrip() {
        let exec = WorkflowExecution {
            id: "wf-test-123".to_string(),
            workflow_name: "handle_compaction".to_string(),
            pane_id: 42,
            current_step: 2,
            status: ExecutionStatus::Running,
            started_at: 1700000000000,
            updated_at: 1700000001000,
        };
        let json = serde_json::to_string(&exec).unwrap();
        let parsed: WorkflowExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "wf-test-123");
        assert_eq!(parsed.workflow_name, "handle_compaction");
        assert_eq!(parsed.pane_id, 42);
        assert_eq!(parsed.current_step, 2);
        assert_eq!(parsed.status, ExecutionStatus::Running);
        assert_eq!(parsed.started_at, 1700000000000);
        assert_eq!(parsed.updated_at, 1700000001000);
    }

    #[test]
    fn workflow_execution_clone() {
        let exec = WorkflowExecution {
            id: "wf-1".to_string(),
            workflow_name: "test".to_string(),
            pane_id: 1,
            current_step: 0,
            status: ExecutionStatus::Waiting,
            started_at: 100,
            updated_at: 200,
        };
        let cloned = exec.clone();
        assert_eq!(cloned.id, exec.id);
        assert_eq!(cloned.status, exec.status);
    }

    // ========================================================================
    // WorkflowEngine construction
    // ========================================================================

    #[test]
    fn workflow_engine_default_concurrency() {
        let engine = WorkflowEngine::default();
        assert_eq!(engine.max_concurrent(), 3);
    }

    #[test]
    fn workflow_engine_custom_concurrency() {
        let engine = WorkflowEngine::new(10);
        assert_eq!(engine.max_concurrent(), 10);
    }

    #[test]
    fn workflow_engine_zero_concurrency() {
        let engine = WorkflowEngine::new(0);
        assert_eq!(engine.max_concurrent(), 0);
    }

    #[test]
    fn workflow_audit_decision_context_tracks_workflow_surface_and_metadata() {
        let input_summary = Some("{\"workflow_name\":\"handle_compaction\",\"step_index\":1}");
        let ctx_json = build_workflow_audit_decision_context(
            "workflow_step",
            "wf-123",
            7,
            "handle_compaction",
            input_summary,
            "continue",
            Some("waiting for verification"),
            1_234,
        )
        .expect("decision context should serialize");
        let ctx: crate::policy::DecisionContext =
            serde_json::from_str(&ctx_json).expect("decision context should parse");
        let evidence = |key: &str| {
            ctx.evidence
                .iter()
                .find(|entry| entry.key == key)
                .map(|entry| entry.value.as_str())
        };

        assert_eq!(ctx.actor, crate::policy::ActorKind::Workflow);
        assert_eq!(ctx.surface, crate::policy::PolicySurface::Workflow);
        assert_eq!(ctx.action, crate::policy::ActionKind::WorkflowRun);
        assert_eq!(ctx.pane_id, Some(7));
        assert_eq!(ctx.workflow_id.as_deref(), Some("wf-123"));
        assert_eq!(ctx.text_summary, input_summary.map(str::to_string));
        assert_eq!(ctx.determining_rule.as_deref(), Some("audit.workflow_step"));
        assert_eq!(evidence("stage"), Some("workflow_audit"));
        assert_eq!(evidence("workflow_action_kind"), Some("workflow_step"));
        assert_eq!(evidence("workflow_surface"), Some("workflow"));
        assert_eq!(evidence("workflow_name"), Some("handle_compaction"));
        assert_eq!(evidence("execution_id"), Some("wf-123"));
        assert_eq!(evidence("workflow_result"), Some("continue"));
        assert_eq!(
            evidence("decision_reason"),
            Some("waiting for verification")
        );
    }

    #[test]
    fn record_workflow_step_action_persists_structured_decision_context() {
        let (_dir, db_path) = temp_db_path();
        let runtime = CompatRuntimeBuilder::current_thread().build().unwrap();
        runtime.block_on(async {
            let storage = crate::storage::StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage should initialize");
            record_workflow_step_action(
                &storage,
                "handle_compaction",
                "wf-456",
                9,
                2,
                "verify_output",
                Some("step-verify".to_string()),
                Some("verification".to_string()),
                "continue",
                Some(41),
            )
            .await;
        });

        let audit = latest_audit_action(&db_path, "workflow_step");
        assert_eq!(audit.actor_kind, "workflow");
        assert_eq!(audit.actor_id.as_deref(), Some("wf-456"));
        assert_eq!(audit.pane_id, Some(9));
        assert_eq!(audit.result, "continue");

        let ctx: crate::policy::DecisionContext = serde_json::from_str(
            audit
                .decision_context
                .as_deref()
                .expect("decision context should be recorded"),
        )
        .expect("decision context should parse");
        assert_eq!(ctx.actor, crate::policy::ActorKind::Workflow);
        assert_eq!(ctx.surface, crate::policy::PolicySurface::Workflow);
        assert_eq!(ctx.action, crate::policy::ActionKind::WorkflowRun);
        assert_eq!(ctx.workflow_id.as_deref(), Some("wf-456"));
        assert_eq!(ctx.determining_rule.as_deref(), Some("audit.workflow_step"));
        assert!(
            ctx.evidence
                .iter()
                .any(|entry| entry.key == "workflow_name" && entry.value == "handle_compaction")
        );
        let summary = ctx
            .text_summary
            .as_deref()
            .expect("workflow summary should be recorded");
        let summary_json: serde_json::Value =
            serde_json::from_str(summary).expect("workflow summary should be json");
        assert_eq!(summary_json["workflow_name"], "handle_compaction");
        assert_eq!(summary_json["execution_id"], "wf-456");
        assert_eq!(summary_json["step_index"], 2);
        assert_eq!(summary_json["step_name"], "verify_output");
        assert_eq!(summary_json["step_id"], "step-verify");
        assert_eq!(summary_json["step_kind"], "verification");
        assert_eq!(summary_json["result_type"], "continue");
        assert_eq!(summary_json["parent_action_id"], 41);
    }

    #[test]
    fn policy_summary_from_injection_serializes_typed_decision_context() {
        let mut context = crate::policy::DecisionContext::empty();
        context.actor = crate::policy::ActorKind::Workflow;
        context.surface = crate::policy::PolicySurface::Mux;
        context.action = crate::policy::ActionKind::SendText;
        context.workflow_id = Some("wf-typed".to_string());
        context.pane_id = Some(11);
        context.set_determining_rule("policy.test.workflow_send");
        context.add_evidence("stage", "workflow_send");

        let result = crate::policy::InjectionResult::Denied {
            decision: crate::policy::PolicyDecision::deny_with_rule(
                "prompt not active",
                "policy.prompt_required",
            )
            .with_context(context.clone()),
            summary: "echo secret".to_string(),
            pane_id: 11,
            action: crate::policy::ActionKind::SendText,
            audit_action_id: Some(5),
        };

        let serialized =
            policy_summary_from_injection(&result).expect("policy summary should serialize");
        let summary = WorkflowStepPolicySummary::parse(&serialized)
            .expect("workflow step policy summary should parse");

        assert_eq!(summary.decision, WorkflowStepPolicyDecision::Deny);
        assert_eq!(summary.action, Some(crate::policy::ActionKind::SendText));
        assert_eq!(summary.rule_id.as_deref(), Some("policy.prompt_required"));
        assert_eq!(summary.reason.as_deref(), Some("prompt not active"));
        assert_eq!(summary.summary.as_deref(), Some("echo secret"));
        assert_eq!(summary.error, None);

        let parsed_context = summary
            .decision_context
            .expect("typed decision context should be preserved");
        assert_eq!(parsed_context.actor, crate::policy::ActorKind::Workflow);
        assert_eq!(parsed_context.surface, crate::policy::PolicySurface::Mux);
        assert_eq!(parsed_context.action, crate::policy::ActionKind::SendText);
        assert_eq!(parsed_context.workflow_id.as_deref(), Some("wf-typed"));
        assert_eq!(parsed_context.pane_id, Some(11));
        assert_eq!(
            parsed_context.determining_rule.as_deref(),
            Some("policy.test.workflow_send")
        );
        assert!(
            parsed_context
                .evidence
                .iter()
                .any(|entry| entry.key == "stage" && entry.value == "workflow_send")
        );
        assert_eq!(policy_summary_decision_is_allow(&serialized), Some(false));
        assert_eq!(
            policy_summary_decision_is_allow(
                r#"{"decision":"allow","action":"send_text","summary":"legacy"}"#
            ),
            Some(true)
        );
    }

    // ========================================================================
    // compute_next_step
    // ========================================================================

    fn make_step_log(
        step_index: usize,
        result_type: &str,
    ) -> crate::storage::WorkflowStepLogRecord {
        static NEXT_ID: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
        crate::storage::WorkflowStepLogRecord {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            workflow_id: "wf-1".to_string(),
            audit_action_id: None,
            step_index,
            step_name: format!("step_{step_index}"),
            step_id: None,
            step_kind: None,
            result_type: result_type.to_string(),
            result_data: None,
            policy_summary: None,
            verification_refs: None,
            error_code: None,
            started_at: 1000,
            completed_at: 2000,
            duration_ms: 1000,
        }
    }

    #[test]
    fn compute_next_step_empty_logs() {
        assert_eq!(compute_next_step(&[]), 0);
    }

    #[test]
    fn compute_next_step_single_continue() {
        let logs = vec![make_step_log(0, "continue")];
        assert_eq!(compute_next_step(&logs), 1);
    }

    #[test]
    fn compute_next_step_multiple_continues() {
        let logs = vec![
            make_step_log(0, "continue"),
            make_step_log(1, "continue"),
            make_step_log(2, "continue"),
        ];
        assert_eq!(compute_next_step(&logs), 3);
    }

    #[test]
    fn compute_next_step_done_is_terminal() {
        let logs = vec![make_step_log(0, "continue"), make_step_log(1, "done")];
        assert_eq!(compute_next_step(&logs), 2);
    }

    #[test]
    fn compute_next_step_retry_re_executes() {
        let logs = vec![make_step_log(0, "continue"), make_step_log(1, "retry")];
        assert_eq!(compute_next_step(&logs), 1);
    }

    #[test]
    fn compute_next_step_wait_for_re_executes() {
        let logs = vec![make_step_log(0, "continue"), make_step_log(1, "wait_for")];
        assert_eq!(compute_next_step(&logs), 1);
    }

    #[test]
    fn compute_next_step_only_non_terminal() {
        let logs = vec![make_step_log(0, "retry"), make_step_log(0, "retry")];
        assert_eq!(compute_next_step(&logs), 0);
    }

    #[test]
    fn compute_next_step_out_of_order_logs() {
        let logs = vec![
            make_step_log(2, "continue"),
            make_step_log(0, "continue"),
            make_step_log(1, "continue"),
        ];
        assert_eq!(compute_next_step(&logs), 3);
    }

    #[test]
    fn compute_next_step_mixed_terminal_and_nonterminal() {
        let logs = vec![
            make_step_log(0, "continue"),
            make_step_log(1, "continue"),
            make_step_log(2, "retry"),
        ];
        // Highest completed is step_index 1, but most recent is retry at 2, so next is 2
        assert_eq!(compute_next_step(&logs), 2);
    }

    #[test]
    fn compute_next_step_jump_to() {
        let mut jump_log = make_step_log(2, "jump_to");
        jump_log.result_data = Some(
            serde_json::json!({
                "type": "jump_to",
                "step": 5
            })
            .to_string(),
        );

        let logs = vec![
            make_step_log(0, "continue"),
            make_step_log(1, "continue"),
            jump_log,
        ];
        // After jumping to 5, the next step should be 5
        assert_eq!(compute_next_step(&logs), 5);
    }

    // ========================================================================
    // generate_workflow_id
    // ========================================================================

    #[test]
    fn generate_workflow_id_contains_name() {
        let id = generate_workflow_id("handle_compaction");
        assert!(id.starts_with("handle_compaction-"));
    }

    #[test]
    fn generate_workflow_id_unique() {
        let id1 = generate_workflow_id("wf");
        let id2 = generate_workflow_id("wf");
        assert_ne!(id1, id2);
    }

    #[test]
    fn generate_workflow_id_format() {
        let id = generate_workflow_id("test");
        let parts: Vec<&str> = id.splitn(3, '-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "test");
        assert!(parts[1].parse::<i64>().is_ok());
        assert_eq!(parts[2].len(), 8);
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ========================================================================
    // now_ms
    // ========================================================================

    #[test]
    fn now_ms_returns_reasonable_value() {
        let ms = now_ms();
        assert!(ms > 1_577_836_800_000); // after 2020-01-01
        assert!(ms > 0);
    }

    // ========================================================================
    // redact_text_for_log
    // ========================================================================

    #[test]
    fn redact_text_for_log_short_text() {
        let result = redact_text_for_log("hello world", 100);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn redact_text_for_log_truncates() {
        let result = redact_text_for_log("abcdefghijklmnop", 5);
        assert_eq!(result, "abcde...");
    }

    #[test]
    fn redact_text_for_log_exact_boundary() {
        let result = redact_text_for_log("12345", 5);
        assert_eq!(result, "12345");
    }

    #[test]
    fn redact_text_for_log_empty() {
        let result = redact_text_for_log("", 100);
        assert_eq!(result, "");
    }

    // ========================================================================
    // step_error_code_from_result
    // ========================================================================

    #[test]
    fn step_error_code_abort_returns_ft5002() {
        let result = StepResult::abort("failure");
        assert_eq!(
            step_error_code_from_result(&result),
            Some("FT-5002".to_string())
        );
    }

    #[test]
    fn step_error_code_continue_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::Continue), None);
    }

    #[test]
    fn step_error_code_done_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::done_empty()), None);
    }

    #[test]
    fn step_error_code_retry_returns_none() {
        assert_eq!(step_error_code_from_result(&StepResult::retry(1000)), None);
    }

    #[test]
    fn step_error_code_send_text_returns_none() {
        assert_eq!(
            step_error_code_from_result(&StepResult::send_text("hello".to_string())),
            None
        );
    }

    // ========================================================================
    // redacted_step_result_for_logging
    // ========================================================================

    #[test]
    fn redacted_step_result_passes_through_non_send_text() {
        let result = StepResult::Continue;
        let redacted = redacted_step_result_for_logging(&result);
        assert!(redacted.is_continue());
    }

    #[test]
    fn redacted_step_result_truncates_send_text() {
        let long_text = "x".repeat(500);
        let result = StepResult::send_text(long_text);
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::SendText { text, .. } = &redacted {
            assert!(text.len() <= 163); // 160 + "..."
        } else {
            panic!("Expected SendText");
        }
    }

    #[test]
    fn redacted_step_result_preserves_short_send_text() {
        let result = StepResult::send_text("short".to_string());
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::SendText { text, .. } = &redacted {
            assert_eq!(text, "short");
        } else {
            panic!("Expected SendText");
        }
    }

    #[test]
    fn redacted_step_result_preserves_wait_fields() {
        let result = StepResult::SendText {
            text: "cmd".to_string(),
            wait_for: Some(WaitCondition::sleep(1000)),
            wait_timeout_ms: Some(5000),
        };
        let redacted = redacted_step_result_for_logging(&result);
        if let StepResult::SendText {
            text,
            wait_for,
            wait_timeout_ms,
        } = &redacted
        {
            assert_eq!(text, "cmd");
            assert!(wait_for.is_some());
            assert_eq!(*wait_timeout_ms, Some(5000));
        } else {
            panic!("Expected SendText");
        }
    }

    // ========================================================================
    // build_verification_refs
    // ========================================================================

    #[test]
    fn build_verification_refs_none_for_continue() {
        assert!(build_verification_refs(&StepResult::Continue, None).is_none());
    }

    #[test]
    fn build_verification_refs_none_for_done() {
        assert!(build_verification_refs(&StepResult::done_empty(), None).is_none());
    }

    #[test]
    fn build_verification_refs_includes_wait_for() {
        let result = StepResult::wait_for_with_timeout(WaitCondition::pattern("test.rule"), 5000);
        let refs = build_verification_refs(&result, None);
        assert!(refs.is_some());
        let json: serde_json::Value = serde_json::from_str(&refs.unwrap()).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["source"], "wait_for");
    }

    #[test]
    fn build_verification_refs_includes_post_send_wait() {
        let result = StepResult::send_text_and_wait(
            "cmd".to_string(),
            WaitCondition::pane_idle(2000),
            10_000,
        );
        let refs = build_verification_refs(&result, None);
        assert!(refs.is_some());
        let json: serde_json::Value = serde_json::from_str(&refs.unwrap()).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["source"], "post_send_wait");
    }

    #[test]
    fn build_verification_refs_send_text_without_wait_is_none() {
        let result = StepResult::send_text("hello".to_string());
        assert!(build_verification_refs(&result, None).is_none());
    }

    #[test]
    fn build_verification_refs_with_step_plan() {
        let step_plan = crate::plan::StepPlan::new(
            1,
            crate::plan::StepAction::Custom {
                action_type: "test".to_string(),
                payload: serde_json::json!({}),
            },
            "test step",
        )
        .with_verification(crate::plan::Verification {
            strategy: crate::plan::VerificationStrategy::PaneIdle {
                pane_id: None,
                idle_threshold_ms: 2000,
            },
            description: Some("verify idle".to_string()),
            timeout_ms: Some(5000),
        });
        let refs = build_verification_refs(&StepResult::Continue, Some(&step_plan));
        assert!(refs.is_some());
        let json: serde_json::Value = serde_json::from_str(&refs.unwrap()).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["source"], "plan");
    }
}
