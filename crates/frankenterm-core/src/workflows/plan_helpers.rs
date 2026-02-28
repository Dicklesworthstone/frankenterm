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
