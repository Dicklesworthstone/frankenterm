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

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::patterns::Detection;

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
}
