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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Detection, Severity};

    // ========================================================================
    // Mock Workflow for testing trait default methods
    // ========================================================================

    struct MockWorkflow {
        trigger_events: &'static [&'static str],
        trigger_rules: &'static [&'static str],
        agent_types: &'static [&'static str],
        needs_pane: bool,
        needs_approval: bool,
        abortable: bool,
        destructive: bool,
        deps: &'static [&'static str],
        enabled: bool,
    }

    impl Default for MockWorkflow {
        fn default() -> Self {
            Self {
                trigger_events: &[],
                trigger_rules: &[],
                agent_types: &[],
                needs_pane: true,
                needs_approval: false,
                abortable: true,
                destructive: false,
                deps: &[],
                enabled: true,
            }
        }
    }

    impl Workflow for MockWorkflow {
        fn name(&self) -> &'static str {
            "mock_workflow"
        }

        fn description(&self) -> &'static str {
            "A mock workflow for testing"
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with("mock.")
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_one", "First step"),
                WorkflowStep::new("step_two", "Second step"),
                WorkflowStep::new("step_three", "Third step"),
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
                    1 => StepResult::cont(),
                    _ => StepResult::done_empty(),
                }
            })
        }

        fn trigger_event_types(&self) -> &'static [&'static str] {
            self.trigger_events
        }

        fn trigger_rule_ids(&self) -> &'static [&'static str] {
            self.trigger_rules
        }

        fn supported_agent_types(&self) -> &'static [&'static str] {
            self.agent_types
        }

        fn requires_pane(&self) -> bool {
            self.needs_pane
        }

        fn requires_approval(&self) -> bool {
            self.needs_approval
        }

        fn can_abort(&self) -> bool {
            self.abortable
        }

        fn is_destructive(&self) -> bool {
            self.destructive
        }

        fn dependencies(&self) -> &'static [&'static str] {
            self.deps
        }

        fn is_enabled(&self) -> bool {
            self.enabled
        }
    }

    fn make_detection(rule_id: &str) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type: AgentType::Codex,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::json!({}),
            matched_text: String::new(),
            span: (0, 0),
        }
    }

    // ========================================================================
    // WorkflowStep tests
    // ========================================================================

    #[test]
    fn workflow_step_new() {
        let step = WorkflowStep::new("init", "Initialize system");
        assert_eq!(step.name, "init");
        assert_eq!(step.description, "Initialize system");
    }

    #[test]
    fn workflow_step_new_from_string() {
        let step = WorkflowStep::new(String::from("run"), String::from("Run the task"));
        assert_eq!(step.name, "run");
        assert_eq!(step.description, "Run the task");
    }

    #[test]
    fn workflow_step_serde_roundtrip() {
        let step = WorkflowStep::new("send", "Send command to pane");
        let json = serde_json::to_string(&step).unwrap();
        let restored: WorkflowStep = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "send");
        assert_eq!(restored.description, "Send command to pane");
    }

    #[test]
    fn workflow_step_clone() {
        let step = WorkflowStep::new("wait", "Wait for response");
        let cloned = step.clone();
        assert_eq!(cloned.name, step.name);
        assert_eq!(cloned.description, step.description);
    }

    // ========================================================================
    // Workflow trait tests (via MockWorkflow)
    // ========================================================================

    #[test]
    fn workflow_name_and_description() {
        let wf = MockWorkflow::default();
        assert_eq!(wf.name(), "mock_workflow");
        assert_eq!(wf.description(), "A mock workflow for testing");
    }

    #[test]
    fn workflow_handles_matching_detection() {
        let wf = MockWorkflow::default();
        let detection = make_detection("mock.test_event");
        assert!(wf.handles(&detection));
    }

    #[test]
    fn workflow_rejects_non_matching_detection() {
        let wf = MockWorkflow::default();
        let detection = make_detection("other.test_event");
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn workflow_steps_count() {
        let wf = MockWorkflow::default();
        assert_eq!(wf.steps().len(), 3);
        assert_eq!(wf.step_count(), 3);
    }

    #[test]
    fn workflow_step_names() {
        let wf = MockWorkflow::default();
        let steps = wf.steps();
        assert_eq!(steps[0].name, "step_one");
        assert_eq!(steps[1].name, "step_two");
        assert_eq!(steps[2].name, "step_three");
    }

    #[test]
    fn workflow_default_trigger_event_types_empty() {
        let wf = MockWorkflow::default();
        assert!(wf.trigger_event_types().is_empty());
    }

    #[test]
    fn workflow_custom_trigger_event_types() {
        let wf = MockWorkflow {
            trigger_events: &["session.compaction", "usage.limit"],
            ..MockWorkflow::default()
        };
        assert_eq!(wf.trigger_event_types().len(), 2);
        assert_eq!(wf.trigger_event_types()[0], "session.compaction");
    }

    #[test]
    fn workflow_default_trigger_rule_ids_empty() {
        let wf = MockWorkflow::default();
        assert!(wf.trigger_rule_ids().is_empty());
    }

    #[test]
    fn workflow_custom_trigger_rule_ids() {
        let wf = MockWorkflow {
            trigger_rules: &["compaction.detected"],
            ..MockWorkflow::default()
        };
        assert_eq!(wf.trigger_rule_ids(), &["compaction.detected"]);
    }

    #[test]
    fn workflow_default_supported_agent_types_empty() {
        let wf = MockWorkflow::default();
        assert!(wf.supported_agent_types().is_empty());
    }

    #[test]
    fn workflow_custom_supported_agent_types() {
        let wf = MockWorkflow {
            agent_types: &["codex", "claude_code"],
            ..MockWorkflow::default()
        };
        assert_eq!(wf.supported_agent_types().len(), 2);
    }

    #[test]
    fn workflow_default_requires_pane() {
        let wf = MockWorkflow::default();
        assert!(wf.requires_pane());
    }

    #[test]
    fn workflow_no_pane_required() {
        let wf = MockWorkflow {
            needs_pane: false,
            ..MockWorkflow::default()
        };
        assert!(!wf.requires_pane());
    }

    #[test]
    fn workflow_default_no_approval_required() {
        let wf = MockWorkflow::default();
        assert!(!wf.requires_approval());
    }

    #[test]
    fn workflow_requires_approval() {
        let wf = MockWorkflow {
            needs_approval: true,
            ..MockWorkflow::default()
        };
        assert!(wf.requires_approval());
    }

    #[test]
    fn workflow_default_can_abort() {
        let wf = MockWorkflow::default();
        assert!(wf.can_abort());
    }

    #[test]
    fn workflow_not_abortable() {
        let wf = MockWorkflow {
            abortable: false,
            ..MockWorkflow::default()
        };
        assert!(!wf.can_abort());
    }

    #[test]
    fn workflow_default_not_destructive() {
        let wf = MockWorkflow::default();
        assert!(!wf.is_destructive());
    }

    #[test]
    fn workflow_destructive() {
        let wf = MockWorkflow {
            destructive: true,
            ..MockWorkflow::default()
        };
        assert!(wf.is_destructive());
    }

    #[test]
    fn workflow_default_no_dependencies() {
        let wf = MockWorkflow::default();
        assert!(wf.dependencies().is_empty());
    }

    #[test]
    fn workflow_with_dependencies() {
        let wf = MockWorkflow {
            deps: &["prereq_workflow", "setup_workflow"],
            ..MockWorkflow::default()
        };
        assert_eq!(wf.dependencies().len(), 2);
        assert_eq!(wf.dependencies()[0], "prereq_workflow");
    }

    #[test]
    fn workflow_default_is_enabled() {
        let wf = MockWorkflow::default();
        assert!(wf.is_enabled());
    }

    #[test]
    fn workflow_disabled() {
        let wf = MockWorkflow {
            enabled: false,
            ..MockWorkflow::default()
        };
        assert!(!wf.is_enabled());
    }

    // ========================================================================
    // steps_to_plans tests
    // ========================================================================

    #[test]
    fn steps_to_plans_generates_correct_count() {
        let wf = MockWorkflow::default();
        let plans = wf.steps_to_plans(42);
        assert_eq!(plans.len(), 3);
    }

    #[test]
    fn steps_to_plans_sequential_step_numbers() {
        let wf = MockWorkflow::default();
        let plans = wf.steps_to_plans(42);
        assert_eq!(plans[0].step_number, 1);
        assert_eq!(plans[1].step_number, 2);
        assert_eq!(plans[2].step_number, 3);
    }

    #[test]
    fn steps_to_plans_uses_step_description() {
        let wf = MockWorkflow::default();
        let plans = wf.steps_to_plans(42);
        assert_eq!(plans[0].description, "First step");
        assert_eq!(plans[1].description, "Second step");
    }

    #[test]
    fn steps_to_plans_custom_action_type() {
        let wf = MockWorkflow::default();
        let plans = wf.steps_to_plans(99);
        if let crate::plan::StepAction::Custom {
            action_type,
            payload,
        } = &plans[0].action
        {
            assert_eq!(action_type, "workflow_step:step_one");
            assert_eq!(payload["pane_id"], 99);
            assert_eq!(payload["workflow"], "mock_workflow");
            assert_eq!(payload["step_name"], "step_one");
        } else {
            panic!("Expected StepAction::Custom");
        }
    }

    #[test]
    fn steps_to_plans_unique_step_ids() {
        let wf = MockWorkflow::default();
        let plans = wf.steps_to_plans(1);
        let ids: Vec<_> = plans.iter().map(|p| &p.step_id).collect();
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                assert_ne!(ids[i], ids[j], "Step IDs should be unique");
            }
        }
    }

    #[test]
    fn steps_to_plans_different_pane_id_changes_payload() {
        let wf = MockWorkflow::default();
        let plans_a = wf.steps_to_plans(1);
        let plans_b = wf.steps_to_plans(2);
        // Payloads should differ due to different pane_id
        if let (
            crate::plan::StepAction::Custom {
                payload: pa, ..
            },
            crate::plan::StepAction::Custom {
                payload: pb, ..
            },
        ) = (&plans_a[0].action, &plans_b[0].action)
        {
            assert_eq!(pa["pane_id"], 1);
            assert_eq!(pb["pane_id"], 2);
        } else {
            panic!("Expected StepAction::Custom");
        }
    }

    // ========================================================================
    // WorkflowInfo tests
    // ========================================================================

    #[test]
    fn workflow_info_from_default_workflow() {
        let wf = MockWorkflow::default();
        let info = WorkflowInfo::from_workflow(&wf);
        assert_eq!(info.name, "mock_workflow");
        assert_eq!(info.description, "A mock workflow for testing");
        assert!(info.enabled);
        assert!(info.trigger_event_types.is_empty());
        assert!(info.trigger_rule_ids.is_empty());
        assert!(info.agent_types.is_empty());
        assert_eq!(info.step_count, 3);
        assert!(info.requires_pane);
        assert!(!info.requires_approval);
        assert!(info.can_abort);
        assert!(!info.destructive);
        assert!(info.dependencies.is_empty());
    }

    #[test]
    fn workflow_info_from_customized_workflow() {
        let wf = MockWorkflow {
            trigger_events: &["session.exit"],
            trigger_rules: &["exit.detected", "timeout.detected"],
            agent_types: &["codex"],
            needs_pane: true,
            needs_approval: true,
            abortable: false,
            destructive: true,
            deps: &["init_workflow"],
            enabled: false,
        };
        let info = WorkflowInfo::from_workflow(&wf);
        assert!(!info.enabled);
        assert_eq!(info.trigger_event_types, vec!["session.exit"]);
        assert_eq!(
            info.trigger_rule_ids,
            vec!["exit.detected", "timeout.detected"]
        );
        assert_eq!(info.agent_types, vec!["codex"]);
        assert!(info.requires_approval);
        assert!(!info.can_abort);
        assert!(info.destructive);
        assert_eq!(info.dependencies, vec!["init_workflow"]);
    }

    #[test]
    fn workflow_info_serde_roundtrip() {
        let wf = MockWorkflow {
            trigger_events: &["e1"],
            trigger_rules: &["r1"],
            agent_types: &["claude_code"],
            deps: &["dep1"],
            ..MockWorkflow::default()
        };
        let info = WorkflowInfo::from_workflow(&wf);
        let json = serde_json::to_string(&info).unwrap();
        let restored: WorkflowInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, info.name);
        assert_eq!(restored.step_count, info.step_count);
        assert_eq!(restored.trigger_event_types, info.trigger_event_types);
        assert_eq!(restored.trigger_rule_ids, info.trigger_rule_ids);
        assert_eq!(restored.agent_types, info.agent_types);
        assert_eq!(restored.dependencies, info.dependencies);
    }
}
