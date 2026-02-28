//! Workflow trait, step metadata, and workflow info types.
//!
//! Defines the `Workflow` trait (the interface all workflows implement),
//! `WorkflowStep` (step metadata), and `WorkflowInfo` (serializable workflow
//! descriptor for listing and discovery).
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Workflow Steps
// ============================================================================

/// A step in a workflow definition.
///
/// Steps provide metadata for display, logging, and debugging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    /// Step name (identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
}

impl WorkflowStep {
    /// Create a new workflow step
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}


// ============================================================================
// Workflow Trait
// ============================================================================

/// A durable, resumable workflow definition.
///
/// Workflows are explicit state machines with a uniform execution model.
/// Implement this trait to define custom automation workflows.
///
/// # Example
///
/// ```no_run
/// use frankenterm_core::workflows::{
///     Workflow, WorkflowContext, WorkflowStep, StepResult, WaitCondition, BoxFuture,
/// };
/// use frankenterm_core::patterns::Detection;
///
/// struct PromptInjectionWorkflow;
///
/// impl Workflow for PromptInjectionWorkflow {
///     fn name(&self) -> &'static str { "prompt_injection" }
///     fn description(&self) -> &'static str { "Sends a prompt and waits for response" }
///
///     fn handles(&self, detection: &Detection) -> bool {
///         detection.rule_id.starts_with("trigger.prompt_injection")
///     }
///
///     fn steps(&self) -> Vec<WorkflowStep> {
///         vec![
///             WorkflowStep::new("send_prompt", "Send prompt to terminal"),
///             WorkflowStep::new("wait_response", "Wait for response pattern"),
///         ]
///     }
///
///     fn execute_step(&self, _ctx: &mut WorkflowContext, step_idx: usize) -> BoxFuture<'_, StepResult> {
///         Box::pin(async move {
///             match step_idx {
///                 0 => StepResult::cont(),
///                 1 => StepResult::wait_for(WaitCondition::pattern("response.complete")),
///                 _ => StepResult::done_empty(),
///             }
///         })
///     }
/// }
/// ```
pub trait Workflow: Send + Sync {
    /// Workflow name (unique identifier)
    fn name(&self) -> &'static str;

    /// Human-readable description
    fn description(&self) -> &'static str;

    /// Check if this workflow handles a given detection.
    ///
    /// Return true if this workflow should be triggered by the detection.
    fn handles(&self, detection: &crate::patterns::Detection) -> bool;

    /// Get the list of steps in this workflow.
    ///
    /// Step metadata is used for display, logging, and debugging.
    fn steps(&self) -> Vec<WorkflowStep>;

    /// Execute a single step of the workflow.
    ///
    /// # Arguments
    /// * `ctx` - Workflow context with storage, pane state, and config
    /// * `step_idx` - Zero-based step index
    ///
    /// # Returns
    /// A `StepResult` indicating what should happen next.
    fn execute_step(&self, ctx: &mut WorkflowContext, step_idx: usize)
    -> BoxFuture<'_, StepResult>;

    /// Optional cleanup when workflow is aborted or completes with error.
    ///
    /// Override to release resources, revert partial changes, etc.
    fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Get the number of steps in this workflow.
    fn step_count(&self) -> usize {
        self.steps().len()
    }

    // ========================================================================
    // Extended metadata for workflow listing (all with default implementations)
    // ========================================================================

    /// Event types that can trigger this workflow (e.g., "session.compaction").
    /// Returns empty slice if not triggered by specific event types.
    fn trigger_event_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Rule IDs that can trigger this workflow (e.g., "compaction.detected").
    /// Returns empty slice if not triggered by specific rules.
    fn trigger_rule_ids(&self) -> &'static [&'static str] {
        &[]
    }

    /// Agent types this workflow supports (e.g., ["codex", "claude_code"]).
    /// Returns empty slice if supports all agent types.
    fn supported_agent_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this workflow requires a target pane to operate on.
    fn requires_pane(&self) -> bool {
        true
    }

    /// Whether this workflow requires approval before execution.
    fn requires_approval(&self) -> bool {
        false
    }

    /// Whether this workflow can be aborted while running.
    fn can_abort(&self) -> bool {
        true
    }

    /// Whether this workflow performs destructive operations.
    fn is_destructive(&self) -> bool {
        false
    }

    /// Names of workflows this one depends on (must complete first).
    fn dependencies(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether this workflow is currently enabled.
    fn is_enabled(&self) -> bool {
        true
    }

    // ========================================================================
    // Plan-first execution support (wa-upg.2.3)
    // ========================================================================

    /// Generate an ActionPlan representing this workflow's execution.
    ///
    /// This enables plan-first execution where the plan is persisted before
    /// any side effects are performed. The plan provides:
    /// - Deterministic step descriptions for audit trails
    /// - Idempotency keys for safe replay
    /// - Structured verification and failure handling
    ///
    /// # Arguments
    /// * `ctx` - Workflow context with pane state and trigger info
    /// * `execution_id` - The workflow execution ID (used in plan metadata)
    ///
    /// # Default Implementation
    /// Returns `None`, meaning the workflow uses legacy step-by-step execution.
    /// Workflows can override this to provide plan-first execution.
    fn to_action_plan(
        &self,
        _ctx: &WorkflowContext,
        _execution_id: &str,
    ) -> Option<crate::plan::ActionPlan> {
        None
    }

    /// Convert workflow steps to StepPlan entries for plan generation.
    ///
    /// Helper method that creates basic StepPlans from WorkflowStep metadata.
    /// Workflows can use this as a starting point and enrich the plans with
    /// preconditions, verification, and failure handling.
    fn steps_to_plans(&self, pane_id: u64) -> Vec<crate::plan::StepPlan> {
        self.steps()
            .iter()
            .enumerate()
            .map(|(idx, step)| {
                let step_number = (idx + 1) as u32;
                crate::plan::StepPlan::new(
                    step_number,
                    crate::plan::StepAction::Custom {
                        action_type: format!("workflow_step:{}", step.name),
                        payload: serde_json::json!({
                            "workflow": self.name(),
                            "step_name": step.name,
                            "description": step.description,
                            "pane_id": pane_id,
                        }),
                    },
                    &step.description,
                )
            })
            .collect()
    }
}

// ============================================================================
// Workflow Info (for listing)
// ============================================================================

/// Information about a workflow for listing and discovery.
///
/// This struct captures the metadata exposed by the `Workflow` trait
/// in a serializable form for robot mode and TUI display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInfo {
    /// Workflow name (unique identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Whether the workflow is enabled
    pub enabled: bool,
    /// Event types that can trigger this workflow
    pub trigger_event_types: Vec<String>,
    /// Rule IDs that can trigger this workflow
    pub trigger_rule_ids: Vec<String>,
    /// Agent types this workflow supports (empty = all)
    pub agent_types: Vec<String>,
    /// Number of steps in the workflow
    pub step_count: usize,
    /// Whether this workflow requires a target pane
    pub requires_pane: bool,
    /// Whether this workflow requires approval before execution
    pub requires_approval: bool,
    /// Whether this workflow can be aborted while running
    pub can_abort: bool,
    /// Whether this workflow performs destructive operations
    pub destructive: bool,
    /// Names of workflows this one depends on
    pub dependencies: Vec<String>,
}

impl WorkflowInfo {
    /// Create a WorkflowInfo from a workflow trait object.
    pub fn from_workflow(workflow: &dyn Workflow) -> Self {
        Self {
            name: workflow.name().to_string(),
            description: workflow.description().to_string(),
            enabled: workflow.is_enabled(),
            trigger_event_types: workflow
                .trigger_event_types()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            trigger_rule_ids: workflow
                .trigger_rule_ids()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            agent_types: workflow
                .supported_agent_types()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            step_count: workflow.step_count(),
            requires_pane: workflow.requires_pane(),
            requires_approval: workflow.requires_approval(),
            can_abort: workflow.can_abort(),
            destructive: workflow.is_destructive(),
            dependencies: workflow
                .dependencies()
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}
