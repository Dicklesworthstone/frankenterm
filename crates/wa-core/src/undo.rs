//! Undo execution engine for recorded actions.
//!
//! This module executes supported undo strategies from `action_undo` metadata
//! and returns deterministic outcomes (`success`, `not_applicable`, `failed`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::WeztermError;
use crate::policy::{PolicyEngine, PolicyGatedInjector};
use crate::storage::{ActionHistoryQuery, ActionUndoRecord, StorageHandle};
use crate::wezterm::WeztermHandle;
use crate::workflows::{
    PaneWorkflowLockManager, WorkflowEngine, WorkflowRunner, WorkflowRunnerConfig,
};
use crate::{Error, Result};

/// Outcome classification for undo execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoOutcome {
    /// Undo action was applied successfully.
    Success,
    /// Undo could not be applied because target state no longer qualifies.
    NotApplicable,
    /// Undo was applicable but execution failed.
    Failed,
}

/// Request for executing undo on a recorded action.
#[derive(Debug, Clone)]
pub struct UndoRequest {
    /// Audit action ID to undo.
    pub action_id: i64,
    /// Actor label to store in `action_undo.undone_by` on success.
    pub actor: String,
    /// Optional reason attached to strategy executors (where supported).
    pub reason: Option<String>,
}

impl UndoRequest {
    /// Build a request with a default actor label.
    #[must_use]
    pub fn new(action_id: i64) -> Self {
        Self {
            action_id,
            actor: "user".to_string(),
            reason: None,
        }
    }

    /// Override actor label.
    #[must_use]
    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = actor.into();
        self
    }

    /// Attach an optional undo reason.
    #[must_use]
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

/// Result payload for undo execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoExecutionResult {
    /// Audit action ID that was targeted.
    pub action_id: i64,
    /// Strategy read from undo metadata.
    pub strategy: String,
    /// Final outcome.
    pub outcome: UndoOutcome,
    /// Human-readable summary.
    pub message: String,
    /// Optional remediation/manual guidance.
    pub guidance: Option<String>,
    /// Target workflow execution when strategy is `workflow_abort`.
    pub target_workflow_id: Option<String>,
    /// Target pane when strategy is `pane_close`.
    pub target_pane_id: Option<u64>,
    /// Populated for successful undo writes.
    pub undone_at: Option<i64>,
}

impl UndoExecutionResult {
    fn success(
        action_id: i64,
        strategy: String,
        message: String,
        target_workflow_id: Option<String>,
        target_pane_id: Option<u64>,
        undone_at: Option<i64>,
    ) -> Self {
        Self {
            action_id,
            strategy,
            outcome: UndoOutcome::Success,
            message,
            guidance: None,
            target_workflow_id,
            target_pane_id,
            undone_at,
        }
    }

    fn not_applicable(
        action_id: i64,
        strategy: String,
        message: String,
        guidance: Option<String>,
        target_workflow_id: Option<String>,
        target_pane_id: Option<u64>,
    ) -> Self {
        Self {
            action_id,
            strategy,
            outcome: UndoOutcome::NotApplicable,
            message,
            guidance,
            target_workflow_id,
            target_pane_id,
            undone_at: None,
        }
    }

    fn failed(
        action_id: i64,
        strategy: String,
        message: String,
        guidance: Option<String>,
        target_workflow_id: Option<String>,
        target_pane_id: Option<u64>,
    ) -> Self {
        Self {
            action_id,
            strategy,
            outcome: UndoOutcome::Failed,
            message,
            guidance,
            target_workflow_id,
            target_pane_id,
            undone_at: None,
        }
    }
}

/// Executes undo strategies against durable storage and WezTerm state.
#[derive(Clone)]
pub struct UndoExecutor {
    storage: Arc<StorageHandle>,
    wezterm: WeztermHandle,
}

impl UndoExecutor {
    /// Create a new undo executor.
    #[must_use]
    pub fn new(storage: Arc<StorageHandle>, wezterm: WeztermHandle) -> Self {
        Self { storage, wezterm }
    }

    /// Execute undo for a single recorded audit action.
    pub async fn execute(&self, request: UndoRequest) -> Result<UndoExecutionResult> {
        let mut history = self
            .storage
            .get_action_history(ActionHistoryQuery {
                audit_action_id: Some(request.action_id),
                limit: Some(1),
                ..Default::default()
            })
            .await?;

        let Some(action) = history.pop() else {
            return Ok(UndoExecutionResult::not_applicable(
                request.action_id,
                "none".to_string(),
                format!("Action {} not found", request.action_id),
                Some("Use `wa history` to list valid action IDs.".to_string()),
                None,
                None,
            ));
        };

        let Some(undo) = self.storage.get_action_undo(request.action_id).await? else {
            return Ok(UndoExecutionResult::not_applicable(
                request.action_id,
                "none".to_string(),
                "No undo metadata recorded for this action".to_string(),
                Some(
                    "This action predates undo metadata, or was recorded as non-undoable."
                        .to_string(),
                ),
                action.actor_id.clone(),
                action.pane_id,
            ));
        };

        if !undo.undoable {
            return Ok(UndoExecutionResult::not_applicable(
                request.action_id,
                undo.undo_strategy,
                "Action is not currently undoable".to_string(),
                undo.undo_hint.or(action.undo_hint),
                action.actor_id,
                action.pane_id,
            ));
        }

        if undo.undone_at.is_some() {
            return Ok(UndoExecutionResult::not_applicable(
                request.action_id,
                undo.undo_strategy,
                "Action has already been undone".to_string(),
                undo.undo_hint.or(action.undo_hint),
                action.actor_id,
                action.pane_id,
            ));
        }

        match undo.undo_strategy.as_str() {
            "workflow_abort" => self.execute_workflow_abort(request, &action, &undo).await,
            "pane_close" => self.execute_pane_close(request, &action, &undo).await,
            "manual" | "none" | "custom" => Ok(UndoExecutionResult::not_applicable(
                action.id,
                undo.undo_strategy,
                "Automatic undo is not supported for this strategy".to_string(),
                undo.undo_hint.or(action.undo_hint),
                action.actor_id,
                action.pane_id,
            )),
            _ => Ok(UndoExecutionResult::failed(
                action.id,
                undo.undo_strategy,
                "Unknown undo strategy".to_string(),
                undo.undo_hint.or(action.undo_hint),
                action.actor_id,
                action.pane_id,
            )),
        }
    }

    async fn execute_workflow_abort(
        &self,
        request: UndoRequest,
        action: &crate::storage::ActionHistoryRecord,
        undo: &ActionUndoRecord,
    ) -> Result<UndoExecutionResult> {
        let execution_id = execution_id_from_undo(undo, action);
        let Some(execution_id) = execution_id else {
            return Ok(UndoExecutionResult::not_applicable(
                action.id,
                undo.undo_strategy.clone(),
                "Undo payload did not contain a workflow execution ID".to_string(),
                undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                None,
                action.pane_id,
            ));
        };

        let runner = self.build_workflow_runner();
        match runner
            .abort_execution(&execution_id, request.reason.as_deref(), false)
            .await
        {
            Ok(result) if result.aborted => {
                let undone_at = self.mark_undone(action.id, &request.actor).await?;
                Ok(UndoExecutionResult::success(
                    action.id,
                    undo.undo_strategy.clone(),
                    format!("Aborted workflow {}", result.execution_id),
                    Some(result.execution_id),
                    Some(result.pane_id),
                    undone_at,
                ))
            }
            Ok(result) => {
                let reason = result
                    .error_reason
                    .unwrap_or_else(|| "not_applicable".to_string());
                let message = format!(
                    "Workflow {} is not undoable in current state ({reason})",
                    result.execution_id
                );
                Ok(UndoExecutionResult::not_applicable(
                    action.id,
                    undo.undo_strategy.clone(),
                    message,
                    undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                    Some(result.execution_id),
                    Some(result.pane_id),
                ))
            }
            Err(err) => Ok(UndoExecutionResult::failed(
                action.id,
                undo.undo_strategy.clone(),
                format!("Failed to abort workflow {execution_id}: {err}"),
                undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                Some(execution_id),
                action.pane_id,
            )),
        }
    }

    async fn execute_pane_close(
        &self,
        request: UndoRequest,
        action: &crate::storage::ActionHistoryRecord,
        undo: &ActionUndoRecord,
    ) -> Result<UndoExecutionResult> {
        let pane_id = pane_id_from_undo(undo).or(action.pane_id);
        let Some(pane_id) = pane_id else {
            return Ok(UndoExecutionResult::not_applicable(
                action.id,
                undo.undo_strategy.clone(),
                "Undo payload did not contain a pane ID".to_string(),
                undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                action.actor_id.clone(),
                None,
            ));
        };

        match self.wezterm.get_pane(pane_id).await {
            Ok(_) => {}
            Err(Error::Wezterm(WeztermError::PaneNotFound(_))) => {
                return Ok(UndoExecutionResult::not_applicable(
                    action.id,
                    undo.undo_strategy.clone(),
                    format!("Pane {pane_id} no longer exists"),
                    undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                    action.actor_id.clone(),
                    Some(pane_id),
                ));
            }
            Err(err) => {
                return Ok(UndoExecutionResult::failed(
                    action.id,
                    undo.undo_strategy.clone(),
                    format!("Failed to validate pane {pane_id}: {err}"),
                    undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                    action.actor_id.clone(),
                    Some(pane_id),
                ));
            }
        }

        match self.wezterm.kill_pane(pane_id).await {
            Ok(()) => {
                let undone_at = self.mark_undone(action.id, &request.actor).await?;
                Ok(UndoExecutionResult::success(
                    action.id,
                    undo.undo_strategy.clone(),
                    format!("Closed pane {pane_id}"),
                    action.actor_id.clone(),
                    Some(pane_id),
                    undone_at,
                ))
            }
            Err(Error::Wezterm(WeztermError::PaneNotFound(_))) => {
                Ok(UndoExecutionResult::not_applicable(
                    action.id,
                    undo.undo_strategy.clone(),
                    format!("Pane {pane_id} was already closed"),
                    undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                    action.actor_id.clone(),
                    Some(pane_id),
                ))
            }
            Err(err) => Ok(UndoExecutionResult::failed(
                action.id,
                undo.undo_strategy.clone(),
                format!("Failed to close pane {pane_id}: {err}"),
                undo.undo_hint.clone().or_else(|| action.undo_hint.clone()),
                action.actor_id.clone(),
                Some(pane_id),
            )),
        }
    }

    fn build_workflow_runner(&self) -> WorkflowRunner {
        let engine = WorkflowEngine::new(10);
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let policy = PolicyEngine::permissive();
        let injector = Arc::new(tokio::sync::Mutex::new(PolicyGatedInjector::with_storage(
            policy,
            Arc::clone(&self.wezterm),
            self.storage.as_ref().clone(),
        )));
        WorkflowRunner::new(
            engine,
            lock_manager,
            Arc::clone(&self.storage),
            injector,
            WorkflowRunnerConfig::default(),
        )
    }

    async fn mark_undone(&self, action_id: i64, actor: &str) -> Result<Option<i64>> {
        let updated = self.storage.mark_action_undone(action_id, actor).await?;
        if !updated {
            return Ok(None);
        }
        Ok(self
            .storage
            .get_action_undo(action_id)
            .await?
            .and_then(|row| row.undone_at))
    }
}

fn parse_undo_payload(undo: &ActionUndoRecord) -> Option<serde_json::Value> {
    undo.undo_payload
        .as_deref()
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
}

fn execution_id_from_undo(
    undo: &ActionUndoRecord,
    action: &crate::storage::ActionHistoryRecord,
) -> Option<String> {
    if let Some(value) = parse_undo_payload(undo).and_then(|payload| {
        payload
            .get("execution_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    }) {
        return Some(value);
    }

    if action.actor_kind == "workflow" {
        return action.actor_id.clone();
    }

    action.workflow_id.clone()
}

fn pane_id_from_undo(undo: &ActionUndoRecord) -> Option<u64> {
    let payload = parse_undo_payload(undo)?;
    let raw = payload.get("pane_id")?.as_u64()?;
    Some(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::storage::{AuditActionRecord, PaneRecord, WorkflowRecord, now_ms};
    use crate::wezterm::{MockWezterm, WeztermInterface};

    async fn seed_pane(storage: &StorageHandle, pane_id: u64) {
        let now = now_ms();
        storage
            .upsert_pane(PaneRecord {
                pane_id,
                pane_uuid: None,
                domain: "local".to_string(),
                window_id: Some(0),
                tab_id: Some(0),
                title: Some(format!("pane-{pane_id}")),
                cwd: Some("/tmp".to_string()),
                tty_name: None,
                first_seen_at: now,
                last_seen_at: now,
                observed: true,
                ignore_reason: None,
                last_decision_at: Some(now),
            })
            .await
            .expect("seed pane");
    }

    async fn seed_action(
        storage: &StorageHandle,
        pane_id: u64,
        actor_kind: &str,
        actor_id: Option<&str>,
        action_kind: &str,
    ) -> i64 {
        let now = now_ms();
        storage
            .record_audit_action(AuditActionRecord {
                id: 0,
                ts: now,
                actor_kind: actor_kind.to_string(),
                actor_id: actor_id.map(str::to_string),
                correlation_id: None,
                pane_id: Some(pane_id),
                domain: Some("local".to_string()),
                action_kind: action_kind.to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: None,
                rule_id: None,
                input_summary: None,
                verification_summary: None,
                decision_context: None,
                result: "success".to_string(),
            })
            .await
            .expect("seed audit action")
    }

    async fn seed_workflow(
        storage: &StorageHandle,
        execution_id: &str,
        pane_id: u64,
        status: &str,
    ) {
        let now = now_ms();
        let completed_at = if status == "running" || status == "waiting" {
            None
        } else {
            Some(now)
        };
        storage
            .upsert_workflow(WorkflowRecord {
                id: execution_id.to_string(),
                workflow_name: "test_workflow".to_string(),
                pane_id,
                trigger_event_id: None,
                current_step: 0,
                status: status.to_string(),
                wait_condition: None,
                context: None,
                result: None,
                error: None,
                started_at: now,
                updated_at: now,
                completed_at,
            })
            .await
            .expect("seed workflow");
    }

    #[tokio::test]
    async fn workflow_abort_undo_succeeds_and_marks_action_undone() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-workflow-success.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 42_u64;
        let execution_id = "wf-undo-success-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(
            storage.as_ref(),
            pane_id,
            "workflow",
            Some(execution_id),
            "workflow_start",
        )
        .await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "running").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "workflow_abort".to_string(),
                undo_hint: Some(format!("wa robot workflow abort {execution_id}")),
                undo_payload: Some(
                    serde_json::json!({ "execution_id": execution_id, "pane_id": pane_id })
                        .to_string(),
                ),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id).with_actor("test-user"))
            .await
            .expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.strategy, "workflow_abort");
        assert_eq!(result.target_workflow_id.as_deref(), Some(execution_id));

        let workflow = storage
            .get_workflow(execution_id)
            .await
            .expect("workflow query")
            .expect("workflow should exist");
        assert_eq!(workflow.status, "aborted");

        let undo = storage
            .get_action_undo(action_id)
            .await
            .expect("undo query")
            .expect("undo exists");
        assert!(undo.undone_at.is_some());
        assert_eq!(undo.undone_by.as_deref(), Some("test-user"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn workflow_abort_undo_is_not_applicable_when_workflow_completed() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-workflow-not-applicable.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 7_u64;
        let execution_id = "wf-undo-completed-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(
            storage.as_ref(),
            pane_id,
            "workflow",
            Some(execution_id),
            "workflow_start",
        )
        .await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "completed").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "workflow_abort".to_string(),
                undo_hint: Some(format!("wa robot workflow abort {execution_id}")),
                undo_payload: Some(serde_json::json!({ "execution_id": execution_id }).to_string()),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id))
            .await
            .expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("already_completed"));

        let undo = storage
            .get_action_undo(action_id)
            .await
            .expect("undo query")
            .expect("undo exists");
        assert!(undo.undone_at.is_none());

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn manual_strategy_returns_guidance() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-manual-guidance.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 11_u64;
        seed_pane(storage.as_ref(), pane_id).await;
        let action_id =
            seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: false,
                undo_strategy: "manual".to_string(),
                undo_hint: Some("Inspect pane state and reverse command manually.".to_string()),
                undo_payload: None,
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id))
            .await
            .expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert_eq!(
            result.guidance.as_deref(),
            Some("Inspect pane state and reverse command manually.")
        );

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn already_undone_action_returns_not_applicable_without_mutation() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-already-undone.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 21_u64;
        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;
        let initial_undone_at = now_ms() - 1_000;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: Some("Pane was already closed.".to_string()),
                undo_payload: Some(serde_json::json!({ "pane_id": pane_id }).to_string()),
                undone_at: Some(initial_undone_at),
                undone_by: Some("first-operator".to_string()),
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id).with_actor("second-operator"))
            .await
            .expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("already been undone"));

        let undo = storage
            .get_action_undo(action_id)
            .await
            .expect("undo query")
            .expect("undo exists");
        assert_eq!(undo.undone_at, Some(initial_undone_at));
        assert_eq!(undo.undone_by.as_deref(), Some("first-operator"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pane_close_undo_closes_existing_pane() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-close-success.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 55_u64;
        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: Some(format!("Close pane {pane_id}")),
                undo_payload: Some(serde_json::json!({ "pane_id": pane_id }).to_string()),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(pane_id).await;
        let executor = UndoExecutor::new(Arc::clone(&storage), mock.clone());
        let result = executor
            .execute(UndoRequest::new(action_id).with_actor("operator"))
            .await
            .expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.target_pane_id, Some(pane_id));

        let pane_lookup = mock.get_pane(pane_id).await;
        assert!(matches!(
            pane_lookup,
            Err(Error::Wezterm(WeztermError::PaneNotFound(id))) if id == pane_id
        ));

        let undo = storage
            .get_action_undo(action_id)
            .await
            .expect("undo query")
            .expect("undo exists");
        assert!(undo.undone_at.is_some());
        assert_eq!(undo.undone_by.as_deref(), Some("operator"));

        storage.shutdown().await.expect("shutdown");
    }
}
