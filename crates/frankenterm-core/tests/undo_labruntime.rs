//! LabRuntime-ported undo tests for deterministic async testing.
//!
//! Ports all 27 `#[tokio::test]` functions from `undo.rs` to asupersync-based
//! `RuntimeFixture`. The undo module uses `StorageHandle` (sqlite) and
//! `MockWezterm` — both work under RuntimeFixture as they use compatible
//! async primitives.
//!
//! Bead: ft-22x4r (Port existing async tests to LabRuntime)

#![cfg(feature = "asupersync-runtime")]

mod common;

use common::fixtures::RuntimeFixture;
use frankenterm_core::storage::{
    ActionUndoRecord, AuditActionRecord, PaneRecord, StorageHandle, WorkflowRecord, now_ms,
};
use frankenterm_core::undo::{UndoExecutor, UndoOutcome, UndoRequest};
use frankenterm_core::wezterm::MockWezterm;
use std::sync::Arc;

// ===========================================================================
// Helpers (mirroring undo.rs internal test helpers)
// ===========================================================================

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

async fn seed_workflow(storage: &StorageHandle, execution_id: &str, pane_id: u64, status: &str) {
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

fn make_undo(
    action_id: i64,
    strategy: &str,
    undoable: bool,
    hint: Option<&str>,
    payload: Option<&str>,
) -> ActionUndoRecord {
    ActionUndoRecord {
        audit_action_id: action_id,
        undoable,
        undo_strategy: strategy.to_string(),
        undo_hint: hint.map(str::to_string),
        undo_payload: payload.map(str::to_string),
        undone_at: None,
        undone_by: None,
    }
}

// ===========================================================================
// Section 1: Undo tests ported from tokio::test to RuntimeFixture
// ===========================================================================

#[test]
fn undo_workflow_abort_succeeds_and_marks_action_undone() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-success.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 42_u64;
        let execution_id = "wf-undo-success-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "workflow", Some(execution_id), "workflow_start").await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "running").await;

        storage.upsert_action_undo(make_undo(
            action_id, "workflow_abort", true,
            Some(&format!("ft robot workflow abort {execution_id}")),
            Some(&serde_json::json!({ "execution_id": execution_id, "pane_id": pane_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id).with_actor("test-user")).await.expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.strategy, "workflow_abort");
        assert_eq!(result.target_workflow_id.as_deref(), Some(execution_id));

        let workflow = storage.get_workflow(execution_id).await.expect("wf query").expect("wf exists");
        assert_eq!(workflow.status, "aborted");

        let undo = storage.get_action_undo(action_id).await.expect("undo query").expect("undo exists");
        assert!(undo.undone_at.is_some());
        assert_eq!(undo.undone_by.as_deref(), Some("test-user"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_workflow_abort_not_applicable_when_completed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-completed.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 7_u64;
        let execution_id = "wf-undo-completed-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "workflow", Some(execution_id), "workflow_start").await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "completed").await;

        storage.upsert_action_undo(make_undo(
            action_id, "workflow_abort", true,
            Some(&format!("ft robot workflow abort {execution_id}")),
            Some(&serde_json::json!({ "execution_id": execution_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("already_completed"));

        let undo = storage.get_action_undo(action_id).await.expect("undo query").expect("undo exists");
        assert!(undo.undone_at.is_none());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_manual_strategy_returns_guidance() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-manual-guidance.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 11).await;
        let action_id = seed_action(storage.as_ref(), 11, "human", Some("cli"), "send_text").await;

        storage.upsert_action_undo(make_undo(
            action_id, "manual", false,
            Some("Inspect pane state and reverse command manually."), None,
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert_eq!(result.guidance.as_deref(), Some("Inspect pane state and reverse command manually."));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_already_undone_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-already-undone.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 21).await;
        let action_id = seed_action(storage.as_ref(), 21, "human", Some("cli"), "spawn").await;

        let mut undo_rec = make_undo(action_id, "pane_close", true, Some("Pane was already closed."), Some(&serde_json::json!({ "pane_id": 21 }).to_string()));
        undo_rec.undone_at = Some(now_ms() - 1_000);
        undo_rec.undone_by = Some("first-operator".to_string());
        storage.upsert_action_undo(undo_rec).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id).with_actor("second-operator")).await.expect("undo result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("already been undone"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_action_not_found_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-not-found.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(99999)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not found"));
        assert!(result.guidance.is_some());

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_no_metadata_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-no-metadata.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", Some("cli"), "send_text").await;

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("No undo metadata"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_not_undoable_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-not-undoable.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage.upsert_action_undo(make_undo(
            action_id, "none", false, Some("Cannot undo text"), None,
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not currently undoable"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_unknown_strategy_returns_failed() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-unknown-strategy.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage.upsert_action_undo(make_undo(action_id, "teleport", true, None, None)).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Failed);
        assert!(result.message.contains("Unknown undo strategy"));
        assert_eq!(result.strategy, "teleport");

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_custom_strategy_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-custom-strategy.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage.upsert_action_undo(make_undo(
            action_id, "custom", true, Some("Use external tool"), None,
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not supported"));
        assert_eq!(result.guidance.as_deref(), Some("Use external tool"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_pane_close_nonexistent_pane_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-gone.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "spawn").await;

        storage.upsert_action_undo(make_undo(
            action_id, "pane_close", true, None, Some(r#"{"pane_id": 999}"#),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_pane_close_no_pane_id_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-no-pane-id.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "spawn").await;

        storage.upsert_action_undo(make_undo(
            action_id, "pane_close", true, None, Some(r#"{"reason": "test"}"#),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        // No pane added to mock — and action has pane_id=1 but no default pane in mock
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        // Falls back to action's pane_id, but mock has no pane 1 → not applicable
        assert_eq!(result.outcome, UndoOutcome::NotApplicable);

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_workflow_abort_no_execution_id_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-no-exec-id.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "workflow", None, "workflow_start").await;

        storage.upsert_action_undo(make_undo(
            action_id, "workflow_abort", true, None, Some(r#"{"no_exec_id": true}"#),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_pane_close_closes_existing_pane() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-close.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 55_u64;

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;

        storage.upsert_action_undo(make_undo(
            action_id, "pane_close", true, None,
            Some(&serde_json::json!({ "pane_id": pane_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(pane_id).await;
        let executor = UndoExecutor::new(Arc::clone(&storage), mock.clone());
        let result = executor.execute(UndoRequest::new(action_id).with_actor("test")).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.target_pane_id, Some(pane_id));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_none_strategy_returns_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-none-strategy.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage.upsert_action_undo(make_undo(
            action_id, "none", true, Some("No undo available"), None,
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not supported"));
        assert_eq!(result.guidance.as_deref(), Some("No undo available"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_executor_clone() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("clone-test.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let _cloned = executor.clone();
        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_pane_close_falls_back_to_action_pane_id() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-fallback.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 77_u64;

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;

        // Payload has no pane_id key
        storage.upsert_action_undo(make_undo(
            action_id, "pane_close", true, None, Some(r#"{"reason": "test"}"#),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(pane_id).await;
        let executor = UndoExecutor::new(Arc::clone(&storage), mock.clone());
        let result = executor.execute(UndoRequest::new(action_id).with_actor("fallback-test")).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.target_pane_id, Some(pane_id));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_workflow_abort_waiting_succeeds() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-waiting.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 88_u64;
        let execution_id = "wf-waiting-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "workflow", Some(execution_id), "workflow_start").await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "waiting").await;

        storage.upsert_action_undo(make_undo(
            action_id, "workflow_abort", true, None,
            Some(&serde_json::json!({ "execution_id": execution_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id).with_actor("test")).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert_eq!(result.strategy, "workflow_abort");

        let wf = storage.get_workflow(execution_id).await.expect("wf query").expect("wf exists");
        assert_eq!(wf.status, "aborted");

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_undoable_false_hint_falls_back() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-fallback-hint.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
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

        // Undo record with no hint, undoable=false
        storage.upsert_action_undo(make_undo(action_id, "manual", false, None, None)).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not currently undoable"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_action_not_found_guidance_suggests_ft_history() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-guidance-msg.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(12345)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert_eq!(result.strategy, "none");
        assert!(result.guidance.as_deref().unwrap().contains("ft history"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_action_not_found_message_includes_action_id() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-notfound-id.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(54321)).await.expect("result");

        assert!(result.message.contains("54321"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_no_metadata_guidance_mentions_non_undoable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-no-meta-guide.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        let guidance = result.guidance.as_deref().unwrap();
        assert!(guidance.contains("non-undoable") || guidance.contains("predates"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_unknown_strategy_returns_hint_from_record() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-unknown-hint.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage.upsert_action_undo(make_undo(
            action_id, "quantum_revert", true, Some("Contact quantum support"), None,
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Failed);
        assert_eq!(result.strategy, "quantum_revert");
        assert_eq!(result.guidance.as_deref(), Some("Contact quantum support"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_pane_close_success_message_contains_pane_id() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-pane-msg.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 123_u64;

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", Some("cli"), "spawn").await;

        storage.upsert_action_undo(make_undo(
            action_id, "pane_close", true, None,
            Some(&serde_json::json!({ "pane_id": pane_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        mock.add_default_pane(pane_id).await;
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert!(result.message.contains("123"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_workflow_abort_success_message_contains_execution_id() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-wf-msg.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 50_u64;
        let execution_id = "wf-msg-test-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "workflow", Some(execution_id), "workflow_start").await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "running").await;

        storage.upsert_action_undo(make_undo(
            action_id, "workflow_abort", true, None,
            Some(&serde_json::json!({ "execution_id": execution_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        assert!(result.message.contains(execution_id));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_request_with_reason_propagates() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-reason-prop.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 60_u64;
        let execution_id = "wf-reason-prop-1";

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "workflow", Some(execution_id), "workflow_start").await;
        seed_workflow(storage.as_ref(), execution_id, pane_id, "running").await;

        storage.upsert_action_undo(make_undo(
            action_id, "workflow_abort", true, None,
            Some(&serde_json::json!({ "execution_id": execution_id }).to_string()),
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(
            UndoRequest::new(action_id).with_actor("reason-test").with_reason("emergency rollback"),
        ).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::Success);
        let wf = storage.get_workflow(execution_id).await.expect("wf query").expect("wf exists");
        assert_eq!(wf.status, "aborted");

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_already_undone_preserves_original_undone_by() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-preserve-by.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));
        let pane_id = 30_u64;

        seed_pane(storage.as_ref(), pane_id).await;
        let action_id = seed_action(storage.as_ref(), pane_id, "human", None, "spawn").await;

        let mut undo_rec = make_undo(action_id, "pane_close", true, None, Some(&serde_json::json!({ "pane_id": pane_id }).to_string()));
        undo_rec.undone_at = Some(1_000_000);
        undo_rec.undone_by = Some("original-actor".to_string());
        storage.upsert_action_undo(undo_rec).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id).with_actor("new-actor")).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);

        let undo = storage.get_action_undo(action_id).await.expect("undo query").expect("undo exists");
        assert_eq!(undo.undone_by.as_deref(), Some("original-actor"));

        storage.shutdown().await.expect("shutdown");
    });
}

#[test]
fn undo_manual_strategy_undoable_true_still_not_applicable() {
    let rt = RuntimeFixture::current_thread();
    rt.block_on(async {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let db_path = temp.path().join("undo-manual-true.db");
        let storage = Arc::new(StorageHandle::new(&db_path.to_string_lossy()).await.expect("storage"));

        seed_pane(storage.as_ref(), 1).await;
        let action_id = seed_action(storage.as_ref(), 1, "human", None, "send_text").await;

        storage.upsert_action_undo(make_undo(
            action_id, "manual", true, Some("manual steps needed"), None,
        )).await.expect("undo metadata");

        let mock = Arc::new(MockWezterm::new());
        let executor = UndoExecutor::new(Arc::clone(&storage), mock);
        let result = executor.execute(UndoRequest::new(action_id)).await.expect("result");

        assert_eq!(result.outcome, UndoOutcome::NotApplicable);
        assert!(result.message.contains("not supported"));
        assert_eq!(result.guidance.as_deref(), Some("manual steps needed"));

        storage.shutdown().await.expect("shutdown");
    });
}

// ===========================================================================
// Note: LabRuntime sections (2-5) omitted for undo tests.
//
// The undo module uses `StorageHandle` (sqlite via tokio::task::spawn_blocking)
// which requires a full runtime with blocking thread pool. The RuntimeFixture
// provides this capability. The LabRuntime's deterministic scheduler does not
// support spawn_blocking, so undo operations would hang waiting for blocking
// I/O to complete.
// ===========================================================================
