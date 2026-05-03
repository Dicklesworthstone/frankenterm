#![cfg(feature = "asupersync-runtime")]

//! Property coverage for workflow handler execution through `WorkflowRunner`.
//!
//! Properties:
//! - concurrent starts are capped by `max_concurrent` and release frees capacity;
//! - saturated start bursts are rejected without queuing phantom workflow records;
//! - retry outcomes are bounded by `max_retries_per_step`;
//! - pre-cancelled retry-capable handlers abort before executing any step;
//! - cancellation during retry backoff aborts promptly, persists failure, and
//!   does not emit duplicate retry or terminal step logs;
//! - cancellation during awaited wait conditions fails the workflow durably
//!   instead of leaving the execution stuck in waiting;
//! - explicit wait timeouts cap handler waits before the overall deadline trips;
//! - the overall workflow deadline fails overdue executions durably.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::fixtures::RuntimeFixture;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::policy::{PolicyEngine, PolicyGatedInjector};
use frankenterm_core::storage::{
    ExportQuery, PaneRecord, StorageHandle, WorkflowStepLogRecord, now_ms,
};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};
use frankenterm_core::workflows::{
    BoxFuture, CxPolicyInjector, PaneWorkflowLockManager, StepResult, Workflow, WorkflowContext,
    WorkflowEngine, WorkflowExecutionResult, WorkflowRunner, WorkflowRunnerConfig,
    WorkflowStartResult, WorkflowStep,
};
use proptest::prelude::*;

const PANE_ID: u64 = 6104;
const RETRY_RULE: &str = "property.workflow.retry";
const CANCEL_RULE: &str = "property.workflow.cancel";
const DEADLINE_RULE: &str = "property.workflow.deadline";
const WAIT_CANCEL_RULE: &str = "property.workflow.wait_cancel";
const CONCURRENCY_RULE: &str = "property.workflow.concurrency";
const WAIT_TIMEOUT_RULE: &str = "property.workflow.wait_timeout";

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
        detection.rule_id == RETRY_RULE
            || detection.rule_id == CANCEL_RULE
            || detection.rule_id == CONCURRENCY_RULE
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

struct AwaitingWaitWorkflow {
    wait_ms: u64,
    cancel_after_ms: u64,
    attempts: Arc<AtomicUsize>,
}

impl AwaitingWaitWorkflow {
    fn new(wait_ms: u64, cancel_after_ms: u64) -> Self {
        Self {
            wait_ms,
            cancel_after_ms,
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempts(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.attempts)
    }
}

struct TimedWaitThenDoneWorkflow {
    wait_ms: u64,
    timeout_ms: u64,
    attempts: Arc<AtomicUsize>,
}

impl TimedWaitThenDoneWorkflow {
    fn new(wait_ms: u64, timeout_ms: u64) -> Self {
        Self {
            wait_ms,
            timeout_ms,
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempts(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.attempts)
    }
}

impl Workflow for TimedWaitThenDoneWorkflow {
    fn name(&self) -> &'static str {
        "property_timed_wait_then_done"
    }

    fn description(&self) -> &'static str {
        "Property workflow that verifies wait timeout and deadline interaction"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == WAIT_TIMEOUT_RULE
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("bounded_wait", "Wait longer than the configured timeout"),
            WorkflowStep::new("after_timeout", "Complete after bounded wait timeout"),
        ]
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let attempts = Arc::clone(&self.attempts);
        let wait_ms = self.wait_ms;
        let timeout_ms = self.timeout_ms;
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            match step_idx {
                0 => StepResult::wait_for_with_timeout(
                    frankenterm_core::workflows::WaitCondition::sleep(wait_ms),
                    timeout_ms,
                ),
                _ => StepResult::done(serde_json::json!({
                    "completed_after_wait_timeout": true,
                    "wait_ms": wait_ms,
                    "timeout_ms": timeout_ms,
                })),
            }
        })
    }
}

impl Workflow for AwaitingWaitWorkflow {
    fn name(&self) -> &'static str {
        "property_awaiting_wait"
    }

    fn description(&self) -> &'static str {
        "Property workflow that awaits a cancelable wait condition"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == WAIT_CANCEL_RULE
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("await_cancelable_wait", "Wait long enough to be cancelled"),
            WorkflowStep::new("must_not_run_after_cancel", "Must not execute after cancel"),
        ]
    }

    fn execute_step(
        &self,
        _ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        self.execute_step_inner(None, step_idx)
    }

    fn execute_step_cx<'a>(
        &'a self,
        cx: &'a frankenterm_core::cx::Cx,
        _ctx: &'a mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'a, StepResult> {
        self.execute_step_inner(Some(cx.clone()), step_idx)
    }
}

impl AwaitingWaitWorkflow {
    fn execute_step_inner(
        &self,
        cancel_cx: Option<frankenterm_core::cx::Cx>,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let attempts = Arc::clone(&self.attempts);
        let wait_ms = self.wait_ms;
        let cancel_after_ms = self.cancel_after_ms;
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            match step_idx {
                0 => {
                    if let Some(cancel_cx) = cancel_cx {
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(cancel_after_ms));
                            cancel_cx.cancel_with(
                                frankenterm_core::outcome::CancelKind::User,
                                Some("workflow wait handler property cancellation"),
                            );
                        });
                    }
                    StepResult::wait_for_with_timeout(
                        frankenterm_core::workflows::WaitCondition::sleep(wait_ms),
                        wait_ms,
                    )
                }
                _ => StepResult::done(serde_json::json!({"unexpected_step": step_idx})),
            }
        })
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 12,
        .. ProptestConfig::default()
    })]

    #[test]
    fn concurrent_start_limit_and_release_property(
        max_concurrent in 1usize..5,
        rejected_attempts in 1usize..5,
        drain_cycles in 1usize..4,
    ) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let config = WorkflowRunnerConfig {
                max_concurrent,
                workflow_total_deadline_ms: 0,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("concurrency_limit", config).await;
            let workflow = Arc::new(RetryThenDoneWorkflow::new(0, 0));
            runner.register_workflow(workflow.clone());

            let mut started = Vec::new();
            for index in 0..max_concurrent {
                let pane_id = PANE_ID + 100 + u64::try_from(index).expect("index fits u64");
                seed_pane(&storage, pane_id).await;
                let start = runner
                    .handle_detection(pane_id, &detection(CONCURRENCY_RULE), None)
                    .await;
                let WorkflowStartResult::Started { execution_id, .. } = start else {
                    prop_assert!(
                        false,
                        "start {index} should acquire a concurrency slot; got {start:?}"
                    );
                    unreachable!("prop_assert above returns")
                };
                started.push((pane_id, execution_id));
            }

            prop_assert_eq!(
                lock_manager.active_locks().len(),
                max_concurrent,
                "successful starts should hold exactly max_concurrent active locks"
            );

            let initial_records = storage
                .export_workflows(ExportQuery::default())
                .await
                .expect("export initially started workflow records");
            prop_assert_eq!(
                initial_records.len(),
                max_concurrent,
                "initial accepted starts should be the only persisted workflow records"
            );

            let mut rejected_total = 0usize;
            for rejected_index in 0..rejected_attempts {
                let pane_id = PANE_ID
                    + 1_000
                    + u64::try_from(rejected_index).expect("rejected index fits u64");
                seed_pane(&storage, pane_id).await;
                let rejected = runner
                    .handle_detection(pane_id, &detection(CONCURRENCY_RULE), None)
                    .await;
                prop_assert!(
                    matches!(
                        rejected,
                        WorkflowStartResult::ConcurrencyLimitReached { active, limit }
                            if active == max_concurrent && limit == max_concurrent
                    ),
                    "start beyond max_concurrent should report active={max_concurrent}, limit={max_concurrent}; got {rejected:?}"
                );
                rejected_total += 1;
            }

            let health_after_rejects = lock_manager.health();
            prop_assert!(
                health_after_rejects.concurrency_limit_blocks_total
                    >= u64::try_from(rejected_total).expect("rejected attempts fits u64"),
                "concurrency-limit rejections should increment health telemetry: {health_after_rejects:?}"
            );
            prop_assert_eq!(
                lock_manager.active_locks().len(),
                max_concurrent,
                "saturated rejected bursts must not change active lock count"
            );
            let records_after_rejects = storage
                .export_workflows(ExportQuery::default())
                .await
                .expect("export workflow records after saturated rejections");
            prop_assert_eq!(
                records_after_rejects.len(),
                max_concurrent,
                "saturated rejected bursts must not persist queued workflow records"
            );

            let mut expected_records = max_concurrent;
            for cycle in 0..drain_cycles {
                let (released_pane_id, released_execution_id) = started.remove(0);
                let result = runner
                    .run_workflow(
                        released_pane_id,
                        workflow.clone(),
                        &released_execution_id,
                        0,
                    )
                    .await;
                prop_assert!(
                    matches!(result, WorkflowExecutionResult::Completed { .. }),
                    "completed workflow should release a concurrency slot; got {result:?}"
                );
                prop_assert_eq!(
                    lock_manager.active_locks().len(),
                    max_concurrent - 1,
                    "completed workflow should release exactly one held concurrency slot"
                );

                let replacement_pane_id =
                    PANE_ID + 2_000 + u64::try_from(cycle).expect("cycle fits u64");
                seed_pane(&storage, replacement_pane_id).await;
                let replacement = runner
                    .handle_detection(replacement_pane_id, &detection(CONCURRENCY_RULE), None)
                    .await;
                let WorkflowStartResult::Started { execution_id, .. } = replacement else {
                    prop_assert!(
                        false,
                        "after one workflow completes, a new start should acquire the freed slot; got {replacement:?}"
                    );
                    unreachable!("prop_assert above returns")
                };
                started.push((replacement_pane_id, execution_id));
                expected_records += 1;
                prop_assert_eq!(
                    lock_manager.active_locks().len(),
                    max_concurrent,
                    "replacement start should refill capacity without oversubscribing"
                );

                for rejected_index in 0..rejected_attempts {
                    let pane_id = PANE_ID
                        + 3_000
                        + (u64::try_from(cycle).expect("cycle fits u64") * 100)
                        + u64::try_from(rejected_index).expect("rejected index fits u64");
                    seed_pane(&storage, pane_id).await;
                    let rejected = runner
                        .handle_detection(pane_id, &detection(CONCURRENCY_RULE), None)
                        .await;
                    prop_assert!(
                        matches!(
                            rejected,
                            WorkflowStartResult::ConcurrencyLimitReached { active, limit }
                                if active == max_concurrent && limit == max_concurrent
                        ),
                        "post-refill saturated start should be rejected without queueing; got {rejected:?}"
                    );
                    rejected_total += 1;
                }

                let health_after_refill_rejects = lock_manager.health();
                prop_assert!(
                    health_after_refill_rejects.concurrency_limit_blocks_total
                        >= u64::try_from(rejected_total).expect("rejected attempts fits u64"),
                    "all saturated rejections should be reflected in telemetry: {health_after_refill_rejects:?}"
                );
                prop_assert_eq!(
                    lock_manager.active_locks().len(),
                    max_concurrent,
                    "post-refill saturated bursts must leave active locks capped"
                );
                let records_after_refill_rejects = storage
                    .export_workflows(ExportQuery::default())
                    .await
                    .expect("export workflow records after refill saturation");
                prop_assert_eq!(
                    records_after_refill_rejects.len(),
                    expected_records,
                    "post-refill saturated bursts must not enqueue phantom workflow records"
                );
            }
            Ok(())
        })?;
    }

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
    fn precancelled_retry_handler_does_not_execute_or_log_steps(
        max_retries in 0usize..5,
        retry_results_before_done in 1usize..8,
        retry_delay_ms in 0u64..2_000,
    ) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let config = WorkflowRunnerConfig {
                max_retries_per_step: max_retries,
                workflow_total_deadline_ms: 0,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("retry_precancel", config).await;
            let workflow = Arc::new(RetryThenDoneWorkflow::new(
                retry_results_before_done,
                retry_delay_ms,
            ));
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
            cx.cancel_with(
                frankenterm_core::outcome::CancelKind::User,
                Some("workflow handler pre-cancel property"),
            );

            let result = runner
                .run_workflow_with_cx(&cx, PANE_ID, workflow, &execution_id, 0)
                .await;

            prop_assert!(
                matches!(result, WorkflowExecutionResult::Error { ref error, .. } if error.contains("cancelled pre-start")),
                "pre-cancelled retry handler should fail before step execution; got {result:?}"
            );
            prop_assert_eq!(
                attempts.load(Ordering::SeqCst),
                0,
                "pre-cancelled retry handler must not execute the first step"
            );
            let record = storage
                .get_workflow(&execution_id)
                .await
                .expect("load pre-cancelled workflow")
                .expect("pre-cancelled workflow exists");
            prop_assert_eq!(record.status.as_str(), "failed");
            prop_assert!(
                record
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("cancelled pre-start")),
                "pre-cancel reason should persist: {:?}",
                record.error
            );
            let logs = storage
                .get_step_logs(&execution_id)
                .await
                .expect("load pre-cancelled retry logs");
            prop_assert!(
                logs.is_empty(),
                "pre-cancelled retry handler must not emit retry or terminal step logs: {logs:?}"
            );
            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after pre-cancelled retry handler"
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
            let logs = storage
                .get_step_logs(&execution_id)
                .await
                .expect("load cancelled retry logs");
            match attempts.load(Ordering::SeqCst) {
                0 => prop_assert!(
                    logs.is_empty(),
                    "cancellation before first handler attempt must not log a step: {logs:?}"
                ),
                1 => assert_retry_step_log_contract(&logs, 1, None)?,
                attempts => prop_assert!(
                    false,
                    "retry cancellation must not execute or log multiple attempts: attempts={attempts}, logs={logs:?}"
                ),
            }
            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after retry cancellation"
            );
            Ok(())
        })?;
    }

    #[test]
    fn cancel_during_wait_condition_property(cancel_after_ms in 1u64..35, wait_ms in 250u64..2_000) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let config = WorkflowRunnerConfig {
                max_retries_per_step: 3,
                workflow_total_deadline_ms: 0,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("wait_cancel", config).await;
            let workflow = Arc::new(AwaitingWaitWorkflow::new(wait_ms, cancel_after_ms));
            let attempts = workflow.attempts();
            runner.register_workflow(workflow.clone());

            let start = runner
                .handle_detection(PANE_ID, &detection(WAIT_CANCEL_RULE), None)
                .await;
            let execution_id = start
                .execution_id()
                .unwrap_or_else(|| panic!("workflow did not start: {start:?}"))
                .to_string();
            let cx = frankenterm_core::cx::for_testing();

            let started_at = Instant::now();
            let result = runner
                .run_workflow_with_cx(&cx, PANE_ID, workflow, &execution_id, 0)
                .await;
            let elapsed = started_at.elapsed();

            prop_assert!(
                matches!(result, WorkflowExecutionResult::Aborted { ref reason, .. } if reason.contains("workflow wait condition cancelled")),
                "wait cancellation should surface as an aborted workflow; got {result:?}"
            );
            prop_assert_eq!(
                attempts.load(Ordering::SeqCst),
                1,
                "cancelled wait must not continue into a later workflow step"
            );
            prop_assert!(
                elapsed < Duration::from_millis(wait_ms),
                "cancelled wait should finish before the requested wait; elapsed={elapsed:?}, wait_ms={wait_ms}"
            );
            prop_assert!(
                elapsed < Duration::from_secs(1),
                "cancelled wait should be prompt; elapsed={elapsed:?}"
            );

            let record = storage
                .get_workflow(&execution_id)
                .await
                .expect("load wait-cancelled workflow")
                .expect("wait-cancelled workflow exists");
            prop_assert_eq!(
                record.status.as_str(),
                "failed",
                "cancelled wait must not leave execution stuck as {:?}",
                record.status
            );
            prop_assert!(
                record
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("workflow wait condition cancelled")),
                "wait cancel reason should persist: {:?}",
                record.error
            );
            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after wait cancellation"
            );
            Ok(())
        })?;
    }

    #[test]
    fn wait_timeout_beats_overall_deadline_property(
        wait_timeout_ms in 5u64..35,
        deadline_margin_ms in 250u64..600,
        wait_overrun_ms in 1_000u64..2_000,
    ) {
        let fixture = RuntimeFixture::current_thread();
        fixture.block_on(async move {
            let wait_ms = wait_timeout_ms + wait_overrun_ms;
            let workflow_total_deadline_ms = wait_timeout_ms + deadline_margin_ms;
            prop_assume!(workflow_total_deadline_ms < wait_ms);

            let config = WorkflowRunnerConfig {
                step_timeout_ms: wait_timeout_ms + 1_000,
                workflow_total_deadline_ms,
                ..WorkflowRunnerConfig::default()
            };
            let (runner, storage, lock_manager) = build_runner("wait_timeout_deadline", config).await;
            let workflow = Arc::new(TimedWaitThenDoneWorkflow::new(wait_ms, wait_timeout_ms));
            let attempts = workflow.attempts();
            runner.register_workflow(workflow.clone());

            let start = runner
                .handle_detection(PANE_ID, &detection(WAIT_TIMEOUT_RULE), None)
                .await;
            let execution_id = start
                .execution_id()
                .unwrap_or_else(|| panic!("workflow did not start: {start:?}"))
                .to_string();

            let started_at = Instant::now();
            let result = runner
                .run_workflow(PANE_ID, workflow, &execution_id, 0)
                .await;
            let elapsed = started_at.elapsed();

            prop_assert!(
                matches!(result, WorkflowExecutionResult::Completed { .. }),
                "explicit wait timeout should let workflow finish before overall deadline; got {result:?}"
            );
            prop_assert_eq!(
                attempts.load(Ordering::SeqCst),
                2,
                "timeout-bounded wait should continue exactly once into the completion step"
            );
            prop_assert!(
                elapsed < Duration::from_millis(workflow_total_deadline_ms),
                "wait timeout should beat the overall deadline; elapsed={elapsed:?}, workflow_total_deadline_ms={workflow_total_deadline_ms}"
            );
            prop_assert!(
                elapsed < Duration::from_millis(wait_ms),
                "wait timeout should cap the handler wait below condition duration; elapsed={elapsed:?}, wait_ms={wait_ms}"
            );

            let record = storage
                .get_workflow(&execution_id)
                .await
                .expect("load timeout-bounded workflow")
                .expect("timeout-bounded workflow exists");
            prop_assert_eq!(record.status.as_str(), "completed");
            prop_assert!(
                record.error.is_none(),
                "timeout-bounded workflow should not persist deadline failure: {:?}",
                record.error
            );
            prop_assert!(
                lock_manager.is_locked(PANE_ID).is_none(),
                "runner must release pane lock after timeout-bounded wait"
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
