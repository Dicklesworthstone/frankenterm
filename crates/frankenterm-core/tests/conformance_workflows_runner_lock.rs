#![cfg(feature = "asupersync-runtime")]
// The Send proof for this fixture's future walks the storage-open chain
// (`StorageHandle::new` -> `new_with_cx` -> ...) and, on nightly-2026-08-31 on
// macOS, exhausts the default 128-step budget before it settles: "overflow
// evaluating the requirement `impl Future<Output = (WorkflowRunner, ...)>`".
// The same code type-checks on the Linux proof workers, so this is the solver's
// depth budget, not a cycle. Raising the limit keeps the fixture buildable on
// both.
#![recursion_limit = "256"]

//! Conformance coverage for the WorkflowRunner <-> pane-lock contract.
//!
//! Contract:
//! - `handle_detection` claims a pane lock only for a startable workflow;
//! - non-start results must not claim or disturb unrelated locks;
//! - every `run_workflow` terminal path releases the claimed pane lock.

mod common;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::fixtures::RuntimeFixture;
use frankenterm_core::patterns::{AgentType, Detection, Severity};
use frankenterm_core::plan::ActionPlan;
use frankenterm_core::policy::{PolicyEngine, PolicyGatedInjector};
use frankenterm_core::storage::{ExportQuery, PaneRecord, StorageHandle, now_ms};
use frankenterm_core::wezterm::{MockWezterm, WeztermHandle};
use frankenterm_core::workflows::{
    BoxFuture, CxPolicyInjector, ManualWorkflowRunOutcome, PaneWorkflowLockManager, StepResult,
    WaitCondition, Workflow, WorkflowContext, WorkflowEngine, WorkflowExecutionResult,
    WorkflowRunner, WorkflowRunnerConfig, WorkflowStartResult, WorkflowStep, WorkflowTriggerPolicy,
};

const PANE_ID: u64 = 7204;
const OTHER_PANE_ID: u64 = 7205;
const RULE_ID: &str = "conformance.workflow.runner_lock";
const NONMATCHING_RULE_ID: &str = "conformance.workflow.no_match";

static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_db_path(label: &str) -> String {
    let counter = DB_COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir()
        .join(format!(
            "workflow_runner_lock_conformance_{label}_{counter}_{}.sqlite3",
            std::process::id()
        ))
        .to_string_lossy()
        .into_owned()
}

fn detection(rule_id: &str) -> Detection {
    Detection {
        rule_id: rule_id.to_string(),
        agent_type: AgentType::Codex,
        event_type: "workflow.conformance".to_string(),
        severity: Severity::Info,
        confidence: 1.0,
        extracted: serde_json::json!({}),
        matched_text: "workflow runner lock conformance".to_string(),
        span: (0, 33),
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
            title: Some(format!("workflow-lock-conformance-{pane_id}")),
            cwd: Some("/tmp/frankenterm-workflow-lock-conformance".to_string()),
            tty_name: None,
            first_seen_at: now,
            last_seen_at: now,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        })
        .await
        .expect("seed workflow lock conformance pane");
}

async fn build_runner(
    label: &str,
    config: WorkflowRunnerConfig,
    mock_contains_target_pane: bool,
) -> (
    WorkflowRunner,
    Arc<StorageHandle>,
    Arc<PaneWorkflowLockManager>,
) {
    let storage = Arc::new(
        StorageHandle::new(&temp_db_path(label))
            .await
            .expect("create workflow lock conformance storage"),
    );
    seed_pane(&storage, PANE_ID).await;
    seed_pane(&storage, OTHER_PANE_ID).await;

    let mock = MockWezterm::new();
    if mock_contains_target_pane {
        mock.add_default_pane(PANE_ID).await;
    }
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

#[derive(Clone)]
struct ScriptedWorkflow {
    name: &'static str,
    rule_id: &'static str,
    results: Arc<Vec<StepResult>>,
    trigger_policy: WorkflowTriggerPolicy,
    invalid_plan: bool,
    attempts: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
    context_observations: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl ScriptedWorkflow {
    fn new(name: &'static str, results: Vec<StepResult>) -> Self {
        Self {
            name,
            rule_id: RULE_ID,
            results: Arc::new(results),
            trigger_policy: WorkflowTriggerPolicy::allow_all(),
            invalid_plan: false,
            attempts: Arc::new(AtomicUsize::new(0)),
            cleanups: Arc::new(AtomicUsize::new(0)),
            context_observations: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_rule(mut self, rule_id: &'static str) -> Self {
        self.rule_id = rule_id;
        self
    }

    fn with_trigger_policy(mut self, trigger_policy: WorkflowTriggerPolicy) -> Self {
        self.trigger_policy = trigger_policy;
        self
    }

    fn with_invalid_plan(mut self) -> Self {
        self.invalid_plan = true;
        self
    }

    fn context_observations(&self) -> Vec<serde_json::Value> {
        self.context_observations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Workflow for ScriptedWorkflow {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        "WorkflowRunner lock conformance scripted workflow"
    }

    fn handles(&self, detection: &Detection) -> bool {
        detection.rule_id == self.rule_id
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        let count = self.results.len().max(1);
        (0..count)
            .map(|idx| WorkflowStep::new(format!("step_{idx}"), "scripted conformance step"))
            .collect()
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let attempts = Arc::clone(&self.attempts);
        let results = Arc::clone(&self.results);
        let context_observations = Arc::clone(&self.context_observations);
        let observation = serde_json::json!({
            "pane_id": ctx.pane_id(),
            "execution_id": ctx.execution_id(),
            "has_injector": ctx.has_injector(),
            "trigger": ctx.trigger().cloned(),
            "pane_meta": {
                "domain": ctx.pane_meta().domain.clone(),
                "title": ctx.pane_meta().title.clone(),
                "cwd": ctx.pane_meta().cwd.clone(),
            },
        });
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            context_observations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(observation);
            results
                .get(step_idx)
                .or_else(|| results.last())
                .cloned()
                .unwrap_or_else(StepResult::done_empty)
        })
    }

    fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
        let cleanups = Arc::clone(&self.cleanups);
        Box::pin(async move {
            cleanups.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn trigger_policy(&self) -> WorkflowTriggerPolicy {
        self.trigger_policy.clone()
    }

    fn to_action_plan(&self, _ctx: &WorkflowContext, _execution_id: &str) -> Option<ActionPlan> {
        if !self.invalid_plan {
            return None;
        }

        let mut plan =
            ActionPlan::builder("invalid workflow lock conformance plan", "test-workspace").build();
        plan.plan_version = u32::MAX;
        Some(plan)
    }
}

fn assert_pane_unlocked(lock_manager: &PaneWorkflowLockManager, case_name: &str) {
    assert!(
        lock_manager.is_locked(PANE_ID).is_none(),
        "{case_name}: pane lock must be released or never claimed"
    );
}

async fn start_workflow(
    runner: &WorkflowRunner,
    workflow: Arc<ScriptedWorkflow>,
) -> (String, WorkflowStartResult) {
    runner.register_workflow(workflow);
    let start = runner
        .handle_detection(PANE_ID, &detection(RULE_ID), None)
        .await;
    let execution_id = start
        .execution_id()
        .unwrap_or_else(|| panic!("workflow did not start: {start:?}"))
        .to_string();
    (execution_id, start)
}

#[test]
fn workflow_runner_claim_handshake_conformance() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let (runner, _storage, lock_manager) =
            build_runner("start_success", WorkflowRunnerConfig::default(), true).await;
        let workflow = Arc::new(ScriptedWorkflow::new(
            "claim_success",
            vec![StepResult::done_empty()],
        ));
        runner.register_workflow(Arc::clone(&workflow) as Arc<dyn Workflow>);
        let start = runner
            .handle_detection(PANE_ID, &detection(RULE_ID), None)
            .await;
        let execution_id = start
            .execution_id()
            .expect("started workflow should include execution id");
        let lock = lock_manager
            .is_locked(PANE_ID)
            .expect("Started result must leave lock claimed for runner handoff");
        assert_eq!(lock.workflow_name, "claim_success");
        assert_eq!(lock.execution_id, execution_id);
        let result = runner
            .run_workflow(PANE_ID, workflow, execution_id, 0)
            .await;
        assert!(
            matches!(result, WorkflowExecutionResult::Completed { .. }),
            "started workflow should complete in handoff check: {result:?}"
        );
        assert_pane_unlocked(&lock_manager, "start_success");

        let (runner, _storage, lock_manager) =
            build_runner("start_no_match", WorkflowRunnerConfig::default(), true).await;
        runner.register_workflow(Arc::new(
            ScriptedWorkflow::new("no_match_workflow", vec![StepResult::done_empty()])
                .with_rule(RULE_ID),
        ));
        let start = runner
            .handle_detection(PANE_ID, &detection(NONMATCHING_RULE_ID), None)
            .await;
        assert!(
            matches!(start, WorkflowStartResult::NoMatchingWorkflow { .. }),
            "nonmatching detection must not start: {start:?}"
        );
        assert_pane_unlocked(&lock_manager, "start_no_match");

        let (runner, _storage, lock_manager) =
            build_runner("start_untrusted", WorkflowRunnerConfig::default(), true).await;
        runner.register_workflow(Arc::new(
            ScriptedWorkflow::new("untrusted_source", vec![StepResult::done_empty()])
                .with_trigger_policy(WorkflowTriggerPolicy::allowlist([OTHER_PANE_ID])),
        ));
        let start = runner
            .handle_detection(PANE_ID, &detection(RULE_ID), None)
            .await;
        assert!(
            matches!(start, WorkflowStartResult::SourcePaneNotTrusted { .. }),
            "untrusted source must be refused before claiming lock: {start:?}"
        );
        assert_pane_unlocked(&lock_manager, "start_untrusted");

        let (runner, _storage, lock_manager) = build_runner(
            "start_already_locked",
            WorkflowRunnerConfig::default(),
            true,
        )
        .await;
        assert!(
            lock_manager
                .try_acquire(PANE_ID, "existing_workflow", "existing-exec")
                .is_acquired(),
            "test precondition should claim pane"
        );
        runner.register_workflow(Arc::new(ScriptedWorkflow::new(
            "already_locked",
            vec![StepResult::done_empty()],
        )));
        let start = runner
            .handle_detection(PANE_ID, &detection(RULE_ID), None)
            .await;
        assert!(
            matches!(
                start,
                WorkflowStartResult::PaneLocked {
                    ref held_by_workflow,
                    ref held_by_execution,
                    ..
                } if held_by_workflow == "existing_workflow"
                    && held_by_execution == "existing-exec"
            ),
            "already-locked pane must preserve holder details: {start:?}"
        );
        let lock = lock_manager
            .is_locked(PANE_ID)
            .expect("existing lock must remain held");
        assert_eq!(lock.workflow_name, "existing_workflow");
        assert_eq!(lock.execution_id, "existing-exec");
        assert_eq!(lock_manager.active_count(), 1);

        let config = WorkflowRunnerConfig {
            max_concurrent: 1,
            ..WorkflowRunnerConfig::default()
        };
        let (runner, _storage, lock_manager) =
            build_runner("start_concurrency_limit", config, true).await;
        assert!(
            lock_manager
                .try_acquire(OTHER_PANE_ID, "other_workflow", "other-exec")
                .is_acquired(),
            "test precondition should fill concurrency slot"
        );
        runner.register_workflow(Arc::new(ScriptedWorkflow::new(
            "concurrency_limited",
            vec![StepResult::done_empty()],
        )));
        let start = runner
            .handle_detection(PANE_ID, &detection(RULE_ID), None)
            .await;
        assert!(
            matches!(
                start,
                WorkflowStartResult::ConcurrencyLimitReached {
                    active: 1,
                    limit: 1
                }
            ),
            "concurrency-limit refusal must not claim target pane: {start:?}"
        );
        assert_pane_unlocked(&lock_manager, "start_concurrency_limit");
        assert_eq!(lock_manager.active_count(), 1);
    });
}

#[test]
fn workflow_runner_trigger_policy_refusal_is_fail_closed_conformance() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let (runner, storage, lock_manager) = build_runner(
            "trigger_policy_refusal",
            WorkflowRunnerConfig::default(),
            true,
        )
        .await;
        let workflow = Arc::new(
            ScriptedWorkflow::new(
                "high_trust_release_workflow",
                vec![StepResult::done_empty()],
            )
            .with_trigger_policy(WorkflowTriggerPolicy::allowlist([OTHER_PANE_ID])),
        );
        runner.register_workflow(Arc::clone(&workflow) as Arc<dyn Workflow>);

        let before_health = lock_manager.health();
        let start = runner
            .handle_detection(PANE_ID, &detection(RULE_ID), None)
            .await;

        assert!(
            matches!(
                start,
                WorkflowStartResult::SourcePaneNotTrusted {
                    source_pane_id: PANE_ID,
                    ref workflow_name,
                    ref rule_id,
                } if workflow_name == "high_trust_release_workflow" && rule_id == RULE_ID
            ),
            "low-trust source pane must not fire a high-trust allowlisted workflow: {start:?}"
        );
        assert!(
            start.is_source_pane_not_trusted(),
            "refused trigger should expose the source-pane trust predicate"
        );
        assert_pane_unlocked(&lock_manager, "trigger_policy_refusal_source");
        assert!(
            lock_manager.is_locked(OTHER_PANE_ID).is_none(),
            "refused trigger must not lock the trusted/high-trust pane"
        );
        let after_health = lock_manager.health();
        assert_eq!(
            before_health.acquisitions_total, after_health.acquisitions_total,
            "source-pane refusal must happen before lock acquisition"
        );
        assert_eq!(
            workflow.attempts.load(Ordering::SeqCst),
            0,
            "refused trigger must not execute workflow steps"
        );
        assert!(
            workflow.context_observations().is_empty(),
            "refused trigger must not construct or expose WorkflowContext"
        );
        assert!(
            storage
                .find_incomplete_workflows()
                .await
                .unwrap()
                .is_empty(),
            "refused trigger must not persist a resumable execution"
        );
        assert!(
            storage
                .export_workflows(ExportQuery::default())
                .await
                .unwrap()
                .is_empty(),
            "refused trigger must not persist any workflow execution row"
        );
    });
}

#[test]
fn workflow_runner_allowed_trigger_carries_context_and_releases_lock_conformance() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let (runner, storage, lock_manager) = build_runner(
            "allowed_trigger_context",
            WorkflowRunnerConfig::default(),
            true,
        )
        .await;
        let workflow = Arc::new(
            ScriptedWorkflow::new(
                "allowed_source_context",
                vec![StepResult::done(serde_json::json!({"accepted": true}))],
            )
            .with_trigger_policy(WorkflowTriggerPolicy::allowlist([PANE_ID])),
        );
        let (execution_id, start) = start_workflow(&runner, Arc::clone(&workflow)).await;
        assert!(
            start.is_started(),
            "allowlisted source pane should start workflow: {start:?}"
        );

        let persisted = storage
            .get_workflow(&execution_id)
            .await
            .expect("query persisted workflow")
            .expect("started workflow should be persisted");
        let persisted_context = persisted
            .context
            .as_ref()
            .expect("started workflow should persist trigger context");
        assert_eq!(persisted_context["source_pane_id"], PANE_ID);
        assert_eq!(persisted_context["rule_id"], RULE_ID);
        assert_eq!(persisted_context["detection"]["rule_id"], RULE_ID);
        assert_eq!(
            persisted_context["matched_text"],
            "workflow runner lock conformance"
        );
        assert!(
            lock_manager.is_locked(PANE_ID).is_some(),
            "started workflow should hold lock until run_workflow handoff"
        );

        let result = runner
            .run_workflow(PANE_ID, workflow.clone(), &execution_id, 0)
            .await;
        assert!(
            matches!(result, WorkflowExecutionResult::Completed { .. }),
            "allowlisted workflow should complete: {result:?}"
        );
        assert_pane_unlocked(&lock_manager, "allowed_trigger_context");

        let observations = workflow.context_observations();
        assert_eq!(
            observations.len(),
            1,
            "workflow should observe exactly one execution context"
        );
        let observed = &observations[0];
        assert_eq!(observed["pane_id"], PANE_ID);
        assert_eq!(observed["execution_id"], execution_id);
        assert_eq!(observed["has_injector"], true);
        assert_eq!(observed["trigger"]["source_pane_id"], PANE_ID);
        assert_eq!(observed["trigger"]["rule_id"], RULE_ID);
        assert_eq!(
            observed["trigger"]["detection"]["matched_text"],
            "workflow runner lock conformance"
        );
        assert_eq!(observed["pane_meta"]["domain"], "local");
        assert_eq!(
            observed["pane_meta"]["title"],
            format!("workflow-lock-conformance-{PANE_ID}")
        );
        assert_eq!(
            observed["pane_meta"]["cwd"],
            "/tmp/frankenterm-workflow-lock-conformance"
        );
    });
}

#[derive(Clone, Copy)]
enum ExpectedRunResult {
    Completed,
    AbortedContains(&'static str),
    ErrorContains(&'static str),
}

struct RunConformanceCase {
    name: &'static str,
    workflow: ScriptedWorkflow,
    config: WorkflowRunnerConfig,
    mock_contains_target_pane: bool,
    expected: ExpectedRunResult,
}

impl RunConformanceCase {
    fn new(name: &'static str, workflow: ScriptedWorkflow, expected: ExpectedRunResult) -> Self {
        Self {
            name,
            workflow,
            config: WorkflowRunnerConfig::default(),
            mock_contains_target_pane: true,
            expected,
        }
    }
}

async fn assert_run_case(case: RunConformanceCase) {
    let (runner, _storage, lock_manager) = build_runner(
        case.name,
        case.config.clone(),
        case.mock_contains_target_pane,
    )
    .await;
    let workflow = Arc::new(case.workflow);
    let (execution_id, _start) = start_workflow(&runner, Arc::clone(&workflow)).await;
    assert!(
        lock_manager.is_locked(PANE_ID).is_some(),
        "{}: Started workflow must claim lock before run",
        case.name
    );

    let result = runner
        .run_workflow(PANE_ID, workflow, &execution_id, 0)
        .await;

    match case.expected {
        ExpectedRunResult::Completed => assert!(
            matches!(result, WorkflowExecutionResult::Completed { .. }),
            "{}: expected Completed, got {result:?}",
            case.name
        ),
        ExpectedRunResult::AbortedContains(needle) => assert!(
            matches!(result, WorkflowExecutionResult::Aborted { ref reason, .. } if reason.contains(needle)),
            "{}: expected Aborted containing {needle:?}, got {result:?}",
            case.name
        ),
        ExpectedRunResult::ErrorContains(needle) => assert!(
            matches!(result, WorkflowExecutionResult::Error { ref error, .. } if error.contains(needle)),
            "{}: expected Error containing {needle:?}, got {result:?}",
            case.name
        ),
    }

    assert_pane_unlocked(&lock_manager, case.name);
    assert_eq!(
        lock_manager.active_count(),
        0,
        "{}: runner must leave no active locks after terminal result",
        case.name
    );
}

#[test]
fn workflow_runner_terminal_paths_release_lock_conformance() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let cases = vec![
            RunConformanceCase::new(
                "done_terminal",
                ScriptedWorkflow::new(
                    "done_terminal",
                    vec![StepResult::done(serde_json::json!({"ok": true}))],
                ),
                ExpectedRunResult::Completed,
            ),
            RunConformanceCase::new(
                "fallthrough_terminal",
                ScriptedWorkflow::new("fallthrough_terminal", vec![StepResult::cont()]),
                ExpectedRunResult::Completed,
            ),
            RunConformanceCase::new(
                "abort_terminal",
                ScriptedWorkflow::new("abort_terminal", vec![StepResult::abort("scripted abort")]),
                ExpectedRunResult::AbortedContains("scripted abort"),
            ),
            {
                let mut case = RunConformanceCase::new(
                    "retry_exhausted_terminal",
                    ScriptedWorkflow::new("retry_exhausted_terminal", vec![StepResult::retry(0)]),
                    ExpectedRunResult::AbortedContains("Max retries"),
                );
                case.config.max_retries_per_step = 1;
                case
            },
            RunConformanceCase::new(
                "jump_exhausted_terminal",
                ScriptedWorkflow::new("jump_exhausted_terminal", vec![StepResult::jump_to(0)]),
                ExpectedRunResult::AbortedContains("exceeded maximum jump count"),
            ),
            RunConformanceCase::new(
                "external_wait_without_registry_terminal",
                ScriptedWorkflow::new(
                    "external_wait_without_registry_terminal",
                    vec![StepResult::wait_for(WaitCondition::external(
                        "missing-signal",
                    ))],
                ),
                ExpectedRunResult::AbortedContains("requires registry"),
            ),
            {
                let mut case = RunConformanceCase::new(
                    "send_text_injector_error_terminal",
                    ScriptedWorkflow::new(
                        "send_text_injector_error_terminal",
                        vec![StepResult::send_text("echo conformance")],
                    ),
                    ExpectedRunResult::AbortedContains("Text injection failed"),
                );
                case.mock_contains_target_pane = false;
                case
            },
            RunConformanceCase::new(
                "invalid_plan_terminal",
                ScriptedWorkflow::new("invalid_plan_terminal", vec![StepResult::done_empty()])
                    .with_invalid_plan(),
                ExpectedRunResult::ErrorContains("Plan validation failed"),
            ),
        ];

        for case in cases {
            assert_run_case(case).await;
        }
    });
}

#[test]
fn workflow_runner_prestart_cancel_releases_claimed_lock_conformance() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        let (runner, _storage, lock_manager) =
            build_runner("prestart_cancel", WorkflowRunnerConfig::default(), true).await;
        let workflow = Arc::new(ScriptedWorkflow::new(
            "prestart_cancel",
            vec![StepResult::done_empty()],
        ));
        let (execution_id, _start) = start_workflow(&runner, Arc::clone(&workflow)).await;
        assert!(
            lock_manager.is_locked(PANE_ID).is_some(),
            "started workflow should hold lock before prestart cancellation"
        );

        let cx = frankenterm_core::cx::for_testing();
        cx.cancel_with(
            frankenterm_core::outcome::CancelKind::User,
            Some("workflow runner lock conformance prestart cancel"),
        );

        let result = runner
            .run_workflow_with_cx(&cx, PANE_ID, workflow, &execution_id, 0)
            .await;
        assert!(
            matches!(result, WorkflowExecutionResult::Error { ref error, .. } if error.contains("cancelled pre-start")),
            "prestart cancellation should surface as Error: {result:?}"
        );
        assert_pane_unlocked(&lock_manager, "prestart_cancel");
        assert_eq!(lock_manager.active_count(), 0);
    });
}

#[test]
fn workflow_runner_manual_run_persists_record_and_releases_lock_ft_cli44() {
    let fixture = RuntimeFixture::current_thread();
    fixture.block_on(async {
        // 1. A manual run (the MCP / robot / human-CLI entry points) must
        //    use the same lock + execution-record protocol as the detection
        //    path. Before ft-cli44 these callers invoked `run_workflow`
        //    directly with a fabricated execution id: no record existed, so
        //    every record-requiring persistence helper failed with
        //    "Workflow not found: <execution_id>" and the run was invisible
        //    to status/abort surfaces.
        let (runner, storage, lock_manager) =
            build_runner("manual_run", WorkflowRunnerConfig::default(), true).await;
        let workflow = Arc::new(ScriptedWorkflow::new(
            "manual_run_wf",
            vec![StepResult::done_empty()],
        ));
        runner.register_workflow(Arc::clone(&workflow) as Arc<dyn Workflow>);
        let cx = frankenterm_core::cx::for_testing();
        let outcome = runner
            .run_workflow_manual_with_cx(
                &cx,
                PANE_ID,
                workflow,
                "mcp-manual_run_wf-1",
                Some(serde_json::json!({ "trigger": "mcp" })),
            )
            .await;
        let ManualWorkflowRunOutcome::Ran(result) = outcome else {
            panic!("manual run should acquire lock + start: {outcome:?}");
        };
        assert!(
            matches!(result, WorkflowExecutionResult::Completed { .. }),
            "manual run should complete: {result:?}"
        );
        let record = storage
            .get_workflow("mcp-manual_run_wf-1")
            .await
            .expect("get_workflow query")
            .expect("manual run must persist an execution record (ft-cli44)");
        assert_eq!(record.workflow_name, "manual_run_wf");
        assert_eq!(record.pane_id, PANE_ID);
        assert_eq!(record.status, "completed");
        assert_eq!(
            record
                .context
                .as_ref()
                .and_then(|c| c.get("trigger"))
                .and_then(|v| v.as_str()),
            Some("mcp"),
            "manual trigger context must be persisted for forensics"
        );
        assert_pane_unlocked(&lock_manager, "manual_run");
        assert_eq!(lock_manager.active_count(), 0);

        // 2. A manual run against a pane whose lock is held by another
        //    execution must refuse with PaneLocked, create no record, and
        //    leave the holder's lock untouched.
        let (runner, storage, lock_manager) =
            build_runner("manual_run_locked", WorkflowRunnerConfig::default(), true).await;
        let holder = Arc::new(ScriptedWorkflow::new(
            "manual_lock_holder",
            vec![StepResult::done_empty()],
        ));
        let (holder_execution_id, _start) = start_workflow(&runner, holder).await;
        assert!(
            lock_manager.is_locked(PANE_ID).is_some(),
            "lock holder must be claimed before manual contention check"
        );
        let contender = Arc::new(ScriptedWorkflow::new(
            "manual_lock_contender",
            vec![StepResult::done_empty()],
        ));
        let outcome = runner
            .run_workflow_manual_with_cx(&cx, PANE_ID, contender, "mcp-contender-1", None)
            .await;
        assert!(
            matches!(
                outcome,
                ManualWorkflowRunOutcome::PaneLocked { ref held_by_execution, .. }
                    if *held_by_execution == holder_execution_id
            ),
            "manual run against a held lock must refuse with the holder's id: {outcome:?}"
        );
        assert!(
            storage
                .get_workflow("mcp-contender-1")
                .await
                .expect("get_workflow query")
                .is_none(),
            "refused manual run must not create an execution record"
        );
        assert!(
            lock_manager.is_locked(PANE_ID).is_some(),
            "holder's lock must survive a refused manual contender"
        );
    });
}
