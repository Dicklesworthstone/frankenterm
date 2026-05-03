#![cfg(feature = "asupersync-runtime")]

//! Property coverage for workflow handler execution through `WorkflowRunner`.
//!
//! Properties:
//! - retry outcomes are bounded by `max_retries_per_step`;
//! - cancellation during retry backoff aborts promptly and persists failure;
//! - the overall workflow deadline fails overdue executions durably.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::fixtures::RuntimeFixture;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::policy::{PolicyEngine, PolicyGatedInjector};
use frankenterm_core::storage::{PaneRecord, StorageHandle, WorkflowStepLogRecord, now_ms};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};
use frankenterm_core::workflows::{
    BoxFuture, CxPolicyInjector, PaneWorkflowLockManager, StepResult, Workflow, WorkflowContext,
    WorkflowEngine, WorkflowExecutionResult, WorkflowRunner, WorkflowRunnerConfig, WorkflowStep,
};
use proptest::prelude::*;

const PANE_ID: u64 = 6104;
const RETRY_RULE: &str = "property.workflow.retry";
const CANCEL_RULE: &str = "property.workflow.cancel";
const DEADLINE_RULE: &str = "property.workflow.deadline";

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path(label: &str) -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "workflow_handler_property_{label}_{counter}_{}.sqlite3",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

fn detection(rule_id: &str) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type: AgentType::Codex,
        event_type: "workflow.property".to_string(),
        severity: Severity::Info,
        confidence: 1.0,
        extracted: serde_json::json!({}),
        matched_text: "workflow property trigger".to_string(),
        span: (0, 25),
    }
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
            title: Some("workflow-property".to_string()),
            cwd: Some("/tmp/frankenterm-workflow-property".to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .expect("seed workflow property pane");
}

async fn build_runner(
    label: &str,
    config: WorkflowRunnerConfig,
) -> (
    WorkflowRunner,
    Arc<StorageHandle>,
    Arc<PaneWorkflowLockManager>,
) {
    let storage = Arc::new(
        StorageHandle::new(&temp_db_path(label))
            .await
            .expect("create workflow property storage"),
    );
    seed_pane(&storage, PANE_ID).await;

    let mock = MockWezterm::new();
    mock.add_default_pane(PANE_ID).await;
    let handle: WeztermHandle = Arc::new(mock);
    let injector =
        CxPolicyInjector::new(PolicyGatedInjector::new(PolicyEngine::permissive(), handle));
    let lock_manager = Arc::new(PaneWorkflowLockManager::new());
    let runner = WorkflowRunner::new(
        WorkflowEngine::default(),
        Arc::clone(&lock_manager),
        Arc::clone(&storage),
        injector,
        config,
    );

    (runner, storage, lock_manager)
}

async fn start_and_run(
    runner: &WorkflowRunner,
    workflow: Arc<dyn Workflow>,
    rule_id: &str,
) -> (String, WorkflowExecutionResult) {
    runner.register_workflow(Arc::clone(&workflow));
    let start = runner
        .handle_detection(PANE_ID, &detection(rule_id), None)
        .await;
    let execution_id = start
        .execution_id()
        .unwrap_or_else(|| panic!("workflow did not start: {start:?}"))
        .to_string();
    let result = runner
        .run_workflow(PANE_ID, workflow, &execution_id, 0)
        .await;
    (execution_id, result)
}

fn assert_retry_step_log_contract(
    logs: &[WorkflowStepLogRecord],
    expected_retry_logs: usize,
    terminal_result: Option<&str>,
) -> Result<(), proptest::test_runner::TestCaseError> {
    prop_assert_eq!(
        logs.len(),
        expected_retry_logs + usize::from(terminal_result.is_some()),
        "retry handler should persist only bounded retry logs plus terminal step"
    );

    for (index, log) in logs.iter().enumerate() {
        prop_assert_eq!(
            log.step_index,
            0,
            "retry handler must keep re-executing the same step"
        );
        prop_assert_eq!(log.step_name.as_str(), "retry_or_done");
        let expected_result = if index < expected_retry_logs {
            "retry"
        } else {
            terminal_result.expect("terminal log exists when index exceeds retry logs")
        };
        prop_assert_eq!(log.result_type.as_str(), expected_result);
    }

    Ok(())
}

struct RetryThenDoneWorkflow {
    retry_results_before_done: usize,
    retry_delay_ms: u64,
    attempts: Arc<AtomicUsize>,
}

impl RetryThenDoneWorkflow {
    fn new(retry_results_before_done: usize, retry_delay_ms: u64) -> Self {
        Self {
            retry_results_before_done,
            retry_delay_ms,
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempts(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.attempts)
    }
}

impl Workflow for RetryThenDoneWorkflow {
    fn name(&self) -> &'static str {
        "property_retry_then_done"
    }

    fn description(&self) -> &'static str {
        "Property workflow that emits retry results before completing"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == RETRY_RULE || detection.rule_id == CANCEL_RULE
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![WorkflowStep::new(
            "retry_or_done",
            "Retry until budget allows completion",
        )]
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        _step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let attempts = Arc::clone(&self.attempts);
        let retry_results_before_done = self.retry_results_before_done;
        let retry_delay_ms = self.retry_delay_ms;
        Box::pin(async move {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            if attempt < retry_results_before_done {
                StepResult::retry(retry_delay_ms)
            } else {
                StepResult::done(serde_json::json!({
                    "attempt": attempt,
                    "retries_before_done": retry_results_before_done,
                }))
            }
        })
    }
}

struct DeadlineOverrunWorkflow {
    sleep_ms: u64,
    attempts: Arc<AtomicUsize>,
}

impl DeadlineOverrunWorkflow {
    fn new(sleep_ms: u64) -> Self {
        Self {
            sleep_ms,
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempts(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.attempts)
    }
}

impl Workflow for DeadlineOverrunWorkflow {
    fn name(&self) -> &'static str {
        "property_deadline_overrun"
    }

    fn description(&self) -> &'static str {
        "Property workflow that overruns the total workflow deadline"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == DEADLINE_RULE
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("overrun", "Spend longer than the total deadline"),
            WorkflowStep::new(
                "should_not_complete",
                "Runner should fail before this completes",
            ),
        ]
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        _step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let attempts = Arc::clone(&self.attempts);
        let sleep_ms = self.sleep_ms;
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(sleep_ms));
            StepResult::cont()
        })
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 12,
        .. ProptestConfig::default()
    })]

    #[test]
    fn retry_budget_property(max_retries in 0usize..5, retry_results_before_done in 0usize..8) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let config = WorkflowRunnerConfig {
                max_retries_per_step: max_retries,
                workflow_total_deadline_ms: 0,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("retry_budget", config).await;
            let workflow = Arc::new(RetryThenDoneWorkflow::new(retry_results_before_done, 0));
            let attempts = workflow.attempts();
            let (execution_id, result) = start_and_run(&runner, workflow, RETRY_RULE).await;

            if retry_results_before_done <= max_retries {
                prop_assert!(
                    matches!(result, WorkflowExecutionResult::Completed { .. }),
                    "retry count within budget should complete; got {result:?}"
                );
                prop_assert_eq!(attempts.load(Ordering::SeqCst), retry_results_before_done + 1);
                let record = storage
                    .get_workflow(&execution_id)
                    .await
                    .expect("load completed workflow")
                    .expect("completed workflow exists");
                prop_assert_eq!(record.status.as_str(), "completed");
                prop_assert!(record.completed_at.is_some());
                let logs = storage
                    .get_step_logs(&execution_id)
                    .await
                    .expect("load completed retry logs");
                assert_retry_step_log_contract(&logs, retry_results_before_done, Some("done"))?;
            } else {
                prop_assert!(
                    matches!(result, WorkflowExecutionResult::Aborted { ref reason, .. } if reason.contains("Max retries")),
                    "retry count above budget should abort on max retries; got {result:?}"
                );
                prop_assert_eq!(attempts.load(Ordering::SeqCst), max_retries + 1);
                let record = storage
                    .get_workflow(&execution_id)
                    .await
                    .expect("load aborted workflow")
                    .expect("aborted workflow exists");
                prop_assert_eq!(record.status.as_str(), "failed");
                prop_assert!(
                    record.error.as_deref().is_some_and(|error| error.contains("Max retries")),
                    "max-retry failure reason should persist: {:?}",
                    record.error
                );
                let logs = storage
                    .get_step_logs(&execution_id)
                    .await
                    .expect("load max-retry logs");
                assert_retry_step_log_contract(&logs, max_retries + 1, None)?;
            }

            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after retry terminal result"
            );
            Ok(())
        })?;
    }

    #[test]
    fn cancel_during_retry_backoff_property(cancel_after_ms in 1u64..35, retry_delay_ms in 250u64..2_000) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let config = WorkflowRunnerConfig {
                max_retries_per_step: 100,
                workflow_total_deadline_ms: 0,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("retry_cancel", config).await;
            let workflow = Arc::new(RetryThenDoneWorkflow::new(100, retry_delay_ms));
            let attempts = workflow.attempts();
            runner.register_workflow(workflow.clone());

            let start = runner
                .handle_detection(PANE_ID, &detection(CANCEL_RULE), None)
                .await;
            let execution_id = start
                .execution_id()
                .unwrap_or_else(|| panic!("workflow did not start: {start:?}"))
                .to_string();
            let cx = frankenterm_core::cx::for_testing();
            let cancel_cx = cx.clone();
            let cancel_thread = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(cancel_after_ms));
                cancel_cx.cancel_with(
                    frankenterm_core::outcome::CancelKind::User,
                    Some("workflow handler property cancellation"),
                );
            });

            let started_at = Instant::now();
            let result = runner
                .run_workflow_with_cx(&cx, PANE_ID, workflow, &execution_id, 0)
                .await;
            cancel_thread.join().expect("cancel thread should not panic");
            let elapsed = started_at.elapsed();

            let cancel_message = match &result {
                WorkflowExecutionResult::Aborted { reason, .. } => reason.as_str(),
                WorkflowExecutionResult::Error { error, .. } => error.as_str(),
                other => {
                    prop_assert!(
                        false,
                        "cancellation should surface as a terminal workflow failure; got {other:?}"
                    );
                    unreachable!("prop_assert above always returns")
                }
            };
            prop_assert!(
                cancel_message.contains("cancelled"),
                "terminal cancellation should explain cancellation; got {result:?}"
            );
            prop_assert!(
                attempts.load(Ordering::SeqCst) <= 1,
                "cancellation must not execute another attempt after the first retry boundary"
            );
            prop_assert!(
                elapsed < Duration::from_millis(retry_delay_ms),
                "cancelled retry backoff should finish before the requested delay; elapsed={elapsed:?}, retry_delay_ms={retry_delay_ms}"
            );
            prop_assert!(
                elapsed < Duration::from_secs(1),
                "cancelled retry backoff should be prompt; elapsed={elapsed:?}"
            );

            let record = storage
                .get_workflow(&execution_id)
                .await
                .expect("load cancelled workflow")
                .expect("cancelled workflow exists");
            prop_assert_eq!(record.status.as_str(), "failed");
            prop_assert!(
                record
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("cancelled")),
                "cancel reason should persist: {:?}",
                record.error
            );
            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after retry cancellation"
            );
            Ok(())
        })?;
    }

    #[test]
    fn overall_deadline_property(deadline_ms in 1u64..20, overrun_ms in 1u64..30) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let config = WorkflowRunnerConfig {
                workflow_total_deadline_ms: deadline_ms,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("overall_deadline", config).await;
            let workflow = Arc::new(DeadlineOverrunWorkflow::new(deadline_ms + overrun_ms));
            let attempts = workflow.attempts();
            let (execution_id, result) = start_and_run(&runner, workflow, DEADLINE_RULE).await;

            prop_assert!(
                matches!(result, WorkflowExecutionResult::Error { ref error, .. } if error.contains("workflow_total_deadline_ms")),
                "overall deadline overrun should fail execution; got {result:?}"
            );
            prop_assert!(
                attempts.load(Ordering::SeqCst) <= 1,
                "deadline should be enforced before any post-deadline step executes"
            );
            let record = storage
                .get_workflow(&execution_id)
                .await
                .expect("load deadline workflow")
                .expect("deadline workflow exists");
            prop_assert_eq!(record.status.as_str(), "failed");
            prop_assert!(
                record
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("workflow_total_deadline_ms")),
                "deadline reason should persist: {:?}",
                record.error
            );
            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after deadline failure"
            );
            Ok(())
        })?;
    }
}
