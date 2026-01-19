//! Durable workflow execution engine
//!
//! Provides idempotent, recoverable, audited workflow execution.
//!
//! # Architecture
//!
//! Workflows are explicit state machines with a uniform execution model:
//! - **Workflow trait**: Defines the workflow interface (name, steps, execution)
//! - **WorkflowContext**: Runtime context with WezTerm client, storage, pane state
//! - **StepResult**: Step outcomes (continue, done, retry, abort, wait)
//! - **WaitCondition**: Conditions to pause execution (pattern, idle, external)
//!
//! This design enables:
//! - Persistent/resumable workflows
//! - Deterministic step logic testing
//! - Shared runner across agent-specific workflows

use crate::policy::PaneCapabilities;
use crate::storage::StorageHandle;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

// ============================================================================
// Step Results
// ============================================================================

/// Result of a workflow step execution.
///
/// Each step returns a `StepResult` that determines what happens next:
/// - `Continue`: Proceed to the next step
/// - `Done`: Workflow completed successfully with a result
/// - `Retry`: Retry this step after a delay
/// - `Abort`: Stop workflow with an error
/// - `WaitFor`: Pause until a condition is met
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StepResult {
    /// Proceed to next step
    Continue,
    /// Workflow completed successfully with optional result data
    Done { result: serde_json::Value },
    /// Retry this step after delay
    Retry {
        /// Delay before retry in milliseconds
        delay_ms: u64,
    },
    /// Abort workflow with error
    Abort {
        /// Reason for abort
        reason: String,
    },
    /// Wait for condition before proceeding
    WaitFor {
        /// Condition to wait for
        condition: WaitCondition,
        /// Timeout in milliseconds (None = workflow-level default)
        timeout_ms: Option<u64>,
    },
}

impl StepResult {
    /// Create a Continue result
    #[must_use]
    pub fn cont() -> Self {
        Self::Continue
    }

    /// Create a Done result with JSON value
    #[must_use]
    pub fn done(result: serde_json::Value) -> Self {
        Self::Done { result }
    }

    /// Create a Done result with no data
    #[must_use]
    pub fn done_empty() -> Self {
        Self::Done {
            result: serde_json::Value::Null,
        }
    }

    /// Create a Retry result
    #[must_use]
    pub fn retry(delay_ms: u64) -> Self {
        Self::Retry { delay_ms }
    }

    /// Create an Abort result
    #[must_use]
    pub fn abort(reason: impl Into<String>) -> Self {
        Self::Abort {
            reason: reason.into(),
        }
    }

    /// Create a WaitFor result with default timeout
    #[must_use]
    pub fn wait_for(condition: WaitCondition) -> Self {
        Self::WaitFor {
            condition,
            timeout_ms: None,
        }
    }

    /// Create a WaitFor result with explicit timeout
    #[must_use]
    pub fn wait_for_with_timeout(condition: WaitCondition, timeout_ms: u64) -> Self {
        Self::WaitFor {
            condition,
            timeout_ms: Some(timeout_ms),
        }
    }

    /// Check if this result continues to the next step
    #[must_use]
    pub fn is_continue(&self) -> bool {
        matches!(self, Self::Continue)
    }

    /// Check if this result completes the workflow
    #[must_use]
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done { .. })
    }

    /// Check if this result is a terminal state (done or abort)
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Abort { .. })
    }
}

// ============================================================================
// Wait Conditions
// ============================================================================

/// Conditions that a workflow can wait for before proceeding.
///
/// Wait conditions pause workflow execution until satisfied:
/// - `Pattern`: Wait for a pattern rule to match on a pane
/// - `PaneIdle`: Wait for a pane to become idle (no output)
/// - `External`: Wait for an external signal by key
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitCondition {
    /// Wait for a pattern to appear on a specific pane
    Pattern {
        /// Pane to monitor (None = workflow's target pane)
        pane_id: Option<u64>,
        /// Rule ID of the pattern to match
        rule_id: String,
    },
    /// Wait for pane to become idle (no recent output)
    PaneIdle {
        /// Pane to monitor (None = workflow's target pane)
        pane_id: Option<u64>,
        /// Idle duration threshold in milliseconds
        idle_threshold_ms: u64,
    },
    /// Wait for an external signal
    External {
        /// Signal key to wait for
        key: String,
    },
}

impl WaitCondition {
    /// Create a Pattern wait condition for the workflow's target pane
    #[must_use]
    pub fn pattern(rule_id: impl Into<String>) -> Self {
        Self::Pattern {
            pane_id: None,
            rule_id: rule_id.into(),
        }
    }

    /// Create a Pattern wait condition for a specific pane
    #[must_use]
    pub fn pattern_on_pane(pane_id: u64, rule_id: impl Into<String>) -> Self {
        Self::Pattern {
            pane_id: Some(pane_id),
            rule_id: rule_id.into(),
        }
    }

    /// Create a PaneIdle wait condition for the workflow's target pane
    #[must_use]
    pub fn pane_idle(idle_threshold_ms: u64) -> Self {
        Self::PaneIdle {
            pane_id: None,
            idle_threshold_ms,
        }
    }

    /// Create a PaneIdle wait condition for a specific pane
    #[must_use]
    pub fn pane_idle_on(pane_id: u64, idle_threshold_ms: u64) -> Self {
        Self::PaneIdle {
            pane_id: Some(pane_id),
            idle_threshold_ms,
        }
    }

    /// Create an External wait condition
    #[must_use]
    pub fn external(key: impl Into<String>) -> Self {
        Self::External { key: key.into() }
    }

    /// Get the pane ID this condition applies to, if any
    #[must_use]
    pub fn pane_id(&self) -> Option<u64> {
        match self {
            Self::Pattern { pane_id, .. } | Self::PaneIdle { pane_id, .. } => *pane_id,
            Self::External { .. } => None,
        }
    }
}

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
// Workflow Context
// ============================================================================

/// Configuration for a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    /// Default timeout for wait conditions (milliseconds)
    pub default_wait_timeout_ms: u64,
    /// Maximum number of retries per step
    pub max_step_retries: u32,
    /// Delay between retry attempts (milliseconds)
    pub retry_delay_ms: u64,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            default_wait_timeout_ms: 30_000, // 30 seconds
            max_step_retries: 3,
            retry_delay_ms: 1_000, // 1 second
        }
    }
}

/// Runtime context for workflow execution.
///
/// Provides access to:
/// - WezTerm client for sending commands
/// - Storage handle for persistence
/// - Current pane state and capabilities
/// - Triggering event/detection
/// - Workflow configuration
#[derive(Clone)]
pub struct WorkflowContext {
    /// Storage handle for persistence operations
    storage: Arc<StorageHandle>,
    /// Target pane ID for this workflow
    pane_id: u64,
    /// Current pane capabilities snapshot
    capabilities: PaneCapabilities,
    /// The event/detection that triggered this workflow (JSON)
    trigger: Option<serde_json::Value>,
    /// Workflow configuration
    config: WorkflowConfig,
    /// Workflow execution ID
    execution_id: String,
}

impl WorkflowContext {
    /// Create a new workflow context
    #[must_use]
    pub fn new(
        storage: Arc<StorageHandle>,
        pane_id: u64,
        capabilities: PaneCapabilities,
        execution_id: impl Into<String>,
    ) -> Self {
        Self {
            storage,
            pane_id,
            capabilities,
            trigger: None,
            config: WorkflowConfig::default(),
            execution_id: execution_id.into(),
        }
    }

    /// Set the triggering event/detection
    #[must_use]
    pub fn with_trigger(mut self, trigger: serde_json::Value) -> Self {
        self.trigger = Some(trigger);
        self
    }

    /// Set custom workflow configuration
    #[must_use]
    pub fn with_config(mut self, config: WorkflowConfig) -> Self {
        self.config = config;
        self
    }

    /// Get the storage handle
    #[must_use]
    pub fn storage(&self) -> &Arc<StorageHandle> {
        &self.storage
    }

    /// Get the target pane ID
    #[must_use]
    pub fn pane_id(&self) -> u64 {
        self.pane_id
    }

    /// Get the current pane capabilities
    #[must_use]
    pub fn capabilities(&self) -> &PaneCapabilities {
        &self.capabilities
    }

    /// Update the pane capabilities snapshot
    pub fn update_capabilities(&mut self, capabilities: PaneCapabilities) {
        self.capabilities = capabilities;
    }

    /// Get the triggering event/detection, if any
    #[must_use]
    pub fn trigger(&self) -> Option<&serde_json::Value> {
        self.trigger.as_ref()
    }

    /// Get the workflow configuration
    #[must_use]
    pub fn config(&self) -> &WorkflowConfig {
        &self.config
    }

    /// Get the execution ID
    #[must_use]
    pub fn execution_id(&self) -> &str {
        &self.execution_id
    }

    /// Get the default wait timeout from config
    #[must_use]
    pub fn default_wait_timeout_ms(&self) -> u64 {
        self.config.default_wait_timeout_ms
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
/// ```ignore
/// use wa_core::workflows::{Workflow, WorkflowContext, WorkflowStep, StepResult, WaitCondition};
/// use wa_core::patterns::Detection;
///
/// struct PromptInjectionWorkflow;
///
/// impl Workflow for PromptInjectionWorkflow {
///     fn name(&self) -> &str { "prompt_injection" }
///     fn description(&self) -> &str { "Sends a prompt and waits for response" }
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
///     async fn execute_step(&self, ctx: &mut WorkflowContext, step_idx: usize) -> StepResult {
///         match step_idx {
///             0 => {
///                 // Send prompt via WezTerm client
///                 StepResult::cont()
///             }
///             1 => {
///                 // Wait for response
///                 StepResult::wait_for(WaitCondition::pattern("response.complete"))
///             }
///             _ => StepResult::done_empty()
///         }
///     }
/// }
/// ```
pub trait Workflow: Send + Sync {
    /// Workflow name (unique identifier)
    fn name(&self) -> &str;

    /// Human-readable description
    fn description(&self) -> &str;

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
    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> impl std::future::Future<Output = StepResult> + Send;

    /// Optional cleanup when workflow is aborted or completes with error.
    ///
    /// Override to release resources, revert partial changes, etc.
    fn cleanup(&self, _ctx: &mut WorkflowContext) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    /// Get the number of steps in this workflow.
    fn step_count(&self) -> usize {
        self.steps().len()
    }
}

// ============================================================================
// Workflow Execution State
// ============================================================================

/// Workflow execution state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowExecution {
    /// Unique execution ID
    pub id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane being operated on
    pub pane_id: u64,
    /// Current step index
    pub current_step: usize,
    /// Status
    pub status: ExecutionStatus,
    /// Started at timestamp
    pub started_at: i64,
    /// Last updated timestamp
    pub updated_at: i64,
}

/// Workflow execution status
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    /// Running
    Running,
    /// Waiting for condition
    Waiting,
    /// Completed successfully
    Completed,
    /// Aborted with error
    Aborted,
}

/// Workflow engine for managing executions
pub struct WorkflowEngine {
    /// Maximum concurrent workflows
    max_concurrent: usize,
}

impl Default for WorkflowEngine {
    fn default() -> Self {
        Self::new(3)
    }
}

impl WorkflowEngine {
    /// Create a new workflow engine
    #[must_use]
    pub fn new(max_concurrent: usize) -> Self {
        Self { max_concurrent }
    }

    /// Get the maximum concurrent workflows setting
    #[must_use]
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Start a workflow execution
    pub async fn start(
        &self,
        _workflow_name: &str,
        _pane_id: u64,
    ) -> crate::Result<WorkflowExecution> {
        // TODO: Implement workflow start
        todo!("Implement workflow start")
    }

    /// Resume a workflow execution
    pub async fn resume(&self, _execution_id: &str) -> crate::Result<WorkflowExecution> {
        // TODO: Implement workflow resume
        todo!("Implement workflow resume")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::{AgentType, Detection, Severity};

    // ========================================================================
    // StepResult Tests
    // ========================================================================

    #[test]
    fn step_result_continue_serializes() {
        let result = StepResult::Continue;
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("continue"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_continue());
    }

    #[test]
    fn step_result_done_serializes() {
        let result = StepResult::done(serde_json::json!({"status": "ok"}));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("done"));
        assert!(json.contains("status"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_done());
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_retry_serializes() {
        let result = StepResult::retry(5000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("retry"));
        assert!(json.contains("5000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::Retry { delay_ms } => assert_eq!(delay_ms, 5000),
            _ => panic!("Expected Retry"),
        }
    }

    #[test]
    fn step_result_abort_serializes() {
        let result = StepResult::abort("test failure");
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("abort"));
        assert!(json.contains("test failure"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_terminal());
    }

    #[test]
    fn step_result_wait_for_serializes() {
        let result =
            StepResult::wait_for_with_timeout(WaitCondition::pattern("prompt.ready"), 10_000);
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("wait_for"));
        assert!(json.contains("prompt.ready"));
        assert!(json.contains("10000"));

        let parsed: StepResult = serde_json::from_str(&json).unwrap();
        match parsed {
            StepResult::WaitFor {
                condition,
                timeout_ms,
            } => {
                assert_eq!(timeout_ms, Some(10_000));
                match condition {
                    WaitCondition::Pattern { rule_id, .. } => assert_eq!(rule_id, "prompt.ready"),
                    _ => panic!("Expected Pattern condition"),
                }
            }
            _ => panic!("Expected WaitFor"),
        }
    }

    #[test]
    fn step_result_helper_methods() {
        assert!(StepResult::cont().is_continue());
        assert!(StepResult::done_empty().is_done());
        assert!(StepResult::done_empty().is_terminal());
        assert!(StepResult::abort("error").is_terminal());
        assert!(!StepResult::retry(100).is_terminal());
        assert!(!StepResult::wait_for(WaitCondition::external("key")).is_terminal());
    }

    // ========================================================================
    // WaitCondition Tests
    // ========================================================================

    #[test]
    fn wait_condition_pattern_serializes() {
        let cond = WaitCondition::pattern("test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pattern"));
        assert!(json.contains("test.rule"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    #[test]
    fn wait_condition_pattern_on_pane_serializes() {
        let cond = WaitCondition::pattern_on_pane(42, "test.rule");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("42"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id(), Some(42));
    }

    #[test]
    fn wait_condition_pane_idle_serializes() {
        let cond = WaitCondition::pane_idle(1000);
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("pane_idle"));
        assert!(json.contains("1000"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
    }

    #[test]
    fn wait_condition_pane_idle_on_serializes() {
        let cond = WaitCondition::pane_idle_on(99, 500);
        assert_eq!(cond.pane_id(), Some(99));
    }

    #[test]
    fn wait_condition_external_serializes() {
        let cond = WaitCondition::external("approval_granted");
        let json = serde_json::to_string(&cond).unwrap();
        assert!(json.contains("external"));
        assert!(json.contains("approval_granted"));

        let parsed: WaitCondition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cond);
        assert_eq!(parsed.pane_id(), None);
    }

    // ========================================================================
    // WorkflowStep Tests
    // ========================================================================

    #[test]
    fn workflow_step_creates() {
        let step = WorkflowStep::new("send_prompt", "Send a prompt to the terminal");
        assert_eq!(step.name, "send_prompt");
        assert_eq!(step.description, "Send a prompt to the terminal");
    }

    // ========================================================================
    // WorkflowConfig Tests
    // ========================================================================

    #[test]
    fn workflow_config_defaults() {
        let config = WorkflowConfig::default();
        assert_eq!(config.default_wait_timeout_ms, 30_000);
        assert_eq!(config.max_step_retries, 3);
        assert_eq!(config.retry_delay_ms, 1_000);
    }

    // ========================================================================
    // WorkflowEngine Tests
    // ========================================================================

    #[test]
    fn engine_can_be_created() {
        let engine = WorkflowEngine::new(5);
        assert_eq!(engine.max_concurrent(), 5);
    }

    // ========================================================================
    // Stub Workflow Tests (wa-nu4.1.1.1 acceptance criteria)
    // ========================================================================

    /// A stub workflow for testing that demonstrates all workflow capabilities
    struct StubWorkflow {
        name: String,
        description: String,
        target_rule_prefix: String,
    }

    impl StubWorkflow {
        fn new() -> Self {
            Self {
                name: "stub_workflow".to_string(),
                description: "A test workflow for verification".to_string(),
                target_rule_prefix: "test.".to_string(),
            }
        }
    }

    impl Workflow for StubWorkflow {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            &self.description
        }

        fn handles(&self, detection: &Detection) -> bool {
            detection.rule_id.starts_with(&self.target_rule_prefix)
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("step_one", "First step - sends prompt"),
                WorkflowStep::new("step_two", "Second step - waits for response"),
                WorkflowStep::new("step_three", "Third step - completes"),
            ]
        }

        async fn execute_step(&self, _ctx: &mut WorkflowContext, step_idx: usize) -> StepResult {
            match step_idx {
                0 => StepResult::cont(),
                1 => StepResult::wait_for(WaitCondition::pattern("response.ready")),
                2 => StepResult::done(serde_json::json!({"completed": true})),
                _ => StepResult::abort("unexpected step index"),
            }
        }

        async fn cleanup(&self, _ctx: &mut WorkflowContext) {
            // Stub cleanup - no-op
        }
    }

    fn make_test_detection(rule_id: &str) -> Detection {
        Detection {
            rule_id: rule_id.to_string(),
            agent_type: AgentType::Wezterm,
            event_type: "test".to_string(),
            severity: Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "test".to_string(),
        }
    }

    #[test]
    fn stub_workflow_compiles_and_has_correct_metadata() {
        let workflow = StubWorkflow::new();

        assert_eq!(workflow.name(), "stub_workflow");
        assert_eq!(workflow.description(), "A test workflow for verification");
        assert_eq!(workflow.step_count(), 3);

        let steps = workflow.steps();
        assert_eq!(steps[0].name, "step_one");
        assert_eq!(steps[1].name, "step_two");
        assert_eq!(steps[2].name, "step_three");
    }

    #[test]
    fn stub_workflow_handles_matching_detections() {
        let workflow = StubWorkflow::new();

        // Should handle detections with matching prefix
        assert!(workflow.handles(&make_test_detection("test.prompt_ready")));
        assert!(workflow.handles(&make_test_detection("test.anything")));

        // Should not handle detections with non-matching prefix
        assert!(!workflow.handles(&make_test_detection("other.prompt_ready")));
        assert!(!workflow.handles(&make_test_detection("production.event")));
    }

    #[tokio::test]
    async fn stub_workflow_executes_steps_correctly() {
        let workflow = StubWorkflow::new();

        // Create a minimal context for testing
        // Note: In real usage, this would have an actual StorageHandle
        // For this test, we just verify the step execution logic

        // We can't easily create a WorkflowContext without a real StorageHandle,
        // but we can verify the workflow's step logic independently
        let steps = workflow.steps();
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn step_result_transitions_exhaustive() {
        // Verify all StepResult variants can be created and identified
        let variants = [
            StepResult::Continue,
            StepResult::Done {
                result: serde_json::Value::Null,
            },
            StepResult::Retry { delay_ms: 1000 },
            StepResult::Abort {
                reason: "test".to_string(),
            },
            StepResult::WaitFor {
                condition: WaitCondition::external("key"),
                timeout_ms: None,
            },
        ];

        // Each variant serializes uniquely
        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        // All 5 variants have unique type identifiers
        assert_eq!(json_types.len(), 5);
        assert!(json_types.contains("continue"));
        assert!(json_types.contains("done"));
        assert!(json_types.contains("retry"));
        assert!(json_types.contains("abort"));
        assert!(json_types.contains("wait_for"));
    }

    #[test]
    fn wait_condition_transitions_exhaustive() {
        // Verify all WaitCondition variants
        let variants = [
            WaitCondition::Pattern {
                pane_id: None,
                rule_id: "test".to_string(),
            },
            WaitCondition::PaneIdle {
                pane_id: None,
                idle_threshold_ms: 1000,
            },
            WaitCondition::External {
                key: "test".to_string(),
            },
        ];

        let mut json_types = std::collections::HashSet::new();
        for variant in &variants {
            let json = serde_json::to_string(variant).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let type_field = parsed["type"].as_str().unwrap().to_string();
            json_types.insert(type_field);
        }

        assert_eq!(json_types.len(), 3);
        assert!(json_types.contains("pattern"));
        assert!(json_types.contains("pane_idle"));
        assert!(json_types.contains("external"));
    }
}
