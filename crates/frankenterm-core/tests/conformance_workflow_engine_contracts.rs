#![cfg(feature = "asupersync-runtime")]

//! WorkflowEngine contract conformance harness.
//!
//! Coverage matrix:
//! - MUST: every public start entry point persists a running execution.
//! - MUST: status transitions preserve durable fields and terminal executions
//!   are not resumable.
//! - MUST: incomplete discovery returns only running/waiting executions.
//! - MUST: resume derives the next step from durable step logs, including
//!   jump targets and post-send progress.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use common::fixtures::RuntimeFixture;
use frankenterm_core::storage::{PaneRecord, StorageHandle, StoredEvent, now_ms};
use frankenterm_core::workflows::{ExecutionStatus, StepResult, WaitCondition, WorkflowEngine};
use serde_json::json;

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path(label: &str) -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "workflow_engine_contract_{label}_{counter}_{}.sqlite3",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

async fn storage_for(label: &str) -> StorageHandle {
    StorageHandle::new(&temp_db_path(label))
        .await
        .expect("create workflow conformance storage")
}

async fn seed_pane(storage: &StorageHandle, pane_id: u64) {
    let now = now_ms();
    storage
        .upsert_pane(PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: Some(1),
            tab_id: Some(1),
            title: Some(format!("workflow-contract-{pane_id}")),
            cwd: Some("/tmp/frankenterm-workflow-contract".to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .expect("seed pane referenced by workflow");
}

async fn seed_panes(storage: &StorageHandle, pane_ids: impl IntoIterator<Item = u64>) {
    for pane_id in pane_ids {
        seed_pane(storage, pane_id).await;
    }
}

async fn seed_event(storage: &StorageHandle, pane_id: u64, label: &str) -> i64 {
    storage
        .record_event(StoredEvent {
            id: 0,
            pane_id,
            rule_id: format!("contract.{label}"),
            agent_type: "codex".to_string(),
            event_type: "workflow_contract".to_string(),
            severity: "info".to_string(),
            confidence: 1.0,
            extracted: Some(json!({"contract": label})),
            matched_text: Some(format!("workflow contract event {label}")),
            segment_id: None,
            detected_at: now_ms(),
            dedupe_key: Some(format!("workflow-contract-{pane_id}-{label}")),
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        })
        .await
        .expect("seed event referenced by workflow")
}

#[test]
fn workflow_engine_start_entrypoints_persist_running_contract() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let storage = storage_for("start_entrypoints").await;
        seed_panes(&storage, [10, 11, 12, 13]).await;
        let legacy_event = seed_event(&storage, 10, "legacy").await;
        let legacy_with_id_event = seed_event(&storage, 11, "legacy-with-id").await;
        let cx_event = seed_event(&storage, 12, "cx").await;
        let cx_with_id_event = seed_event(&storage, 13, "cx-with-id").await;
        let engine = WorkflowEngine::new(7);
        let cx = frankenterm_core::cx::for_request();

        let legacy_start = engine
            .start(
                &storage,
                "contract_start",
                10,
                Some(legacy_event),
                Some(json!({"path": "legacy"})),
            )
            .await
            .expect("legacy start succeeds");
        let legacy_with_id = engine
            .start_with_id(
                &storage,
                "contract-explicit-legacy".to_string(),
                "contract_start_with_id",
                11,
                Some(legacy_with_id_event),
                Some(json!({"path": "legacy_with_id"})),
            )
            .await
            .expect("legacy start_with_id succeeds");
        let cx_start = engine
            .start_cx(
                &cx,
                &storage,
                "contract_start_cx",
                12,
                Some(cx_event),
                Some(json!({"path": "cx"})),
            )
            .await
            .expect("cx start succeeds");
        let cx_with_id = engine
            .start_with_id_cx(
                &cx,
                &storage,
                "contract-explicit-cx".to_string(),
                "contract_start_with_id_cx",
                13,
                Some(cx_with_id_event),
                Some(json!({"path": "cx_with_id"})),
            )
            .await
            .expect("cx start_with_id succeeds");

        let cases = [
            (
                &legacy_start,
                "contract_start",
                10,
                Some(legacy_event),
                "legacy",
            ),
            (
                &legacy_with_id,
                "contract_start_with_id",
                11,
                Some(legacy_with_id_event),
                "legacy_with_id",
            ),
            (&cx_start, "contract_start_cx", 12, Some(cx_event), "cx"),
            (
                &cx_with_id,
                "contract_start_with_id_cx",
                13,
                Some(cx_with_id_event),
                "cx_with_id",
            ),
        ];

        for (execution, workflow_name, pane_id, trigger_event_id, context_path) in cases {
            assert_eq!(execution.workflow_name, workflow_name);
            assert_eq!(execution.pane_id, pane_id);
            assert_eq!(execution.current_step, 0);
            assert_eq!(execution.status, ExecutionStatus::Running);
            assert!(
                execution.updated_at >= execution.started_at,
                "updated_at must not precede started_at for {}",
                execution.id
            );

            let record = storage
                .get_workflow(&execution.id)
                .await
                .expect("load persisted workflow")
                .expect("workflow record exists");
            assert_eq!(record.workflow_name, workflow_name);
            assert_eq!(record.pane_id, pane_id);
            assert_eq!(record.trigger_event_id, trigger_event_id);
            assert_eq!(record.current_step, 0);
            assert_eq!(record.status, "running");
            assert!(record.wait_condition.is_none());
            assert!(record.result.is_none());
            assert!(record.error.is_none());
            assert!(record.completed_at.is_none());
            assert_eq!(
                record.context.as_ref().and_then(|value| value.get("path")),
                Some(&json!(context_path))
            );
        }
    });
}

#[test]
fn workflow_engine_status_resume_and_incomplete_contracts() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let storage = storage_for("status_resume").await;
        seed_pane(&storage, 42).await;
        let engine = WorkflowEngine::default();

        for id in [
            "contract-running",
            "contract-waiting",
            "contract-completed",
            "contract-aborted",
        ] {
            engine
                .start_with_id(&storage, id.to_string(), "contract_status", 42, None, None)
                .await
                .expect("seed workflow");
        }

        engine
            .update_status(
                &storage,
                "contract-running",
                ExecutionStatus::Running,
                2,
                None,
                None,
            )
            .await
            .expect("running update succeeds");

        let wait_condition = WaitCondition::pattern("contract.rule");
        engine
            .update_status(
                &storage,
                "contract-waiting",
                ExecutionStatus::Waiting,
                3,
                Some(&wait_condition),
                None,
            )
            .await
            .expect("waiting update succeeds");

        engine
            .update_status(
                &storage,
                "contract-completed",
                ExecutionStatus::Completed,
                4,
                None,
                None,
            )
            .await
            .expect("completed update succeeds");

        engine
            .update_status(
                &storage,
                "contract-aborted",
                ExecutionStatus::Aborted,
                5,
                None,
                Some("operator canceled"),
            )
            .await
            .expect("aborted update succeeds");

        let running = storage
            .get_workflow("contract-running")
            .await
            .expect("load running")
            .expect("running record exists");
        assert_eq!(running.status, "running");
        assert_eq!(running.current_step, 2);
        assert!(running.wait_condition.is_none());
        assert!(running.completed_at.is_none());

        let waiting = storage
            .get_workflow("contract-waiting")
            .await
            .expect("load waiting")
            .expect("waiting record exists");
        assert_eq!(waiting.status, "waiting");
        assert_eq!(waiting.current_step, 3);
        assert_eq!(waiting.wait_condition, Some(json!(wait_condition)));
        assert!(waiting.completed_at.is_none());

        let completed = storage
            .get_workflow("contract-completed")
            .await
            .expect("load completed")
            .expect("completed record exists");
        assert_eq!(completed.status, "completed");
        assert!(completed.completed_at.is_some());

        let aborted = storage
            .get_workflow("contract-aborted")
            .await
            .expect("load aborted")
            .expect("aborted record exists");
        assert_eq!(aborted.status, "aborted");
        assert_eq!(aborted.error.as_deref(), Some("operator canceled"));
        assert!(aborted.completed_at.is_some());

        let incomplete_ids = engine
            .find_incomplete(&storage)
            .await
            .expect("find incomplete")
            .into_iter()
            .map(|record| record.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            incomplete_ids,
            BTreeSet::from([
                "contract-running".to_string(),
                "contract-waiting".to_string()
            ])
        );

        let (running_resume, running_next) = engine
            .resume(&storage, "contract-running")
            .await
            .expect("resume running")
            .expect("running workflow resumes");
        assert_eq!(running_resume.status, ExecutionStatus::Running);
        assert_eq!(running_next, 2);

        let (waiting_resume, waiting_next) = engine
            .resume(&storage, "contract-waiting")
            .await
            .expect("resume waiting")
            .expect("waiting workflow resumes");
        assert_eq!(waiting_resume.status, ExecutionStatus::Waiting);
        assert_eq!(waiting_next, 3);

        assert!(
            engine
                .resume(&storage, "contract-completed")
                .await
                .expect("resume completed")
                .is_none(),
            "completed workflows must be terminal"
        );
        assert!(
            engine
                .resume(&storage, "contract-aborted")
                .await
                .expect("resume aborted")
                .is_none(),
            "aborted workflows must be terminal"
        );

        let missing = engine
            .update_status(
                &storage,
                "contract-missing",
                ExecutionStatus::Running,
                0,
                None,
                None,
            )
            .await
            .expect_err("missing workflow update must fail");
        assert!(
            missing.to_string().contains("contract-missing"),
            "missing-workflow error should identify the execution id: {missing}"
        );
    });
}

#[test]
fn workflow_engine_step_log_resume_contract_matrix() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let storage = storage_for("step_log_resume").await;
        seed_pane(&storage, 77).await;
        let engine = WorkflowEngine::default();
        let cx = frankenterm_core::cx::for_request();

        let cases = [
            ("continue", StepResult::cont(), 1usize),
            ("done", StepResult::done(json!({"ok": true})), 1usize),
            ("retry", StepResult::retry(50), 0usize),
            (
                "wait_for",
                StepResult::wait_for_with_timeout(WaitCondition::pattern("contract.wait"), 500),
                0usize,
            ),
            ("abort", StepResult::abort("step failed"), 0usize),
            ("jump_to", StepResult::jump_to(4), 4usize),
        ];

        for (name, result, expected_next_step) in cases {
            let execution_id = format!("contract-log-{name}");
            engine
                .start_with_id_cx(
                    &cx,
                    &storage,
                    execution_id.clone(),
                    "contract_log",
                    77,
                    None,
                    None,
                )
                .await
                .expect("seed workflow");
            engine
                .log_step_cx(&cx, &storage, &execution_id, 0, name, &result, 1234)
                .await
                .expect("log workflow step");

            let logs = storage
                .get_step_logs(&execution_id)
                .await
                .expect("load step logs");
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].result_type, name);
            assert!(logs[0].result_data.is_some());

            let (_execution, next_step) = engine
                .resume_cx(&cx, &storage, &execution_id)
                .await
                .expect("resume logged workflow")
                .expect("logged workflow resumes");
            assert_eq!(
                next_step, expected_next_step,
                "{name} must resume at its contract step"
            );
        }

        engine
            .start_with_id(
                &storage,
                "contract-log-send-text".to_string(),
                "contract_log",
                77,
                None,
                None,
            )
            .await
            .expect("seed send_text workflow");
        let send_wait = StepResult::send_text_and_wait(
            "echo done",
            WaitCondition::pattern("contract.sent"),
            1_000,
        );
        engine
            .log_step(
                &storage,
                "contract-log-send-text",
                2,
                "send_text",
                &send_wait,
                5678,
            )
            .await
            .expect("log send_text step");
        engine
            .update_status(
                &storage,
                "contract-log-send-text",
                ExecutionStatus::Running,
                3,
                None,
                None,
            )
            .await
            .expect("persist post-send progress");

        let send_logs = storage
            .get_step_logs("contract-log-send-text")
            .await
            .expect("load send_text logs");
        assert_eq!(send_logs.len(), 1);
        assert_eq!(send_logs[0].result_type, "send_text");
        assert!(
            send_logs[0].verification_refs.is_some(),
            "send_text with a verification wait must persist verification refs"
        );

        let (_execution, next_step) = engine
            .resume(&storage, "contract-log-send-text")
            .await
            .expect("resume send_text workflow")
            .expect("send_text workflow resumes");
        assert_eq!(
            next_step, 3,
            "post-send progress must not replay a completed send_text side effect"
        );
    });
}
