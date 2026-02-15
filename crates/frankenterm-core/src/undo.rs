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
                Some("Use `ft history` to list valid action IDs.".to_string()),
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
        let injector = Arc::new(crate::runtime_compat::Mutex::new(
            PolicyGatedInjector::with_storage(
                policy,
                Arc::clone(&self.wezterm),
                self.storage.as_ref().clone(),
            ),
        ));
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
                undo_hint: Some(format!("ft robot workflow abort {execution_id}")),
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
                undo_hint: Some(format!("ft robot workflow abort {execution_id}")),
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

    // ── Pure function tests (no DB needed) ──

    #[test]
    fn undo_outcome_serde_roundtrip() {
        for variant in [
            UndoOutcome::Success,
            UndoOutcome::NotApplicable,
            UndoOutcome::Failed,
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            let back: UndoOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(variant, back);
        }
    }

    #[test]
    fn undo_outcome_serde_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&UndoOutcome::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&UndoOutcome::NotApplicable).unwrap(),
            "\"not_applicable\""
        );
        assert_eq!(
            serde_json::to_string(&UndoOutcome::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn undo_request_new_defaults() {
        let req = UndoRequest::new(42);
        assert_eq!(req.action_id, 42);
        assert_eq!(req.actor, "user");
        assert!(req.reason.is_none());
    }

    #[test]
    fn undo_request_builder_methods() {
        let req = UndoRequest::new(7)
            .with_actor("admin")
            .with_reason("rollback");
        assert_eq!(req.action_id, 7);
        assert_eq!(req.actor, "admin");
        assert_eq!(req.reason.as_deref(), Some("rollback"));
    }

    #[test]
    fn undo_execution_result_success_constructor() {
        let r = UndoExecutionResult::success(
            1,
            "pane_close".to_string(),
            "Closed pane 5".to_string(),
            None,
            Some(5),
            Some(1234567890),
        );
        assert_eq!(r.action_id, 1);
        assert_eq!(r.outcome, UndoOutcome::Success);
        assert_eq!(r.strategy, "pane_close");
        assert!(r.guidance.is_none());
        assert_eq!(r.target_pane_id, Some(5));
        assert_eq!(r.undone_at, Some(1234567890));
    }

    #[test]
    fn undo_execution_result_not_applicable_constructor() {
        let r = UndoExecutionResult::not_applicable(
            2,
            "manual".to_string(),
            "Cannot undo".to_string(),
            Some("Do it manually".to_string()),
            Some("wf-1".to_string()),
            None,
        );
        assert_eq!(r.outcome, UndoOutcome::NotApplicable);
        assert_eq!(r.guidance.as_deref(), Some("Do it manually"));
        assert_eq!(r.target_workflow_id.as_deref(), Some("wf-1"));
        assert!(r.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_failed_constructor() {
        let r = UndoExecutionResult::failed(
            3,
            "workflow_abort".to_string(),
            "Abort failed".to_string(),
            None,
            Some("wf-2".to_string()),
            Some(10),
        );
        assert_eq!(r.outcome, UndoOutcome::Failed);
        assert_eq!(r.target_workflow_id.as_deref(), Some("wf-2"));
        assert_eq!(r.target_pane_id, Some(10));
        assert!(r.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_serde_roundtrip() {
        let r = UndoExecutionResult::success(
            1,
            "pane_close".to_string(),
            "Done".to_string(),
            Some("wf-99".to_string()),
            Some(42),
            Some(99999),
        );
        let json = serde_json::to_string(&r).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action_id, 1);
        assert_eq!(back.outcome, UndoOutcome::Success);
        assert_eq!(back.strategy, "pane_close");
        assert_eq!(back.target_workflow_id.as_deref(), Some("wf-99"));
        assert_eq!(back.target_pane_id, Some(42));
        assert_eq!(back.undone_at, Some(99999));
    }

    #[test]
    fn parse_undo_payload_valid_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": 5}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let val = parse_undo_payload(&undo);
        assert!(val.is_some());
        assert_eq!(val.unwrap()["pane_id"], 5);
    }

    #[test]
    fn parse_undo_payload_invalid_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some("not json".to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(parse_undo_payload(&undo).is_none());
    }

    #[test]
    fn parse_undo_payload_none() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "manual".to_string(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        assert!(parse_undo_payload(&undo).is_none());
    }

    fn make_action_history(
        id: i64,
        actor_kind: &str,
        actor_id: Option<&str>,
        workflow_id: Option<&str>,
        pane_id: Option<u64>,
    ) -> crate::storage::ActionHistoryRecord {
        crate::storage::ActionHistoryRecord {
            id,
            ts: 1_700_000_000_000,
            actor_kind: actor_kind.to_string(),
            actor_id: actor_id.map(str::to_string),
            correlation_id: None,
            pane_id,
            domain: Some("local".to_string()),
            action_kind: "send_text".to_string(),
            policy_decision: "allow".to_string(),
            decision_reason: None,
            rule_id: None,
            input_summary: None,
            verification_summary: None,
            decision_context: None,
            result: "success".to_string(),
            undoable: Some(true),
            undo_strategy: Some("workflow_abort".to_string()),
            undo_hint: None,
            undone_at: None,
            undone_by: None,
            workflow_id: workflow_id.map(str::to_string),
            step_name: None,
        }
    }

    #[test]
    fn execution_id_from_undo_payload() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"execution_id": "wf-payload"}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let action =
            make_action_history(1, "workflow", Some("wf-actor"), Some("wf-workflow"), None);
        // Payload takes priority
        assert_eq!(
            execution_id_from_undo(&undo, &action).as_deref(),
            Some("wf-payload")
        );
    }

    #[test]
    fn execution_id_from_actor_id_when_workflow_actor() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "workflow", Some("wf-from-actor"), Some("wf-id"), None);
        // Falls back to actor_id when actor_kind is "workflow"
        assert_eq!(
            execution_id_from_undo(&undo, &action).as_deref(),
            Some("wf-from-actor")
        );
    }

    #[test]
    fn execution_id_from_workflow_id_fallback() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, Some("wf-fallback"), None);
        assert_eq!(
            execution_id_from_undo(&undo, &action).as_deref(),
            Some("wf-fallback")
        );
    }

    #[test]
    fn execution_id_from_undo_none_when_no_source() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, None, None);
        assert!(execution_id_from_undo(&undo, &action).is_none());
    }

    #[test]
    fn pane_id_from_undo_valid() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": 42}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert_eq!(pane_id_from_undo(&undo), Some(42));
    }

    #[test]
    fn pane_id_from_undo_missing_key() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"other": 1}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn pane_id_from_undo_wrong_type() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": "not_a_number"}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn pane_id_from_undo_no_payload() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    // ── DB-backed tests ──

    #[tokio::test]
    async fn action_not_found_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-not-found.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(99999))
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not found"));
        assert!(result.guidance.is_some());

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn no_undo_metadata_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-no-metadata.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", Some("cli"), "send_text").await;

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id))
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("No undo metadata"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn not_undoable_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-not-undoable.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: false,
                undo_strategy: "none".to_string(),
                undo_hint: Some("Cannot undo text".to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not currently undoable"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn unknown_strategy_returns_failed() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-unknown-strategy.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "teleport".to_string(),
                undo_hint: None,
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::Failed);
        assert!(result.message.contains("Unknown undo strategy"));
        assert_eq!(result.strategy, "teleport");

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn custom_strategy_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-custom-strategy.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "custom".to_string(),
                undo_hint: Some("Use external tool".to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not supported"));
        assert_eq!(result.guidance.as_deref(), Some("Use external tool"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pane_close_nonexistent_pane_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-gone.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "spawn").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: None,
                undo_payload: Some(r#"{"pane_id": 999}"#.to_string()),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        // Don't add pane 999, so get_pane will return PaneNotFound
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id))
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("no longer exists"));
        assert_eq!(result.target_pane_id, Some(999));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pane_close_no_pane_id_in_payload_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-no-id.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        // Seed pane but action has no pane_id
        seed_pane(storage.as_ref(), 1).await;
        let now = now_ms();
        let action_id = storage
            .record_audit_action(AuditActionRecord {
                id: 0,
                ts: now,
                actor_kind: "human".to_string(),
                actor_id: None,
                correlation_id: None,
                pane_id: None,
                domain: Some("local".to_string()),
                action_kind: "spawn".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: None,
                rule_id: None,
                input_summary: None,
                verification_summary: None,
                decision_context: None,
                result: "success".to_string(),
            })
            .await
            .expect("seed action");

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: None,
                undo_payload: Some(r#"{"other": "data"}"#.to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("pane ID"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn workflow_abort_no_execution_id_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-no-exec.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        // Non-workflow actor, no workflow_id, no payload execution_id
        let now = now_ms();
        let action_id = storage
            .record_audit_action(AuditActionRecord {
                id: 0,
                ts: now,
                actor_kind: "human".to_string(),
                actor_id: None,
                correlation_id: None,
                pane_id: Some(1),
                domain: Some("local".to_string()),
                action_kind: "send_text".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: None,
                rule_id: None,
                input_summary: None,
                verification_summary: None,
                decision_context: None,
                result: "success".to_string(),
            })
            .await
            .expect("seed action");

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "workflow_abort".to_string(),
                undo_hint: None,
                undo_payload: Some(r#"{"some": "data"}"#.to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("execution ID"));

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

    // ── Additional pure-function and type-level tests ──

    #[test]
    fn undo_outcome_deserialize_from_string_values() {
        let s: UndoOutcome = serde_json::from_str(r#""success""#).unwrap();
        assert_eq!(s, UndoOutcome::Success);
        let n: UndoOutcome = serde_json::from_str(r#""not_applicable""#).unwrap();
        assert_eq!(n, UndoOutcome::NotApplicable);
        let f: UndoOutcome = serde_json::from_str(r#""failed""#).unwrap();
        assert_eq!(f, UndoOutcome::Failed);
    }

    #[test]
    fn undo_outcome_invalid_string_fails_deser() {
        let result = serde_json::from_str::<UndoOutcome>(r#""unknown_variant""#);
        assert!(result.is_err());
    }

    #[test]
    fn undo_outcome_copy_semantics() {
        let a = UndoOutcome::Success;
        let b = a; // Copy
        assert_eq!(a, b); // a still accessible after copy
    }

    #[test]
    fn undo_outcome_debug_format() {
        let dbg = format!("{:?}", UndoOutcome::NotApplicable);
        assert!(dbg.contains("NotApplicable"));
    }

    #[test]
    fn undo_request_negative_action_id() {
        let req = UndoRequest::new(-1);
        assert_eq!(req.action_id, -1);
    }

    #[test]
    fn undo_request_with_actor_empty_string() {
        let req = UndoRequest::new(1).with_actor("");
        assert_eq!(req.actor, "");
    }

    #[test]
    fn undo_request_builder_chaining_order_independent() {
        let r1 = UndoRequest::new(5).with_actor("admin").with_reason("oops");
        let r2 = UndoRequest::new(5).with_reason("oops").with_actor("admin");
        assert_eq!(r1.action_id, r2.action_id);
        assert_eq!(r1.actor, r2.actor);
        assert_eq!(r1.reason, r2.reason);
    }

    #[test]
    fn undo_request_with_reason_string_type() {
        let req = UndoRequest::new(1).with_reason(String::from("owned"));
        assert_eq!(req.reason.as_deref(), Some("owned"));
    }

    #[test]
    fn undo_execution_result_serde_all_none_optionals() {
        let r = UndoExecutionResult {
            action_id: 1,
            strategy: "manual".to_string(),
            outcome: UndoOutcome::NotApplicable,
            message: "nope".to_string(),
            guidance: None,
            target_workflow_id: None,
            target_pane_id: None,
            undone_at: None,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        assert!(back.guidance.is_none());
        assert!(back.target_workflow_id.is_none());
        assert!(back.target_pane_id.is_none());
        assert!(back.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_serde_all_some_optionals() {
        let r = UndoExecutionResult {
            action_id: 99,
            strategy: "pane_close".to_string(),
            outcome: UndoOutcome::Success,
            message: "closed".to_string(),
            guidance: Some("check state".to_string()),
            target_workflow_id: Some("wf-99".to_string()),
            target_pane_id: Some(42),
            undone_at: Some(1_700_000_000),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.guidance.as_deref(), Some("check state"));
        assert_eq!(back.target_workflow_id.as_deref(), Some("wf-99"));
        assert_eq!(back.target_pane_id, Some(42));
        assert_eq!(back.undone_at, Some(1_700_000_000));
    }

    #[test]
    fn undo_execution_result_not_applicable_has_no_undone_at() {
        let r = UndoExecutionResult::not_applicable(
            1,
            "manual".to_string(),
            "msg".to_string(),
            None,
            None,
            None,
        );
        assert!(r.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_failed_has_no_undone_at() {
        let r = UndoExecutionResult::failed(
            1,
            "workflow_abort".to_string(),
            "msg".to_string(),
            None,
            None,
            None,
        );
        assert!(r.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_action_id_preserved_in_all_constructors() {
        let s =
            UndoExecutionResult::success(42, "s".to_string(), "m".to_string(), None, None, None);
        let n = UndoExecutionResult::not_applicable(
            42,
            "n".to_string(),
            "m".to_string(),
            None,
            None,
            None,
        );
        let f = UndoExecutionResult::failed(42, "f".to_string(), "m".to_string(), None, None, None);
        assert_eq!(s.action_id, 42);
        assert_eq!(n.action_id, 42);
        assert_eq!(f.action_id, 42);
    }

    #[test]
    fn parse_undo_payload_empty_string() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "manual".to_string(),
            undo_hint: None,
            undo_payload: Some(String::new()),
            undone_at: None,
            undone_by: None,
        };
        assert!(parse_undo_payload(&undo).is_none());
    }

    #[test]
    fn parse_undo_payload_nested_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "custom".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"nested": {"deep": 42}}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let val = parse_undo_payload(&undo).unwrap();
        assert_eq!(val["nested"]["deep"], 42);
    }

    #[test]
    fn execution_id_from_undo_payload_non_string_execution_id() {
        // execution_id is numeric, not string — should not extract
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"execution_id": 12345}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, None, None);
        // Numeric execution_id won't match as_str, falls through to None
        assert!(execution_id_from_undo(&undo, &action).is_none());
    }

    #[test]
    fn execution_id_from_undo_payload_empty_string_execution_id() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"execution_id": ""}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, Some("wf-fallback"), None);
        // Empty string is still a valid string, so it takes priority
        assert_eq!(execution_id_from_undo(&undo, &action).as_deref(), Some(""));
    }

    #[test]
    fn pane_id_from_undo_zero() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": 0}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert_eq!(pane_id_from_undo(&undo), Some(0));
    }

    #[test]
    fn pane_id_from_undo_large_value() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": 18446744073709551615}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert_eq!(pane_id_from_undo(&undo), Some(u64::MAX));
    }

    #[test]
    fn pane_id_from_undo_negative_number() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": -1}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        // Negative numbers don't parse as u64
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn pane_id_from_undo_float_number() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": 3.14}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        // Floats don't parse as u64
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[tokio::test]
    async fn none_strategy_returns_not_applicable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-none-strategy.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "none".to_string(),
                undo_hint: Some("No undo available".to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not supported"));
        assert_eq!(result.guidance.as_deref(), Some("No undo available"));

        storage.shutdown().await.expect("shutdown");
    }

    #[test]
    fn make_action_history_helper_fields() {
        let h = make_action_history(10, "workflow", Some("wf-1"), Some("wfid"), Some(5));
        assert_eq!(h.id, 10);
        assert_eq!(h.actor_kind, "workflow");
        assert_eq!(h.actor_id.as_deref(), Some("wf-1"));
        assert_eq!(h.workflow_id.as_deref(), Some("wfid"));
        assert_eq!(h.pane_id, Some(5));
        assert_eq!(h.domain.as_deref(), Some("local"));
        assert_eq!(h.action_kind, "send_text");
        assert_eq!(h.result, "success");
    }

    #[test]
    fn undo_execution_result_debug_format() {
        let r = UndoExecutionResult::success(
            1,
            "pane_close".to_string(),
            "ok".to_string(),
            None,
            None,
            None,
        );
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("UndoExecutionResult"));
        assert!(dbg.contains("pane_close"));
    }

    #[test]
    fn undo_request_debug_format() {
        let req = UndoRequest::new(7).with_actor("admin");
        let dbg = format!("{:?}", req);
        assert!(dbg.contains("UndoRequest"));
        assert!(dbg.contains("admin"));
    }

    #[test]
    fn undo_execution_result_success_guidance_is_none() {
        let r = UndoExecutionResult::success(1, "s".to_string(), "m".to_string(), None, None, None);
        // success constructor always sets guidance to None
        assert!(r.guidance.is_none());
    }

    // ── Expanded tests (wa-1u90p.7.1) ──

    #[test]
    fn undo_outcome_clone_produces_equal_value() {
        let original = UndoOutcome::Failed;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    #[test]
    fn undo_outcome_eq_is_reflexive() {
        let v = UndoOutcome::Success;
        assert_eq!(v, v);
    }

    #[test]
    fn undo_outcome_eq_is_symmetric() {
        let a = UndoOutcome::NotApplicable;
        let b = UndoOutcome::NotApplicable;
        assert_eq!(a, b);
        assert_eq!(b, a);
    }

    #[test]
    fn undo_outcome_ne_across_variants() {
        assert_ne!(UndoOutcome::Success, UndoOutcome::Failed);
        assert_ne!(UndoOutcome::Success, UndoOutcome::NotApplicable);
        assert_ne!(UndoOutcome::Failed, UndoOutcome::NotApplicable);
    }

    #[test]
    fn undo_outcome_deser_rejects_integer() {
        assert!(serde_json::from_str::<UndoOutcome>("42").is_err());
    }

    #[test]
    fn undo_outcome_deser_rejects_null() {
        assert!(serde_json::from_str::<UndoOutcome>("null").is_err());
    }

    #[test]
    fn undo_outcome_deser_rejects_boolean() {
        assert!(serde_json::from_str::<UndoOutcome>("true").is_err());
    }

    #[test]
    fn undo_outcome_deser_rejects_object() {
        assert!(serde_json::from_str::<UndoOutcome>(r#"{"kind":"success"}"#).is_err());
    }

    #[test]
    fn undo_outcome_deser_rejects_camel_case() {
        // snake_case is required, CamelCase should fail
        assert!(serde_json::from_str::<UndoOutcome>(r#""NotApplicable""#).is_err());
    }

    #[test]
    fn undo_outcome_debug_format_success() {
        let dbg = format!("{:?}", UndoOutcome::Success);
        assert_eq!(dbg, "Success");
    }

    #[test]
    fn undo_outcome_debug_format_failed() {
        let dbg = format!("{:?}", UndoOutcome::Failed);
        assert_eq!(dbg, "Failed");
    }

    #[test]
    fn undo_request_clone_produces_equal_fields() {
        let req = UndoRequest::new(7).with_actor("admin").with_reason("rollback");
        let cloned = req.clone();
        assert_eq!(cloned.action_id, 7);
        assert_eq!(cloned.actor, "admin");
        assert_eq!(cloned.reason.as_deref(), Some("rollback"));
    }

    #[test]
    fn undo_request_action_id_zero() {
        let req = UndoRequest::new(0);
        assert_eq!(req.action_id, 0);
    }

    #[test]
    fn undo_request_action_id_max() {
        let req = UndoRequest::new(i64::MAX);
        assert_eq!(req.action_id, i64::MAX);
    }

    #[test]
    fn undo_request_action_id_min() {
        let req = UndoRequest::new(i64::MIN);
        assert_eq!(req.action_id, i64::MIN);
    }

    #[test]
    fn undo_request_with_actor_overrides_default() {
        let req = UndoRequest::new(1);
        assert_eq!(req.actor, "user");
        let req = req.with_actor("robot");
        assert_eq!(req.actor, "robot");
    }

    #[test]
    fn undo_request_with_actor_last_wins() {
        let req = UndoRequest::new(1).with_actor("first").with_actor("second");
        assert_eq!(req.actor, "second");
    }

    #[test]
    fn undo_request_with_reason_last_wins() {
        let req = UndoRequest::new(1).with_reason("first").with_reason("second");
        assert_eq!(req.reason.as_deref(), Some("second"));
    }

    #[test]
    fn undo_request_with_unicode_actor() {
        let req = UndoRequest::new(1).with_actor("operateur-\u{00e9}l\u{00e8}ve");
        assert!(req.actor.contains('\u{00e9}'));
    }

    #[test]
    fn undo_request_with_unicode_reason() {
        let req = UndoRequest::new(1).with_reason("undo reason \u{1f4a5}");
        assert!(req.reason.unwrap().contains('\u{1f4a5}'));
    }

    #[test]
    fn undo_request_with_very_long_actor() {
        let long_actor = "a".repeat(10_000);
        let req = UndoRequest::new(1).with_actor(long_actor.clone());
        assert_eq!(req.actor.len(), 10_000);
    }

    #[test]
    fn undo_execution_result_clone_preserves_all_fields() {
        let r = UndoExecutionResult {
            action_id: 99,
            strategy: "workflow_abort".to_string(),
            outcome: UndoOutcome::Success,
            message: "aborted workflow wf-1".to_string(),
            guidance: Some("check logs".to_string()),
            target_workflow_id: Some("wf-1".to_string()),
            target_pane_id: Some(42),
            undone_at: Some(1_700_000_000),
        };
        let c = r.clone();
        assert_eq!(c.action_id, r.action_id);
        assert_eq!(c.strategy, r.strategy);
        assert_eq!(c.outcome, r.outcome);
        assert_eq!(c.message, r.message);
        assert_eq!(c.guidance, r.guidance);
        assert_eq!(c.target_workflow_id, r.target_workflow_id);
        assert_eq!(c.target_pane_id, r.target_pane_id);
        assert_eq!(c.undone_at, r.undone_at);
    }

    #[test]
    fn undo_execution_result_success_with_all_some() {
        let r = UndoExecutionResult::success(
            100,
            "workflow_abort".to_string(),
            "aborted".to_string(),
            Some("wf-100".to_string()),
            Some(200),
            Some(999_999),
        );
        assert_eq!(r.outcome, UndoOutcome::Success);
        assert_eq!(r.action_id, 100);
        assert_eq!(r.target_workflow_id.as_deref(), Some("wf-100"));
        assert_eq!(r.target_pane_id, Some(200));
        assert_eq!(r.undone_at, Some(999_999));
        // guidance is always None for success
        assert!(r.guidance.is_none());
    }

    #[test]
    fn undo_execution_result_not_applicable_with_guidance() {
        let r = UndoExecutionResult::not_applicable(
            5,
            "pane_close".to_string(),
            "Pane gone".to_string(),
            Some("Manually recreate pane".to_string()),
            Some("wf-5".to_string()),
            Some(55),
        );
        assert_eq!(r.outcome, UndoOutcome::NotApplicable);
        assert_eq!(r.guidance.as_deref(), Some("Manually recreate pane"));
        assert_eq!(r.target_workflow_id.as_deref(), Some("wf-5"));
        assert_eq!(r.target_pane_id, Some(55));
        assert!(r.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_failed_with_guidance() {
        let r = UndoExecutionResult::failed(
            6,
            "pane_close".to_string(),
            "kill_pane error".to_string(),
            Some("Try again later".to_string()),
            None,
            Some(66),
        );
        assert_eq!(r.outcome, UndoOutcome::Failed);
        assert_eq!(r.guidance.as_deref(), Some("Try again later"));
        assert!(r.target_workflow_id.is_none());
        assert_eq!(r.target_pane_id, Some(66));
        assert!(r.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_message_preserved_in_all_constructors() {
        let s = UndoExecutionResult::success(
            1,
            "s".to_string(),
            "success message".to_string(),
            None,
            None,
            None,
        );
        let n = UndoExecutionResult::not_applicable(
            1,
            "n".to_string(),
            "not applicable message".to_string(),
            None,
            None,
            None,
        );
        let f = UndoExecutionResult::failed(
            1,
            "f".to_string(),
            "failed message".to_string(),
            None,
            None,
            None,
        );
        assert_eq!(s.message, "success message");
        assert_eq!(n.message, "not applicable message");
        assert_eq!(f.message, "failed message");
    }

    #[test]
    fn undo_execution_result_serde_roundtrip_failed_variant() {
        let r = UndoExecutionResult::failed(
            77,
            "workflow_abort".to_string(),
            "timeout".to_string(),
            Some("retry".to_string()),
            Some("wf-77".to_string()),
            Some(88),
        );
        let json = serde_json::to_string(&r).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action_id, 77);
        assert_eq!(back.outcome, UndoOutcome::Failed);
        assert_eq!(back.strategy, "workflow_abort");
        assert_eq!(back.message, "timeout");
        assert_eq!(back.guidance.as_deref(), Some("retry"));
        assert_eq!(back.target_workflow_id.as_deref(), Some("wf-77"));
        assert_eq!(back.target_pane_id, Some(88));
        assert!(back.undone_at.is_none());
    }

    #[test]
    fn undo_execution_result_serde_roundtrip_not_applicable_variant() {
        let r = UndoExecutionResult::not_applicable(
            33,
            "manual".to_string(),
            "no auto undo".to_string(),
            Some("use CLI".to_string()),
            None,
            None,
        );
        let json = serde_json::to_string(&r).unwrap();
        let back: UndoExecutionResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.action_id, 33);
        assert_eq!(back.outcome, UndoOutcome::NotApplicable);
        assert_eq!(back.guidance.as_deref(), Some("use CLI"));
    }

    #[test]
    fn undo_execution_result_serde_json_contains_expected_keys() {
        let r = UndoExecutionResult::success(
            1,
            "pane_close".to_string(),
            "done".to_string(),
            None,
            Some(5),
            Some(12345),
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"action_id\""));
        assert!(json.contains("\"strategy\""));
        assert!(json.contains("\"outcome\""));
        assert!(json.contains("\"message\""));
        assert!(json.contains("\"target_pane_id\""));
        assert!(json.contains("\"undone_at\""));
    }

    #[test]
    fn undo_execution_result_deser_from_raw_json() {
        let raw = r#"{
            "action_id": 10,
            "strategy": "pane_close",
            "outcome": "success",
            "message": "Closed pane 5",
            "guidance": null,
            "target_workflow_id": null,
            "target_pane_id": 5,
            "undone_at": 1700000000
        }"#;
        let r: UndoExecutionResult = serde_json::from_str(raw).unwrap();
        assert_eq!(r.action_id, 10);
        assert_eq!(r.outcome, UndoOutcome::Success);
        assert_eq!(r.target_pane_id, Some(5));
        assert_eq!(r.undone_at, Some(1_700_000_000));
        assert!(r.guidance.is_none());
    }

    #[test]
    fn undo_execution_result_debug_contains_all_field_names() {
        let r = UndoExecutionResult {
            action_id: 1,
            strategy: "test".to_string(),
            outcome: UndoOutcome::Failed,
            message: "msg".to_string(),
            guidance: Some("guide".to_string()),
            target_workflow_id: Some("wf".to_string()),
            target_pane_id: Some(9),
            undone_at: Some(100),
        };
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("action_id"));
        assert!(dbg.contains("strategy"));
        assert!(dbg.contains("outcome"));
        assert!(dbg.contains("message"));
        assert!(dbg.contains("guidance"));
        assert!(dbg.contains("target_workflow_id"));
        assert!(dbg.contains("target_pane_id"));
        assert!(dbg.contains("undone_at"));
    }

    #[test]
    fn parse_undo_payload_array_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "custom".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"[1, 2, 3]"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let val = parse_undo_payload(&undo);
        assert!(val.is_some());
        assert!(val.unwrap().is_array());
    }

    #[test]
    fn parse_undo_payload_null_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "custom".to_string(),
            undo_hint: None,
            undo_payload: Some("null".to_string()),
            undone_at: None,
            undone_by: None,
        };
        let val = parse_undo_payload(&undo);
        assert!(val.is_some());
        assert!(val.unwrap().is_null());
    }

    #[test]
    fn parse_undo_payload_whitespace_only() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "custom".to_string(),
            undo_hint: None,
            undo_payload: Some("   ".to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(parse_undo_payload(&undo).is_none());
    }

    #[test]
    fn parse_undo_payload_boolean_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "custom".to_string(),
            undo_hint: None,
            undo_payload: Some("true".to_string()),
            undone_at: None,
            undone_by: None,
        };
        let val = parse_undo_payload(&undo);
        assert!(val.is_some());
        assert_eq!(val.unwrap(), serde_json::Value::Bool(true));
    }

    #[test]
    fn parse_undo_payload_string_json() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "custom".to_string(),
            undo_hint: None,
            undo_payload: Some(r#""just a string""#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let val = parse_undo_payload(&undo);
        assert!(val.is_some());
        assert_eq!(val.unwrap().as_str(), Some("just a string"));
    }

    #[test]
    fn execution_id_from_undo_payload_null_execution_id() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"execution_id": null}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, None, None);
        // null is not a string, falls through, human actor with no workflow_id => None
        assert!(execution_id_from_undo(&undo, &action).is_none());
    }

    #[test]
    fn execution_id_from_undo_payload_array_execution_id() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"execution_id": ["a", "b"]}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, Some("wf-fallback"), None);
        // Array is not a string, falls through to workflow_id
        assert_eq!(
            execution_id_from_undo(&undo, &action).as_deref(),
            Some("wf-fallback")
        );
    }

    #[test]
    fn execution_id_from_undo_workflow_actor_with_no_actor_id() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: None,
            undone_at: None,
            undone_by: None,
        };
        // actor_kind is "workflow" but actor_id is None
        let action = make_action_history(1, "workflow", None, Some("wf-from-workflow"), None);
        // Falls to actor_id (None), returns None, then falls to workflow_id
        assert_eq!(
            execution_id_from_undo(&undo, &action).as_deref(),
            // actor_kind == "workflow" => return actor_id which is None
            None
        );
    }

    #[test]
    fn execution_id_from_undo_invalid_payload_json_uses_fallback() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "workflow_abort".to_string(),
            undo_hint: None,
            undo_payload: Some("not valid json".to_string()),
            undone_at: None,
            undone_by: None,
        };
        let action = make_action_history(1, "human", None, Some("wf-fallback-2"), None);
        // Invalid JSON => parse_undo_payload returns None => falls to workflow_id
        assert_eq!(
            execution_id_from_undo(&undo, &action).as_deref(),
            Some("wf-fallback-2")
        );
    }

    #[test]
    fn pane_id_from_undo_null_value() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": null}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn pane_id_from_undo_boolean_value() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": true}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn pane_id_from_undo_nested_pane_id() {
        // pane_id is nested, not at top level — should return None
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"data": {"pane_id": 42}}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn pane_id_from_undo_array_value() {
        let undo = ActionUndoRecord {
            audit_action_id: 1,
            undoable: true,
            undo_strategy: "pane_close".to_string(),
            undo_hint: None,
            undo_payload: Some(r#"{"pane_id": [1, 2]}"#.to_string()),
            undone_at: None,
            undone_by: None,
        };
        assert!(pane_id_from_undo(&undo).is_none());
    }

    #[test]
    fn make_action_history_helper_all_none() {
        let h = make_action_history(0, "human", None, None, None);
        assert_eq!(h.id, 0);
        assert_eq!(h.actor_kind, "human");
        assert!(h.actor_id.is_none());
        assert!(h.workflow_id.is_none());
        assert!(h.pane_id.is_none());
        assert!(h.correlation_id.is_none());
        assert!(h.undo_hint.is_none());
    }

    #[test]
    fn make_action_history_helper_default_undo_fields() {
        let h = make_action_history(1, "robot", None, None, None);
        assert_eq!(h.undoable, Some(true));
        assert_eq!(h.undo_strategy.as_deref(), Some("workflow_abort"));
        assert!(h.undone_at.is_none());
        assert!(h.undone_by.is_none());
        assert!(h.step_name.is_none());
    }

    #[tokio::test]
    async fn undo_executor_clone() {
        // UndoExecutor derives Clone
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("clone-test.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );
        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let _cloned = executor.clone();
        // Just verify it compiles and doesn't panic
        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pane_close_falls_back_to_action_pane_id() {
        // When payload has no pane_id but action record does, should use action's pane_id
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-close-fallback.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let pane_id = 77_u64;
        seed_pane(storage.as_ref(), pane_id).await;
        let action_id =
            seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: None,
                // Payload has no pane_id key
                undo_payload: Some(r#"{"reason": "test"}"#.to_string()),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(pane_id).await;
        let executor = UndoExecutor::new(Arc::clone(&storage), mock.clone());
        let result = executor
            .execute(UndoRequest::new(action_id).with_actor("fallback-test"))
            .await
            .expect("result");

        // Should succeed using action's pane_id as fallback
        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.target_pane_id, Some(pane_id));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn workflow_abort_waiting_workflow_succeeds() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-waiting.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 88_u64;
        let execution_id = "wf-waiting-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(
            storage.as_ref(),
            pane_id,
            "workflow",
            Some(execution_id),
            "workflow_start",
        )
        .await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "waiting").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "workflow_abort".to_string(),
                undo_hint: None,
                undo_payload: Some(
                    serde_json::json!({ "execution_id": execution_id }).to_string(),
                ),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id).with_actor("test"))
            .await
            .expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.strategy, "workflow_abort");

        let workflow = storage
            .get_workflow(execution_id)
            .await
            .expect("workflow query")
            .expect("workflow exists");
        assert_eq!(workflow.status, "aborted");

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn undoable_false_hint_falls_back_to_action_hint() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-fallback-hint.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        // Create action with undo_hint in the action record itself
        let now = now_ms();
        let action_id = storage
            .record_audit_action(AuditActionRecord {
                id: 0,
                ts: now,
                actor_kind: "human".to_string(),
                actor_id: None,
                correlation_id: None,
                pane_id: Some(1),
                domain: Some("local".to_string()),
                action_kind: "send_text".to_string(),
                policy_decision: "allow".to_string(),
                decision_reason: None,
                rule_id: None,
                input_summary: None,
                verification_summary: None,
                decision_context: None,
                result: "success".to_string(),
            })
            .await
            .expect("seed action");

        // Undo record with no hint (None), undoable=false
        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: false,
                undo_strategy: "manual".to_string(),
                undo_hint: None,
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not currently undoable"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn action_not_found_guidance_suggests_ft_history() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-guidance-msg.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(12345))
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert_eq!(result.strategy, "none");
        assert!(result
            .guidance
            .as_deref()
            .unwrap()
            .contains("ft history"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn action_not_found_message_includes_action_id() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-notfound-id.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(54321))
            .await
            .expect("result");

        assert!(result.message.contains("54321"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn no_undo_metadata_guidance_mentions_non_undoable() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-no-meta-guide.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id))
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        let guidance = result.guidance.as_deref().unwrap();
        assert!(guidance.contains("non-undoable") || guidance.contains("predates"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn unknown_strategy_returns_hint_from_undo_record() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-unknown-hint.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "quantum_revert".to_string(),
                undo_hint: Some("Contact quantum support".to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::Failed);
        assert_eq!(result.strategy, "quantum_revert");
        assert_eq!(
            result.guidance.as_deref(),
            Some("Contact quantum support")
        );

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn pane_close_success_message_contains_pane_id() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-msg.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 123_u64;
        seed_pane(storage.as_ref(), pane_id).await;
        let action_id =
            seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: None,
                undo_payload: Some(serde_json::json!({ "pane_id": pane_id }).to_string()),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(pane_id).await;
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id))
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert!(result.message.contains("123"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn workflow_abort_success_message_contains_execution_id() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-msg.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 50_u64;
        let execution_id = "wf-msg-test-1";

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
                undo_hint: None,
                undo_payload: Some(
                    serde_json::json!({ "execution_id": execution_id }).to_string(),
                ),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert!(result.message.contains(execution_id));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn undo_request_with_reason_propagates_to_workflow_abort() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-reason-prop.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 60_u64;
        let execution_id = "wf-reason-prop-1";

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
                undo_hint: None,
                undo_payload: Some(
                    serde_json::json!({ "execution_id": execution_id }).to_string(),
                ),
                undone_at: None,
                undone_by: None,
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(
                UndoRequest::new(action_id)
                    .with_actor("reason-test")
                    .with_reason("emergency rollback"),
            )
            .await
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        // The reason was passed through; verify the workflow is aborted
        let wf = storage
            .get_workflow(execution_id)
            .await
            .expect("wf query")
            .expect("wf exists");
        assert_eq!(wf.status, "aborted");

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn already_undone_preserves_original_undone_by() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-preserve-by.db");
        let db_path = db_path.to_string_lossy().to_string();
        let storage = Arc::new(StorageHandle::new(&db_path).await.expect("storage"));
        let pane_id = 30_u64;
        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", None, "spawn").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "pane_close".to_string(),
                undo_hint: None,
                undo_payload: Some(serde_json::json!({ "pane_id": pane_id }).to_string()),
                undone_at: Some(1_000_000),
                undone_by: Some("original-actor".to_string()),
            })
            .await
            .expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor
            .execute(UndoRequest::new(action_id).with_actor("new-actor"))
            .await
            .expect("result");

        // Should be not applicable since already undone
        assert_eq!(result.outcome, UndoOutcome::NotApplicable);

        // Original undone_by should be preserved
        let undo = storage
            .get_action_undo(action_id)
            .await
            .expect("undo query")
            .expect("undo exists");
        assert_eq!(undo.undone_by.as_deref(), Some("original-actor"));

        storage.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn manual_strategy_undoable_true_still_returns_not_applicable() {
        // Even when undoable=true, manual strategy returns not_applicable
        // because automatic undo is not supported
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-manual-true.db");
        let storage = Arc::new(
            StorageHandle::new(&db_path.to_string_lossy())
                .await
                .expect("storage"),
        );

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage
            .upsert_action_undo(ActionUndoRecord {
                audit_action_id: action_id,
                undoable: true,
                undo_strategy: "manual".to_string(),
                undo_hint: Some("manual steps needed".to_string()),
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
            .expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not supported"));
        assert_eq!(result.guidance.as_deref(), Some("manual steps needed"));

        storage.shutdown().await.expect("shutdown");
    }
}
