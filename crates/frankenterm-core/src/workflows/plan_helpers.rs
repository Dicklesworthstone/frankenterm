//! Plan-first execution helpers for workflow action plans.
//!
//! Provides helpers for generating, validating, and executing `ActionPlan`-based
//! workflows that combine step metadata with idempotency keys and verification.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

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
    /// The idempotency ledger could not be read (storage failure or
    /// cx-cancel). An unavailable ledger is indistinguishable from an
    /// incomplete side-effecting step, so replay must fail closed — but
    /// unlike [`Self::PartiallyExecuted`] there is no real `started_at` to
    /// report, so this variant carries none rather than fabricating one.
    LedgerUnavailable,
}

/// Whether a logged step row represents a *confirmed-completed* step for
/// idempotency purposes.
///
/// Fails closed (ft-3rc59): a `send_text` row counts as completed ONLY when
/// its policy summary records an explicit `allow`. An absent policy summary
/// (`None`) is NOT treated as completed — a successful injection never
/// produces one (`policy_summary_from_injection` always serializes a summary
/// for every `InjectionResult`), so a missing summary marks an anomalous /
/// degraded log row whose completion cannot be confirmed. Treating that row
/// as completed (the previous behaviour) let crash-resume silently SKIP a
/// side-effecting send, dropping it. Returning `false` instead routes the row
/// to [`IdempotencyCheckResult::PartiallyExecuted`] (the runner aborts),
/// surfacing the ambiguity rather than dropping the send. A denied /
/// require-approval summary (`Some` but not `allow`) is likewise not
/// completed, matching the established partial-execution handling.
///
/// `continue` / `done` control results are always completed; any other result
/// type is not.
fn step_log_is_completed(result_type: &str, policy_summary: Option<&str>) -> bool {
    match result_type {
        "continue" | "done" => true,
        "send_text" => policy_summary
            .is_some_and(|summary| policy_summary_decision_is_allow(summary).unwrap_or(false)),
        _ => false,
    }
}

/// Check if a step has already been executed based on its idempotency key.
///
/// This enables safe replay by checking the step log for previous executions.
///
/// ft-dit9w: ergonomic wrapper around [`check_step_idempotency_with_cx`].
pub async fn check_step_idempotency(
    storage: &StorageHandle,
    execution_id: &str,
    idempotency_key: &crate::plan::IdempotencyKey,
    step_index: usize,
) -> IdempotencyCheckResult {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    check_step_idempotency_with_cx(&cx, storage, execution_id, idempotency_key, step_index).await
}

/// ft-xbnl0.2.3 Cx-first sibling of [`check_step_idempotency`].
///
/// Tick 186: routes the single storage call (get_step_logs)
/// through get_step_logs_with_cx. On cx-cancel or storage failure,
/// the check fails closed as `LedgerUnavailable`: an unavailable
/// idempotency ledger is indistinguishable from an incomplete
/// side-effecting step, so replay must stop instead of running the
/// step from scratch. (Previously this path fabricated a
/// `PartiallyExecuted { started_at: now_ms() }`, which made the runner's
/// abort reason claim the step "was started at" the time of the *check*.)
pub async fn check_step_idempotency_with_cx(
    cx: &crate::cx::Cx,
    storage: &StorageHandle,
    execution_id: &str,
    idempotency_key: &crate::plan::IdempotencyKey,
    step_index: usize,
) -> IdempotencyCheckResult {
    let Ok(logs) = storage.get_step_logs_with_cx(cx, execution_id).await else {
        return IdempotencyCheckResult::LedgerUnavailable;
    };

    let mut latest_completed: Option<(i64, Option<String>)> = None;
    let mut latest_started: Option<i64> = None;

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

        let is_completed =
            step_log_is_completed(log.result_type.as_str(), log.policy_summary.as_deref());

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

#[cfg(test)]
mod tests {
    use super::*;
    // `run_async_test` calls `runtime.block_on(..)`; `block_on` is provided by
    // the `CompatRuntime` trait, which must be in scope at the call site.
    #[allow(unused_imports)]
    use crate::patterns::Detection;
    use crate::runtime_async::CompatRuntime;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build plan helper test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    // ========================================================================
    // step_log_is_completed — fail-closed completion determination (ft-3rc59)
    // ========================================================================

    #[test]
    fn send_text_with_missing_policy_summary_is_not_completed() {
        // Regression guard for ft-3rc59: a send_text log row whose policy
        // summary is absent must NOT be treated as completed — otherwise
        // crash-resume reads it as AlreadyCompleted and silently SKIPS the
        // side-effecting send, dropping it. Fail closed.
        assert!(!step_log_is_completed("send_text", None));
    }

    #[test]
    fn send_text_with_allow_summary_is_completed() {
        // The genuine idempotent-skip path: a confirmed `allow` send is
        // completed and is correctly skipped on replay.
        assert!(step_log_is_completed(
            "send_text",
            Some(r#"{"decision":"allow"}"#)
        ));
    }

    #[test]
    fn send_text_with_deny_summary_is_not_completed() {
        // A denied send is not "completed"; replay must re-evaluate/abort
        // rather than skip — same handling the absent-summary case now gets.
        assert!(!step_log_is_completed(
            "send_text",
            Some(r#"{"decision":"deny"}"#)
        ));
    }

    #[test]
    fn send_text_with_unparseable_summary_is_not_completed() {
        // An unparseable summary cannot confirm an allow → fail closed.
        assert!(!step_log_is_completed("send_text", Some("not json")));
    }

    #[test]
    fn control_results_are_completed_without_summary() {
        // Non-side-effecting control results carry no policy summary and are
        // always completed.
        assert!(step_log_is_completed("continue", None));
        assert!(step_log_is_completed("done", None));
    }

    #[test]
    fn unknown_result_type_is_never_completed() {
        assert!(!step_log_is_completed("something_else", None));
        assert!(!step_log_is_completed(
            "send_text_v2",
            Some(r#"{"decision":"allow"}"#)
        ));
    }

    // ========================================================================
    // Mock Workflow for plan generation tests
    // ========================================================================

    struct PlanTestWorkflow;

    impl Workflow for PlanTestWorkflow {
        fn name(&self) -> &'static str {
            "plan_test"
        }

        fn description(&self) -> &'static str {
            "Plan test workflow"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with("plan.")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("check", "Check preconditions"),
                WorkflowStep::new("execute", "Execute action"),
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
                    _ => StepResult::done_empty(),
                }
            })
        }
    }

    struct EmptyStepsWorkflow;

    impl Workflow for EmptyStepsWorkflow {
        fn name(&self) -> &'static str {
            "empty_steps"
        }

        fn description(&self) -> &'static str {
            "Workflow with no steps"
        }

        fn handles(&self, _detection: &Detection) -> bool {
            false
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            _step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async { StepResult::done_empty() })
        }
    }

    // ========================================================================
    // workflow_to_action_plan tests
    // ========================================================================

    #[test]
    fn action_plan_from_workflow_basic() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-100", 42, "exec-abc");

        assert_eq!(plan.title, "Plan test workflow");
        assert_eq!(plan.workspace_id, "ws-100");
        assert_eq!(plan.steps.len(), 2);
        assert!(!plan.plan_id.is_placeholder());
    }

    #[test]
    fn action_plan_step_numbering() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");

        assert_eq!(plan.steps[0].step_number, 1);
        assert_eq!(plan.steps[1].step_number, 2);
    }

    #[test]
    fn action_plan_step_descriptions() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");

        assert_eq!(plan.steps[0].description, "Check preconditions");
        assert_eq!(plan.steps[1].description, "Execute action");
    }

    #[test]
    fn action_plan_metadata_includes_workflow_name() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 55, "exec-xyz");

        let meta = plan.metadata.as_ref().unwrap();
        assert_eq!(meta["workflow_name"], "plan_test");
        assert_eq!(meta["execution_id"], "exec-xyz");
        assert_eq!(meta["pane_id"], 55);
        assert_eq!(meta["generated_by"], "workflow_to_action_plan");
    }

    #[test]
    fn action_plan_has_created_at() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");

        // created_at should be set (non-None)
        assert!(plan.created_at.is_some());
    }

    #[test]
    fn action_plan_validates() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");

        // Plan should pass validation
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn action_plan_from_empty_workflow() {
        let wf = EmptyStepsWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");

        assert_eq!(plan.steps.len(), 0);
        assert_eq!(plan.title, "Workflow with no steps");
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn action_plan_deterministic_hash() {
        let wf = PlanTestWorkflow;
        let plan_a = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");
        let plan_b = workflow_to_action_plan(&wf, "ws-1", 1, "exec-1");

        // Same inputs → same plan ID (hash is content-addressed)
        assert_eq!(plan_a.plan_id, plan_b.plan_id);
    }

    #[test]
    fn action_plan_different_workspace_different_hash() {
        let wf = PlanTestWorkflow;
        let plan_a = workflow_to_action_plan(&wf, "ws-A", 1, "exec-1");
        let plan_b = workflow_to_action_plan(&wf, "ws-B", 1, "exec-1");

        assert_ne!(plan_a.plan_id, plan_b.plan_id);
    }

    #[test]
    fn action_plan_different_pane_different_payload() {
        let wf = PlanTestWorkflow;
        let plan_a = workflow_to_action_plan(&wf, "ws-1", 10, "exec-1");
        let plan_b = workflow_to_action_plan(&wf, "ws-1", 20, "exec-1");

        // Different pane_id → different step action payloads → different hash
        assert_ne!(plan_a.plan_id, plan_b.plan_id);
    }

    #[test]
    fn action_plan_step_actions_are_custom() {
        let wf = PlanTestWorkflow;
        let plan = workflow_to_action_plan(&wf, "ws-1", 7, "exec-1");

        for (i, step) in plan.steps.iter().enumerate() {
            match &step.action {
                crate::plan::StepAction::Custom {
                    action_type,
                    payload,
                } => {
                    let expected_names = ["check", "execute"];
                    assert_eq!(action_type, &format!("workflow_step:{}", expected_names[i]));
                    assert_eq!(payload["pane_id"], 7);
                }
                other => panic!("Expected Custom action, got {:?}", other),
            }
        }
    }

    // ========================================================================
    // IdempotencyCheckResult tests
    // ========================================================================

    #[test]
    fn idempotency_not_executed() {
        let result = IdempotencyCheckResult::NotExecuted;
        assert!(matches!(result, IdempotencyCheckResult::NotExecuted));
    }

    #[test]
    fn idempotency_already_completed() {
        let result = IdempotencyCheckResult::AlreadyCompleted {
            completed_at: 1700000000,
            previous_result: Some("done".to_string()),
        };
        if let IdempotencyCheckResult::AlreadyCompleted {
            completed_at,
            previous_result,
        } = result
        {
            assert_eq!(completed_at, 1700000000);
            assert_eq!(previous_result, Some("done".to_string()));
        } else {
            panic!("Wrong variant");
        }
    }

    #[test]
    fn idempotency_already_completed_no_result() {
        let result = IdempotencyCheckResult::AlreadyCompleted {
            completed_at: 1700000001,
            previous_result: None,
        };
        if let IdempotencyCheckResult::AlreadyCompleted {
            previous_result, ..
        } = result
        {
            assert!(previous_result.is_none());
        }
    }

    #[test]
    fn idempotency_partially_executed() {
        let result = IdempotencyCheckResult::PartiallyExecuted {
            started_at: 1700000002,
        };
        if let IdempotencyCheckResult::PartiallyExecuted { started_at } = result {
            assert_eq!(started_at, 1700000002);
        } else {
            panic!("Wrong variant");
        }
    }

    #[test]
    fn idempotency_check_result_clone() {
        let original = IdempotencyCheckResult::AlreadyCompleted {
            completed_at: 999,
            previous_result: Some("test".to_string()),
        };
        let cloned = original.clone();
        if let IdempotencyCheckResult::AlreadyCompleted {
            completed_at,
            previous_result,
        } = cloned
        {
            assert_eq!(completed_at, 999);
            assert_eq!(previous_result, Some("test".to_string()));
        }
    }

    #[test]
    fn idempotency_check_result_debug() {
        let result = IdempotencyCheckResult::NotExecuted;
        let debug = format!("{:?}", result);
        assert!(debug.contains("NotExecuted"));
    }

    #[test]
    fn idempotency_lookup_cancellation_fails_closed_as_partial() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("idempotency_lookup_cancelled.db")
                .to_string_lossy()
                .to_string();
            let storage = crate::storage::StorageHandle::new(&db_path).await.unwrap();
            let budget = crate::cx::Budget::new().with_poll_quota(0);
            let cx = crate::cx::Cx::for_testing_with_budget(budget);
            cx.cancel_with(
                crate::outcome::CancelKind::User,
                Some("idempotency lookup cancelled"),
            );

            let result = check_step_idempotency_with_cx(
                &cx,
                &storage,
                "exec-idempotency-cancelled",
                &crate::plan::IdempotencyKey("step:cancelled-ledger".to_string()),
                0,
            )
            .await;

            assert!(
                matches!(result, IdempotencyCheckResult::LedgerUnavailable),
                "cancelled ledger lookup must fail closed, got {result:?}"
            );
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn malformed_send_text_policy_summary_fails_closed_as_partial() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("idempotency_malformed_policy_summary.db")
                .to_string_lossy()
                .to_string();
            let storage = crate::storage::StorageHandle::new(&db_path).await.unwrap();
            let key = crate::plan::IdempotencyKey("step:malformed-policy".to_string());

            // workflow_step_logs.workflow_id has an enforced FK to
            // workflow_executions(id), which in turn FKs panes(pane_id)
            // (foreign_keys=ON since ft-s4myu), so both parent rows must
            // exist before the step log insert.
            storage
                .upsert_pane(crate::storage::PaneRecord {
                    pane_id: 1,
                    pane_uuid: None,
                    domain: "local".to_string(),
                    window_id: None,
                    tab_id: None,
                    title: None,
                    cwd: None,
                    tty_name: None,
                    first_seen_at: 100,
                    last_seen_at: 100,
                    observed: true,
                    ignore_reason: None,
                    last_decision_at: None,
                })
                .await
                .unwrap();
            storage
                .upsert_workflow(crate::storage::WorkflowRecord {
                    id: "exec-malformed-policy".to_string(),
                    workflow_name: "malformed-policy-test".to_string(),
                    pane_id: 1,
                    trigger_event_id: None,
                    current_step: 0,
                    status: "running".to_string(),
                    wait_condition: None,
                    context: None,
                    result: None,
                    error: None,
                    started_at: 100,
                    updated_at: 100,
                    completed_at: None,
                })
                .await
                .unwrap();

            storage
                .insert_step_log(
                    "exec-malformed-policy",
                    None,
                    0,
                    "send",
                    Some(key.0.clone()),
                    Some("send_text".to_string()),
                    "send_text",
                    Some(r#"{"idempotency_key":"step:malformed-policy"}"#.to_string()),
                    Some("{not-valid-policy-json".to_string()),
                    None,
                    None,
                    100,
                    125,
                )
                .await
                .unwrap();

            let result = check_step_idempotency_with_cx(
                &crate::cx::for_testing(),
                &storage,
                "exec-malformed-policy",
                &key,
                0,
            )
            .await;

            assert!(
                matches!(
                    result,
                    IdempotencyCheckResult::PartiallyExecuted { started_at: 100 }
                ),
                "malformed policy summary must not be treated as an allowed send_text replay: {result:?}"
            );
            storage.shutdown().await.unwrap();
        });
    }
}
