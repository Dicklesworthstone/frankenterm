//! Swarm-native pipeline runtime with hooks and recovery policies.
//!
//! Provides multi-step automation pipelines for agent fleet orchestration.
//! Builds on the existing workflows infrastructure and integrates with
//! swarm_work_queue, swarm_scheduler, and mission_agent_mail.
//!
//! # Key abstractions
//!
//! - [`PipelineDefinition`]: Declarative multi-step pipeline with DAG ordering
//! - [`PipelineStep`]: Individual step with preconditions, timeout, recovery
//! - [`RecoveryPolicy`]: Exponential backoff, circuit breaker, fallback chain
//! - [`CompensatingAction`]: Undo/rollback actions for partial failure
//! - [`WorkflowHook`]: Pre/post step and lifecycle hooks
//! - [`HookRegistry`]: Centralized hook management with ordering
//! - [`PipelineExecutor`]: Runs pipelines with hook dispatch and recovery
//!
//! # Architecture
//!
//! ```text
//! PipelineDefinition → PipelineExecutor → StepRunner
//!                          ↓                  ↓
//!                     HookRegistry      RecoveryPolicy
//!                          ↓                  ↓
//!                    WorkflowHook[]     CompensatingAction[]
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Hook system
// ---------------------------------------------------------------------------

/// Lifecycle phase during which a hook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookPhase {
    /// Before the pipeline starts (after validation).
    PipelineStart,
    /// After the pipeline completes (success or failure).
    PipelineEnd,
    /// Before a step begins execution.
    PreStep,
    /// After a step completes (success, failure, or skip).
    PostStep,
    /// Before a recovery attempt.
    PreRecovery,
    /// After a recovery attempt.
    PostRecovery,
    /// Before a compensating action runs.
    PreCompensation,
    /// After a compensating action runs.
    PostCompensation,
}

impl fmt::Display for HookPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PipelineStart => write!(f, "pipeline.start"),
            Self::PipelineEnd => write!(f, "pipeline.end"),
            Self::PreStep => write!(f, "step.pre"),
            Self::PostStep => write!(f, "step.post"),
            Self::PreRecovery => write!(f, "recovery.pre"),
            Self::PostRecovery => write!(f, "recovery.post"),
            Self::PreCompensation => write!(f, "compensation.pre"),
            Self::PostCompensation => write!(f, "compensation.post"),
        }
    }
}

/// Context passed to hooks for inspection and decision-making.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    /// Pipeline execution identifier.
    pub execution_id: String,
    /// Pipeline name.
    pub pipeline_name: String,
    /// Current step index (if step-level hook).
    pub step_index: Option<usize>,
    /// Current step label (if step-level hook).
    pub step_label: Option<String>,
    /// Elapsed time in milliseconds since pipeline start.
    pub elapsed_ms: u64,
    /// Number of steps completed so far.
    pub steps_completed: usize,
    /// Total steps in the pipeline.
    pub total_steps: usize,
    /// Last step result (if PostStep hook).
    pub last_result: Option<StepOutcome>,
    /// Arbitrary metadata carried through the pipeline.
    pub metadata: HashMap<String, String>,
}

/// Outcome of a hook invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookOutcome {
    /// Continue normal execution.
    Continue,
    /// Skip the current step (PreStep only).
    SkipStep,
    /// Abort the pipeline with a reason.
    Abort { reason: String },
    /// Inject metadata into the pipeline context.
    InjectMetadata { key: String, value: String },
}

/// A registered hook with ordering priority and phase filter.
#[derive(Debug, Clone)]
pub struct HookRegistration {
    /// Unique name for this hook.
    pub name: String,
    /// Which phases this hook fires on.
    pub phases: HashSet<HookPhase>,
    /// Lower priority runs first (default: 100).
    pub priority: u32,
    /// Whether the hook is currently enabled.
    pub enabled: bool,
    /// The hook implementation.
    pub handler: HookHandler,
}

/// Hook handler function type.
///
/// Using an enum rather than a trait to keep things simple and serializable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookHandler {
    /// Log a message at the specified level.
    Log { level: LogLevel, template: String },
    /// Emit a telemetry counter.
    Telemetry { counter_name: String },
    /// Validate a precondition; abort if false.
    Precondition { check: PreconditionCheck },
    /// Inject fixed metadata.
    Metadata { key: String, value: String },
    /// Notify via the mission Agent Mail system.
    AgentMailNotify { subject_template: String },
    /// Custom hook identified by a tag (for testing/extension).
    Custom { tag: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// Precondition checks that can gate step execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PreconditionCheck {
    /// Require a metadata key to be present.
    MetadataPresent { key: String },
    /// Require a metadata key to equal a value.
    MetadataEquals { key: String, value: String },
    /// Require fewer than N failures in the pipeline so far.
    MaxFailures { threshold: u32 },
    /// Require elapsed time to be under a limit.
    TimeLimit { max_ms: u64 },
}

/// Centralized hook registry with ordered dispatch.
#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    hooks: Vec<HookRegistration>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self { hooks: Vec::new() }
    }

    /// Register a new hook. Returns the hook name.
    pub fn register(&mut self, hook: HookRegistration) -> String {
        let name = hook.name.clone();
        self.hooks.push(hook);
        // Keep sorted by priority (stable sort preserves insertion order for ties).
        self.hooks.sort_by_key(|h| h.priority);
        name
    }

    /// Remove a hook by name. Returns true if found.
    pub fn unregister(&mut self, name: &str) -> bool {
        let before = self.hooks.len();
        self.hooks.retain(|h| h.name != name);
        self.hooks.len() < before
    }

    /// Enable or disable a hook by name.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> bool {
        for hook in &mut self.hooks {
            if hook.name == name {
                hook.enabled = enabled;
                return true;
            }
        }
        false
    }

    /// Dispatch all matching hooks for a phase. Returns outcomes in priority order.
    pub fn dispatch(&self, phase: HookPhase, context: &HookContext) -> Vec<(String, HookOutcome)> {
        let mut outcomes = Vec::new();
        for hook in &self.hooks {
            if !hook.enabled || !hook.phases.contains(&phase) {
                continue;
            }
            let outcome = execute_hook_handler(&hook.handler, context);
            outcomes.push((hook.name.clone(), outcome));
        }
        outcomes
    }

    /// Number of registered hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// List all hook names.
    pub fn hook_names(&self) -> Vec<&str> {
        self.hooks.iter().map(|h| h.name.as_str()).collect()
    }
}

fn execute_hook_handler(handler: &HookHandler, context: &HookContext) -> HookOutcome {
    match handler {
        HookHandler::Log { .. } => HookOutcome::Continue,
        HookHandler::Telemetry { .. } => HookOutcome::Continue,
        HookHandler::Precondition { check } => evaluate_precondition(check, context),
        HookHandler::Metadata { key, value } => HookOutcome::InjectMetadata {
            key: key.clone(),
            value: value.clone(),
        },
        HookHandler::AgentMailNotify { .. } => HookOutcome::Continue,
        HookHandler::Custom { .. } => HookOutcome::Continue,
    }
}

fn evaluate_precondition(check: &PreconditionCheck, context: &HookContext) -> HookOutcome {
    match check {
        PreconditionCheck::MetadataPresent { key } => {
            if context.metadata.contains_key(key) {
                HookOutcome::Continue
            } else {
                HookOutcome::Abort {
                    reason: format!("precondition failed: metadata key '{key}' not present"),
                }
            }
        }
        PreconditionCheck::MetadataEquals { key, value } => {
            if context.metadata.get(key).is_some_and(|v| v == value) {
                HookOutcome::Continue
            } else {
                HookOutcome::Abort {
                    reason: format!("precondition failed: metadata '{key}' != '{value}'"),
                }
            }
        }
        PreconditionCheck::MaxFailures { threshold } => {
            // We encode failure count in metadata under "pipeline.failure_count".
            let count: u32 = context
                .metadata
                .get("pipeline.failure_count")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if count < *threshold {
                HookOutcome::Continue
            } else {
                HookOutcome::Abort {
                    reason: format!(
                        "precondition failed: {count} failures >= threshold {threshold}"
                    ),
                }
            }
        }
        PreconditionCheck::TimeLimit { max_ms } => {
            if context.elapsed_ms <= *max_ms {
                HookOutcome::Continue
            } else {
                HookOutcome::Abort {
                    reason: format!(
                        "precondition failed: elapsed {}ms > limit {}ms",
                        context.elapsed_ms, max_ms
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery policies
// ---------------------------------------------------------------------------

/// Backoff strategy for retries.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BackoffStrategy {
    /// Fixed delay between retries.
    Fixed { delay_ms: u64 },
    /// Exponential backoff with base and optional jitter.
    Exponential {
        base_ms: u64,
        multiplier: f64,
        max_delay_ms: u64,
    },
    /// Linear backoff (delay increases by a fixed increment).
    Linear {
        initial_ms: u64,
        increment_ms: u64,
        max_delay_ms: u64,
    },
}

impl BackoffStrategy {
    /// Compute the delay for a given attempt (0-indexed).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let ms = match self {
            Self::Fixed { delay_ms } => *delay_ms,
            Self::Exponential {
                base_ms,
                multiplier,
                max_delay_ms,
            } => {
                let delay = (*base_ms as f64) * multiplier.powi(attempt as i32);
                (delay as u64).min(*max_delay_ms)
            }
            Self::Linear {
                initial_ms,
                increment_ms,
                max_delay_ms,
            } => {
                let delay = initial_ms + (increment_ms * u64::from(attempt));
                delay.min(*max_delay_ms)
            }
        };
        Duration::from_millis(ms)
    }
}

/// Circuit breaker state for failure containment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CircuitState {
    /// Normal operation; failures are tracked.
    Closed,
    /// Failures exceeded threshold; rejecting requests.
    Open { opened_at_ms: u64 },
    /// Testing whether the system has recovered.
    HalfOpen { attempt_count: u32 },
}

/// Circuit breaker configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Number of failures before opening the circuit.
    pub failure_threshold: u32,
    /// How long the circuit stays open before transitioning to half-open.
    pub reset_timeout_ms: u64,
    /// Number of successful probes needed to close from half-open.
    pub success_threshold: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            reset_timeout_ms: 30_000,
            success_threshold: 2,
        }
    }
}

/// Circuit breaker for step/pipeline failure containment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreaker {
    pub config: CircuitBreakerConfig,
    pub state: CircuitState,
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub total_failures: u64,
    pub total_successes: u64,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            state: CircuitState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            total_failures: 0,
            total_successes: 0,
        }
    }

    /// Check if the circuit allows a request at the given timestamp.
    pub fn allow_request(&self, now_ms: u64) -> bool {
        match &self.state {
            CircuitState::Closed => true,
            CircuitState::Open { opened_at_ms } => {
                now_ms.saturating_sub(*opened_at_ms) >= self.config.reset_timeout_ms
            }
            CircuitState::HalfOpen { .. } => true,
        }
    }

    /// Check if a request is allowed and advance state for timeout-based probes.
    pub fn allow_request_and_advance(&mut self, now_ms: u64) -> bool {
        match self.state {
            CircuitState::Closed | CircuitState::HalfOpen { .. } => true,
            CircuitState::Open { opened_at_ms } => {
                if now_ms.saturating_sub(opened_at_ms) >= self.config.reset_timeout_ms {
                    self.state = CircuitState::HalfOpen { attempt_count: 0 };
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a success.
    pub fn record_success(&mut self, now_ms: u64) {
        self.total_successes += 1;
        self.consecutive_failures = 0;
        self.consecutive_successes += 1;

        match &self.state {
            CircuitState::HalfOpen { .. } => {
                if self.consecutive_successes >= self.config.success_threshold {
                    self.state = CircuitState::Closed;
                    self.consecutive_successes = 0;
                }
            }
            CircuitState::Open { opened_at_ms } => {
                if now_ms.saturating_sub(*opened_at_ms) >= self.config.reset_timeout_ms {
                    self.state = CircuitState::HalfOpen { attempt_count: 1 };
                }
            }
            CircuitState::Closed => {}
        }
    }

    /// Record a failure.
    pub fn record_failure(&mut self, now_ms: u64) {
        self.total_failures += 1;
        self.consecutive_failures += 1;
        self.consecutive_successes = 0;

        match &self.state {
            CircuitState::Closed => {
                if self.consecutive_failures >= self.config.failure_threshold {
                    self.state = CircuitState::Open {
                        opened_at_ms: now_ms,
                    };
                }
            }
            CircuitState::HalfOpen { .. } => {
                self.state = CircuitState::Open {
                    opened_at_ms: now_ms,
                };
            }
            CircuitState::Open { .. } => {
                self.state = CircuitState::Open {
                    opened_at_ms: now_ms,
                };
            }
        }
    }

    /// Reset the circuit breaker.
    pub fn reset(&mut self) {
        self.state = CircuitState::Closed;
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
    }
}

/// Recovery policy for a step or pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryPolicy {
    /// Maximum retry attempts before giving up.
    pub max_retries: u32,
    /// Backoff strategy between retries.
    pub backoff: BackoffStrategy,
    /// Optional circuit breaker for failure containment.
    pub circuit_breaker: Option<CircuitBreakerConfig>,
    /// Fallback step labels to try if this step fails permanently.
    pub fallback_chain: Vec<String>,
    /// Whether to run compensating actions on permanent failure.
    pub compensate_on_failure: bool,
    /// Error patterns that should NOT be retried (permanent failures).
    pub non_retryable_errors: Vec<String>,
    /// Timeout for the entire recovery process.
    pub recovery_timeout_ms: Option<u64>,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            backoff: BackoffStrategy::Exponential {
                base_ms: 1000,
                multiplier: 2.0,
                max_delay_ms: 30_000,
            },
            circuit_breaker: None,
            fallback_chain: Vec::new(),
            compensate_on_failure: true,
            non_retryable_errors: Vec::new(),
            recovery_timeout_ms: None,
        }
    }
}

impl RecoveryPolicy {
    /// Check if an error message matches a non-retryable pattern.
    pub fn is_non_retryable(&self, error: &str) -> bool {
        self.non_retryable_errors
            .iter()
            .any(|pat| error.contains(pat))
    }
}

// ---------------------------------------------------------------------------
// Compensating actions
// ---------------------------------------------------------------------------

/// A compensating (undo) action that runs when a step or pipeline fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompensatingAction {
    /// Human-readable label for the compensation.
    pub label: String,
    /// Which step this compensates (by label).
    pub compensates_step: String,
    /// The action to perform.
    pub action: CompensationKind,
    /// Maximum time for the compensation to complete.
    pub timeout_ms: u64,
    /// Whether failure of this compensation is fatal to the rollback.
    pub required: bool,
}

/// Types of compensating actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompensationKind {
    /// Send a cancel/abort command.
    SendCommand { command: String },
    /// Restore a checkpoint by ID.
    RestoreCheckpoint { checkpoint_id: String },
    /// Notify an agent about the rollback.
    NotifyAgent { agent_name: String, message: String },
    /// Log the compensation (no-op for testing).
    Log { message: String },
    /// Custom compensation identified by tag.
    Custom {
        tag: String,
        params: HashMap<String, String>,
    },
}

// ---------------------------------------------------------------------------
// Pipeline definitions
// ---------------------------------------------------------------------------

/// Status of a pipeline step execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed { error: String },
    Skipped { reason: String },
    Compensated,
}

/// Outcome of a step execution (for hooks and reporting).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub step_index: usize,
    pub step_label: String,
    pub status: StepStatus,
    pub attempts: u32,
    pub duration_ms: u64,
    pub recovery_attempts: u32,
    pub compensations_run: u32,
}

/// A single step in a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStep {
    /// Unique label within the pipeline.
    pub label: String,
    /// Human-readable description.
    pub description: String,
    /// Step action to perform.
    pub action: StepAction,
    /// Labels of steps that must complete before this one.
    pub depends_on: Vec<String>,
    /// Recovery policy for this step.
    pub recovery: RecoveryPolicy,
    /// Compensating action if this step needs to be undone.
    pub compensation: Option<CompensatingAction>,
    /// Maximum execution time for this step.
    pub timeout_ms: u64,
    /// Whether this step can be skipped on non-fatal failure.
    pub optional: bool,
    /// Precondition metadata keys that must be present.
    pub preconditions: Vec<String>,
}

/// Actions a pipeline step can perform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StepAction {
    /// Dispatch a work item to the swarm work queue.
    DispatchWork { work_item_id: String, priority: u32 },
    /// Send a coordination message via Agent Mail.
    SendMessage {
        subject: String,
        body: String,
        recipients: Vec<String>,
    },
    /// Wait for a condition to be met.
    WaitForCondition {
        condition: PipelineCondition,
        poll_interval_ms: u64,
    },
    /// Execute a sub-pipeline.
    SubPipeline { pipeline_name: String },
    /// Run a shell command (policy-gated).
    Command { command: String, args: Vec<String> },
    /// Take a durable-state checkpoint.
    Checkpoint { label: String },
    /// No-op step (for testing or synchronization points).
    Noop,
}

/// Conditions that a pipeline can wait for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PipelineCondition {
    /// Work item reaches a target status.
    WorkItemStatus {
        work_item_id: String,
        target_status: String,
    },
    /// A metadata key reaches a target value.
    MetadataEquals { key: String, value: String },
    /// Elapsed time exceeds a threshold.
    Timeout { after_ms: u64 },
    /// All specified steps have completed.
    AllStepsComplete { step_labels: Vec<String> },
}

/// Overall pipeline status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineStatus {
    /// Not yet started.
    Pending,
    /// Currently executing steps.
    Running,
    /// All steps completed successfully.
    Succeeded,
    /// Pipeline failed (some required steps failed).
    Failed { reason: String },
    /// Pipeline was explicitly aborted.
    Aborted { reason: String },
    /// Pipeline is rolling back via compensating actions.
    Compensating,
}

/// A complete pipeline definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineDefinition {
    /// Unique pipeline name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Ordered steps (execution follows dependency graph).
    pub steps: Vec<PipelineStep>,
    /// Global recovery policy (applied to steps without their own).
    pub default_recovery: RecoveryPolicy,
    /// Pipeline-level timeout.
    pub timeout_ms: u64,
    /// Whether to run compensating actions on failure.
    pub compensate_on_failure: bool,
    /// Pipeline-level metadata.
    pub metadata: HashMap<String, String>,
}

impl PipelineDefinition {
    /// Validate the pipeline definition.
    pub fn validate(&self) -> Result<(), PipelineError> {
        if self.name.is_empty() {
            return Err(PipelineError::ValidationFailed {
                reason: "pipeline name must not be empty".to_string(),
            });
        }
        if self.steps.is_empty() {
            return Err(PipelineError::ValidationFailed {
                reason: "pipeline must have at least one step".to_string(),
            });
        }

        // Check for duplicate labels.
        let mut seen = HashSet::new();
        for step in &self.steps {
            if step.label.is_empty() {
                return Err(PipelineError::ValidationFailed {
                    reason: "step labels must not be empty".to_string(),
                });
            }
            if !seen.insert(&step.label) {
                return Err(PipelineError::ValidationFailed {
                    reason: format!("duplicate step label: {}", step.label),
                });
            }
        }

        // Check dependency references.
        let label_set: HashSet<&str> = self.steps.iter().map(|s| s.label.as_str()).collect();
        for step in &self.steps {
            for dep in &step.depends_on {
                if !label_set.contains(dep.as_str()) {
                    return Err(PipelineError::ValidationFailed {
                        reason: format!("step '{}' depends on unknown step '{}'", step.label, dep),
                    });
                }
                if dep == &step.label {
                    return Err(PipelineError::ValidationFailed {
                        reason: format!("step '{}' depends on itself", step.label),
                    });
                }
            }
        }

        // Check for cycles using topological sort.
        if self.topological_order().is_err() {
            return Err(PipelineError::ValidationFailed {
                reason: "dependency cycle detected in pipeline steps".to_string(),
            });
        }

        Ok(())
    }

    /// Compute a topological ordering of steps based on dependencies.
    pub fn topological_order(&self) -> Result<Vec<usize>, PipelineError> {
        let label_to_index: HashMap<&str, usize> = self
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| (s.label.as_str(), i))
            .collect();

        let n = self.steps.len();
        let mut in_degree = vec![0u32; n];
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];

        for (i, step) in self.steps.iter().enumerate() {
            for dep in &step.depends_on {
                if let Some(&dep_idx) = label_to_index.get(dep.as_str()) {
                    adj[dep_idx].push(i);
                    in_degree[i] += 1;
                }
            }
        }

        let mut queue: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
        queue.sort_unstable(); // deterministic ordering for zero-indegree roots
        let mut order = Vec::with_capacity(n);

        while let Some(node) = queue.first().copied() {
            queue.remove(0);
            order.push(node);
            let mut next_nodes: Vec<usize> = Vec::new();
            for &neighbor in &adj[node] {
                in_degree[neighbor] -= 1;
                if in_degree[neighbor] == 0 {
                    next_nodes.push(neighbor);
                }
            }
            next_nodes.sort_unstable();
            queue.extend(next_nodes);
        }

        if order.len() == n {
            Ok(order)
        } else {
            Err(PipelineError::DependencyCycle)
        }
    }

    /// Get steps that are ready to execute given a set of completed step labels.
    pub fn ready_steps(&self, completed: &HashSet<String>) -> Vec<usize> {
        self.steps
            .iter()
            .enumerate()
            .filter(|(_, step)| {
                !completed.contains(&step.label)
                    && step.depends_on.iter().all(|d| completed.contains(d))
            })
            .map(|(i, _)| i)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Pipeline execution
// ---------------------------------------------------------------------------

/// Snapshot of a pipeline execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineExecution {
    /// Unique execution identifier.
    pub execution_id: String,
    /// Pipeline name.
    pub pipeline_name: String,
    /// Current overall status.
    pub status: PipelineStatus,
    /// Per-step outcomes.
    pub step_outcomes: BTreeMap<usize, StepOutcome>,
    /// Pipeline-level metadata (accumulated from hooks and steps).
    pub metadata: HashMap<String, String>,
    /// When the execution started (epoch ms).
    pub started_at_ms: u64,
    /// When the execution ended (epoch ms), if finished.
    pub ended_at_ms: Option<u64>,
    /// Total failure count across all steps.
    pub total_failures: u32,
    /// Compensating actions that were executed.
    pub compensations_executed: Vec<String>,
}

/// Executor for running pipeline definitions with hook dispatch and recovery.
#[derive(Debug)]
pub struct PipelineExecutor {
    hook_registry: HookRegistry,
    circuit_breakers: HashMap<String, CircuitBreaker>,
}

impl PipelineExecutor {
    pub fn new() -> Self {
        Self {
            hook_registry: HookRegistry::new(),
            circuit_breakers: HashMap::new(),
        }
    }

    pub fn with_hooks(hook_registry: HookRegistry) -> Self {
        Self {
            hook_registry,
            circuit_breakers: HashMap::new(),
        }
    }

    /// Access the hook registry.
    pub fn hooks(&self) -> &HookRegistry {
        &self.hook_registry
    }

    /// Access the hook registry mutably.
    pub fn hooks_mut(&mut self) -> &mut HookRegistry {
        &mut self.hook_registry
    }

    /// Execute a pipeline definition synchronously (step-by-step).
    pub fn execute(
        &mut self,
        pipeline: &PipelineDefinition,
        now_ms: u64,
    ) -> Result<PipelineExecution, PipelineError> {
        pipeline.validate()?;

        let execution_id = format!("exec-{}-{}", pipeline.name, now_ms);
        let order = pipeline.topological_order()?;

        let mut execution = PipelineExecution {
            execution_id: execution_id.clone(),
            pipeline_name: pipeline.name.clone(),
            status: PipelineStatus::Running,
            step_outcomes: BTreeMap::new(),
            metadata: pipeline.metadata.clone(),
            started_at_ms: now_ms,
            ended_at_ms: None,
            total_failures: 0,
            compensations_executed: Vec::new(),
        };
        let total_steps = pipeline.steps.len();

        // Fire PipelineStart hooks.
        let start_ctx = self.make_hook_context(&execution, None, None, total_steps, now_ms);
        let start_outcomes = self
            .hook_registry
            .dispatch(HookPhase::PipelineStart, &start_ctx);
        for (hook_name, outcome) in &start_outcomes {
            match outcome {
                HookOutcome::Abort { reason } => {
                    execution.status = PipelineStatus::Aborted {
                        reason: format!("hook '{hook_name}' aborted pipeline: {reason}"),
                    };
                    execution.ended_at_ms = Some(now_ms);
                    return Ok(execution);
                }
                HookOutcome::InjectMetadata { key, value } => {
                    execution.metadata.insert(key.clone(), value.clone());
                }
                _ => {}
            }
        }

        // Execute steps in topological order.
        let mut completed_labels: HashSet<String> = HashSet::new();
        let mut step_time = now_ms;

        for &step_idx in &order {
            let step = &pipeline.steps[step_idx];
            step_time += 1; // Simulate time progression.

            let unmet_dependencies: Vec<&str> = step
                .depends_on
                .iter()
                .filter(|dep| !completed_labels.contains(*dep))
                .map(String::as_str)
                .collect();
            if !unmet_dependencies.is_empty() {
                let reason = format!("unmet dependencies: {}", unmet_dependencies.join(", "));
                if step.optional {
                    let outcome = StepOutcome {
                        step_index: step_idx,
                        step_label: step.label.clone(),
                        status: StepStatus::Skipped { reason },
                        attempts: 0,
                        duration_ms: 0,
                        recovery_attempts: 0,
                        compensations_run: 0,
                    };
                    execution.step_outcomes.insert(step_idx, outcome);
                    continue;
                }

                execution.total_failures += 1;
                let outcome = StepOutcome {
                    step_index: step_idx,
                    step_label: step.label.clone(),
                    status: StepStatus::Failed {
                        error: reason.clone(),
                    },
                    attempts: 0,
                    duration_ms: 0,
                    recovery_attempts: 0,
                    compensations_run: 0,
                };
                execution.step_outcomes.insert(step_idx, outcome);
                execution.status = PipelineStatus::Failed {
                    reason: format!("step '{}' failed: {reason}", step.label),
                };
                break;
            }

            // Check preconditions.
            let precondition_failed = step
                .preconditions
                .iter()
                .find(|key| !execution.metadata.contains_key(*key));

            if let Some(missing_key) = precondition_failed {
                if step.optional {
                    let outcome = StepOutcome {
                        step_index: step_idx,
                        step_label: step.label.clone(),
                        status: StepStatus::Skipped {
                            reason: format!("missing precondition: {missing_key}"),
                        },
                        attempts: 0,
                        duration_ms: 0,
                        recovery_attempts: 0,
                        compensations_run: 0,
                    };
                    execution.step_outcomes.insert(step_idx, outcome);
                    completed_labels.insert(step.label.clone());
                    continue;
                }
                execution.total_failures += 1;
                let outcome = StepOutcome {
                    step_index: step_idx,
                    step_label: step.label.clone(),
                    status: StepStatus::Failed {
                        error: format!("missing required precondition: {missing_key}"),
                    },
                    attempts: 1,
                    duration_ms: 0,
                    recovery_attempts: 0,
                    compensations_run: 0,
                };
                execution.step_outcomes.insert(step_idx, outcome);
                execution.status = PipelineStatus::Failed {
                    reason: format!(
                        "step '{}' failed: missing precondition '{}'",
                        step.label, missing_key
                    ),
                };
                break;
            }

            // Fire PreStep hooks.
            let pre_ctx = self.make_hook_context(
                &execution,
                Some(step_idx),
                Some(&step.label),
                total_steps,
                step_time,
            );
            let pre_outcomes = self.hook_registry.dispatch(HookPhase::PreStep, &pre_ctx);
            let mut skip_step = false;
            for (hook_name, outcome) in &pre_outcomes {
                match outcome {
                    HookOutcome::SkipStep => {
                        skip_step = true;
                        break;
                    }
                    HookOutcome::Abort { reason } => {
                        execution.status = PipelineStatus::Aborted {
                            reason: format!(
                                "hook '{hook_name}' aborted at step '{}': {reason}",
                                step.label
                            ),
                        };
                        execution.ended_at_ms = Some(step_time);
                        return Ok(execution);
                    }
                    HookOutcome::InjectMetadata { key, value } => {
                        execution.metadata.insert(key.clone(), value.clone());
                    }
                    HookOutcome::Continue => {}
                }
            }

            if skip_step {
                let outcome = StepOutcome {
                    step_index: step_idx,
                    step_label: step.label.clone(),
                    status: StepStatus::Skipped {
                        reason: "skipped by PreStep hook".to_string(),
                    },
                    attempts: 0,
                    duration_ms: 0,
                    recovery_attempts: 0,
                    compensations_run: 0,
                };
                execution.step_outcomes.insert(step_idx, outcome);
                completed_labels.insert(step.label.clone());
                continue;
            }

            // Execute the step with recovery.
            let step_result = self.execute_step_with_recovery(
                step,
                step_idx,
                &mut execution,
                step_time,
                total_steps,
            );
            execution
                .step_outcomes
                .insert(step_idx, step_result.clone());

            // Fire PostStep hooks.
            let post_ctx = self.make_hook_context(
                &execution,
                Some(step_idx),
                Some(&step.label),
                total_steps,
                step_time.saturating_add(1),
            );
            let _ = self.hook_registry.dispatch(HookPhase::PostStep, &post_ctx);

            match &step_result.status {
                StepStatus::Succeeded => {
                    completed_labels.insert(step.label.clone());
                }
                StepStatus::Skipped { .. } => {
                    completed_labels.insert(step.label.clone());
                }
                StepStatus::Failed { error } => {
                    if !step.optional {
                        execution.status = PipelineStatus::Failed {
                            reason: format!("step '{}' failed: {error}", step.label),
                        };
                        break;
                    }
                }
                StepStatus::Compensated => {
                    // Compensated steps are considered "done" for dependency purposes.
                    completed_labels.insert(step.label.clone());
                }
                _ => {}
            }
        }

        // If still running, all steps completed successfully.
        if execution.status == PipelineStatus::Running {
            execution.status = PipelineStatus::Succeeded;
        }

        // Run compensating actions if the pipeline failed and compensation is enabled.
        if matches!(execution.status, PipelineStatus::Failed { .. })
            && pipeline.compensate_on_failure
        {
            self.run_compensations(pipeline, &mut execution, step_time);
        }

        // Fire PipelineEnd hooks.
        let end_ctx = self.make_hook_context(
            &execution,
            None,
            None,
            total_steps,
            step_time.saturating_add(1),
        );
        let end_outcomes = self
            .hook_registry
            .dispatch(HookPhase::PipelineEnd, &end_ctx);
        for (_, outcome) in &end_outcomes {
            if let HookOutcome::InjectMetadata { key, value } = outcome {
                execution.metadata.insert(key.clone(), value.clone());
            }
        }

        execution.ended_at_ms = Some(step_time + 1);
        Ok(execution)
    }

    fn execute_step_with_recovery(
        &mut self,
        step: &PipelineStep,
        step_idx: usize,
        execution: &mut PipelineExecution,
        now_ms: u64,
        total_steps: usize,
    ) -> StepOutcome {
        let recovery = &step.recovery;
        let mut attempts = 0u32;
        let mut recovery_attempts = 0u32;

        // Check circuit breaker if configured.
        let breaker_key = format!("{}:{}", execution.pipeline_name, step.label);
        if let Some(cb_config) = &recovery.circuit_breaker {
            let breaker = self
                .circuit_breakers
                .entry(breaker_key.clone())
                .or_insert_with(|| CircuitBreaker::new(cb_config.clone()));
            if !breaker.allow_request_and_advance(now_ms) {
                return StepOutcome {
                    step_index: step_idx,
                    step_label: step.label.clone(),
                    status: StepStatus::Failed {
                        error: "circuit breaker open".to_string(),
                    },
                    attempts,
                    duration_ms: u64::from(attempts), // Simulated.
                    recovery_attempts,
                    compensations_run: 0,
                };
            }
        }

        // Attempt the step action.
        loop {
            attempts += 1;
            let result = execute_step_action(&step.action, &execution.metadata);

            match result {
                Ok(()) => {
                    // Record success in circuit breaker.
                    if let Some(breaker) = self.circuit_breakers.get_mut(&breaker_key) {
                        breaker.record_success(now_ms);
                    }
                    return StepOutcome {
                        step_index: step_idx,
                        step_label: step.label.clone(),
                        status: StepStatus::Succeeded,
                        attempts,
                        duration_ms: u64::from(attempts), // Simulated.
                        recovery_attempts,
                        compensations_run: 0,
                    };
                }
                Err(error) => {
                    execution.total_failures += 1;
                    execution.metadata.insert(
                        "pipeline.failure_count".to_string(),
                        execution.total_failures.to_string(),
                    );

                    // Record failure in circuit breaker.
                    if let Some(breaker) = self.circuit_breakers.get_mut(&breaker_key) {
                        breaker.record_failure(now_ms);
                    }

                    // Check if this is a non-retryable error.
                    if recovery.is_non_retryable(&error) {
                        return StepOutcome {
                            step_index: step_idx,
                            step_label: step.label.clone(),
                            status: StepStatus::Failed { error },
                            attempts,
                            duration_ms: u64::from(attempts),
                            recovery_attempts,
                            compensations_run: 0,
                        };
                    }

                    // Check if we've exhausted retries.
                    if attempts > recovery.max_retries {
                        // Try fallback chain.
                        if !recovery.fallback_chain.is_empty() {
                            return StepOutcome {
                                step_index: step_idx,
                                step_label: step.label.clone(),
                                status: StepStatus::Failed {
                                    error: format!(
                                        "{error} (exhausted {} retries, {} fallbacks available)",
                                        recovery.max_retries,
                                        recovery.fallback_chain.len()
                                    ),
                                },
                                attempts,
                                duration_ms: u64::from(attempts),
                                recovery_attempts,
                                compensations_run: 0,
                            };
                        }
                        return StepOutcome {
                            step_index: step_idx,
                            step_label: step.label.clone(),
                            status: StepStatus::Failed { error },
                            attempts,
                            duration_ms: u64::from(attempts),
                            recovery_attempts,
                            compensations_run: 0,
                        };
                    }

                    let pre_recovery_ctx = self.make_hook_context(
                        execution,
                        Some(step_idx),
                        Some(&step.label),
                        total_steps,
                        now_ms.saturating_add(u64::from(attempts)),
                    );
                    let pre_recovery_outcomes = self
                        .hook_registry
                        .dispatch(HookPhase::PreRecovery, &pre_recovery_ctx);
                    for (hook_name, outcome) in &pre_recovery_outcomes {
                        match outcome {
                            HookOutcome::Abort { reason } => {
                                return StepOutcome {
                                    step_index: step_idx,
                                    step_label: step.label.clone(),
                                    status: StepStatus::Failed {
                                        error: format!(
                                            "recovery aborted by hook '{hook_name}': {reason}"
                                        ),
                                    },
                                    attempts,
                                    duration_ms: u64::from(attempts),
                                    recovery_attempts,
                                    compensations_run: 0,
                                };
                            }
                            HookOutcome::SkipStep => {
                                return StepOutcome {
                                    step_index: step_idx,
                                    step_label: step.label.clone(),
                                    status: StepStatus::Failed {
                                        error: format!("recovery skipped by hook '{hook_name}'"),
                                    },
                                    attempts,
                                    duration_ms: u64::from(attempts),
                                    recovery_attempts,
                                    compensations_run: 0,
                                };
                            }
                            HookOutcome::InjectMetadata { key, value } => {
                                execution.metadata.insert(key.clone(), value.clone());
                            }
                            HookOutcome::Continue => {}
                        }
                    }

                    recovery_attempts += 1;
                    // Backoff delay is computed but not actually waited (sync execution).
                    let _delay = recovery.backoff.delay_for_attempt(attempts - 1);
                    let post_recovery_ctx = self.make_hook_context(
                        execution,
                        Some(step_idx),
                        Some(&step.label),
                        total_steps,
                        now_ms.saturating_add(u64::from(attempts + recovery_attempts)),
                    );
                    let post_recovery_outcomes = self
                        .hook_registry
                        .dispatch(HookPhase::PostRecovery, &post_recovery_ctx);
                    for (hook_name, outcome) in &post_recovery_outcomes {
                        match outcome {
                            HookOutcome::Abort { reason } => {
                                return StepOutcome {
                                    step_index: step_idx,
                                    step_label: step.label.clone(),
                                    status: StepStatus::Failed {
                                        error: format!(
                                            "post-recovery hook '{hook_name}' aborted: {reason}"
                                        ),
                                    },
                                    attempts,
                                    duration_ms: u64::from(attempts),
                                    recovery_attempts,
                                    compensations_run: 0,
                                };
                            }
                            HookOutcome::InjectMetadata { key, value } => {
                                execution.metadata.insert(key.clone(), value.clone());
                            }
                            HookOutcome::SkipStep | HookOutcome::Continue => {}
                        }
                    }
                }
            }
        }
    }

    fn run_compensations(
        &self,
        pipeline: &PipelineDefinition,
        execution: &mut PipelineExecution,
        now_ms: u64,
    ) {
        // Run compensating actions in reverse step order.
        let total_steps = pipeline.steps.len();
        let mut compensation_time = now_ms;

        let succeeded_indices: Vec<usize> = execution
            .step_outcomes
            .iter()
            .filter(|(_, outcome)| outcome.status == StepStatus::Succeeded)
            .map(|(&idx, _)| idx)
            .rev()
            .collect();
        for step_idx in succeeded_indices {
            if let Some(compensation) = &pipeline.steps[step_idx].compensation {
                compensation_time = compensation_time.saturating_add(1);
                let step_label = pipeline.steps[step_idx].label.clone();
                let pre_ctx = self.make_hook_context(
                    execution,
                    Some(step_idx),
                    Some(step_label.as_str()),
                    total_steps,
                    compensation_time,
                );
                let pre_outcomes = self
                    .hook_registry
                    .dispatch(HookPhase::PreCompensation, &pre_ctx);
                let mut skip_compensation = false;
                for (hook_name, outcome) in &pre_outcomes {
                    match outcome {
                        HookOutcome::Abort { reason } => {
                            execution.metadata.insert(
                                format!("pipeline.compensation.{step_label}.abort"),
                                format!("{hook_name}: {reason}"),
                            );
                            skip_compensation = true;
                            break;
                        }
                        HookOutcome::SkipStep => {
                            skip_compensation = true;
                            break;
                        }
                        HookOutcome::InjectMetadata { key, value } => {
                            execution.metadata.insert(key.clone(), value.clone());
                        }
                        HookOutcome::Continue => {}
                    }
                }
                if skip_compensation {
                    continue;
                }

                execution
                    .compensations_executed
                    .push(compensation.label.clone());
                if let Some(outcome) = execution.step_outcomes.get_mut(&step_idx) {
                    outcome.compensations_run += 1;
                    outcome.status = StepStatus::Compensated;
                }

                compensation_time = compensation_time.saturating_add(1);
                let post_ctx = self.make_hook_context(
                    execution,
                    Some(step_idx),
                    Some(step_label.as_str()),
                    total_steps,
                    compensation_time,
                );
                let post_outcomes = self
                    .hook_registry
                    .dispatch(HookPhase::PostCompensation, &post_ctx);
                for (hook_name, outcome) in &post_outcomes {
                    match outcome {
                        HookOutcome::Abort { reason } => {
                            execution.metadata.insert(
                                format!("pipeline.compensation.{step_label}.post_abort"),
                                format!("{hook_name}: {reason}"),
                            );
                        }
                        HookOutcome::InjectMetadata { key, value } => {
                            execution.metadata.insert(key.clone(), value.clone());
                        }
                        HookOutcome::SkipStep | HookOutcome::Continue => {}
                    }
                }
            }
        }
    }

    #[allow(clippy::unused_self)]
    fn make_hook_context(
        &self,
        execution: &PipelineExecution,
        step_index: Option<usize>,
        step_label: Option<&str>,
        total_steps: usize,
        now_ms: u64,
    ) -> HookContext {
        HookContext {
            execution_id: execution.execution_id.clone(),
            pipeline_name: execution.pipeline_name.clone(),
            step_index,
            step_label: step_label.map(String::from),
            elapsed_ms: now_ms.saturating_sub(execution.started_at_ms),
            steps_completed: execution
                .step_outcomes
                .values()
                .filter(|o| matches!(o.status, StepStatus::Succeeded | StepStatus::Compensated))
                .count(),
            total_steps,
            last_result: step_index.and_then(|i| execution.step_outcomes.get(&i).cloned()),
            metadata: execution.metadata.clone(),
        }
    }
}

impl Default for PipelineExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// Execute a step action. Returns Ok(()) on success, Err(message) on failure.
fn execute_step_action(
    action: &StepAction,
    _metadata: &HashMap<String, String>,
) -> Result<(), String> {
    match action {
        StepAction::Noop => Ok(()),
        StepAction::Checkpoint { .. } => Ok(()),
        StepAction::DispatchWork { work_item_id, .. } => {
            if work_item_id.is_empty() {
                Err("empty work item ID".to_string())
            } else {
                Ok(())
            }
        }
        StepAction::SendMessage { recipients, .. } => {
            if recipients.is_empty() {
                Err("no recipients specified".to_string())
            } else {
                Ok(())
            }
        }
        StepAction::WaitForCondition { condition, .. } => match condition {
            PipelineCondition::Timeout { after_ms } => {
                if *after_ms == 0 {
                    Ok(())
                } else {
                    // In a real implementation, this would poll.
                    Ok(())
                }
            }
            _ => Ok(()),
        },
        StepAction::SubPipeline { pipeline_name } => {
            if pipeline_name.is_empty() {
                Err("empty sub-pipeline name".to_string())
            } else {
                Ok(())
            }
        }
        StepAction::Command { command, .. } => {
            if command.is_empty() {
                Err("empty command".to_string())
            } else {
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from pipeline operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelineError {
    ValidationFailed { reason: String },
    DependencyCycle,
    StepNotFound { label: String },
    ExecutionFailed { reason: String },
    CircuitBreakerOpen { step_label: String },
    Timeout { step_label: String, elapsed_ms: u64 },
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValidationFailed { reason } => write!(f, "pipeline validation failed: {reason}"),
            Self::DependencyCycle => write!(f, "dependency cycle detected in pipeline"),
            Self::StepNotFound { label } => write!(f, "step not found: {label}"),
            Self::ExecutionFailed { reason } => write!(f, "pipeline execution failed: {reason}"),
            Self::CircuitBreakerOpen { step_label } => {
                write!(f, "circuit breaker open for step: {step_label}")
            }
            Self::Timeout {
                step_label,
                elapsed_ms,
            } => write!(f, "step '{step_label}' timed out after {elapsed_ms}ms"),
        }
    }
}

impl std::error::Error for PipelineError {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_step(label: &str) -> PipelineStep {
        PipelineStep {
            label: label.to_string(),
            description: format!("Test step {label}"),
            action: StepAction::Noop,
            depends_on: Vec::new(),
            recovery: RecoveryPolicy::default(),
            compensation: None,
            timeout_ms: 5000,
            optional: false,
            preconditions: Vec::new(),
        }
    }

    fn step_with_deps(label: &str, deps: Vec<&str>) -> PipelineStep {
        let mut s = noop_step(label);
        s.depends_on = deps.into_iter().map(String::from).collect();
        s
    }

    fn simple_pipeline(name: &str, steps: Vec<PipelineStep>) -> PipelineDefinition {
        PipelineDefinition {
            name: name.to_string(),
            description: format!("Test pipeline {name}"),
            steps,
            default_recovery: RecoveryPolicy::default(),
            timeout_ms: 60_000,
            compensate_on_failure: true,
            metadata: HashMap::new(),
        }
    }

    // -- Validation tests --

    #[test]
    fn validate_empty_name_fails() {
        let p = simple_pipeline("", vec![noop_step("a")]);
        assert!(matches!(
            p.validate(),
            Err(PipelineError::ValidationFailed { .. })
        ));
    }

    #[test]
    fn validate_empty_steps_fails() {
        let p = simple_pipeline("test", vec![]);
        assert!(matches!(
            p.validate(),
            Err(PipelineError::ValidationFailed { .. })
        ));
    }

    #[test]
    fn validate_duplicate_labels_fails() {
        let p = simple_pipeline("test", vec![noop_step("a"), noop_step("a")]);
        let err = p.validate().unwrap_err();
        assert!(matches!(err, PipelineError::ValidationFailed { .. }));
    }

    #[test]
    fn validate_unknown_dependency_fails() {
        let p = simple_pipeline("test", vec![step_with_deps("a", vec!["nonexistent"])]);
        assert!(matches!(
            p.validate(),
            Err(PipelineError::ValidationFailed { .. })
        ));
    }

    #[test]
    fn validate_self_dependency_fails() {
        let p = simple_pipeline("test", vec![step_with_deps("a", vec!["a"])]);
        assert!(matches!(
            p.validate(),
            Err(PipelineError::ValidationFailed { .. })
        ));
    }

    #[test]
    fn validate_cycle_detection() {
        let p = simple_pipeline(
            "test",
            vec![
                step_with_deps("a", vec!["b"]),
                step_with_deps("b", vec!["a"]),
            ],
        );
        assert!(matches!(
            p.validate(),
            Err(PipelineError::ValidationFailed { .. })
        ));
    }

    #[test]
    fn validate_valid_pipeline() {
        let p = simple_pipeline(
            "test",
            vec![
                noop_step("a"),
                step_with_deps("b", vec!["a"]),
                step_with_deps("c", vec!["a"]),
                step_with_deps("d", vec!["b", "c"]),
            ],
        );
        assert!(p.validate().is_ok());
    }

    // -- Topological ordering tests --

    #[test]
    fn topological_order_linear_chain() {
        let p = simple_pipeline(
            "test",
            vec![
                noop_step("a"),
                step_with_deps("b", vec!["a"]),
                step_with_deps("c", vec!["b"]),
            ],
        );
        let order = p.topological_order().unwrap();
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn topological_order_diamond() {
        let p = simple_pipeline(
            "test",
            vec![
                noop_step("root"),
                step_with_deps("left", vec!["root"]),
                step_with_deps("right", vec!["root"]),
                step_with_deps("join", vec!["left", "right"]),
            ],
        );
        let order = p.topological_order().unwrap();
        assert_eq!(order[0], 0); // root first
        assert_eq!(order[3], 3); // join last
    }

    #[test]
    fn ready_steps_tracks_completion() {
        let p = simple_pipeline(
            "test",
            vec![
                noop_step("a"),
                step_with_deps("b", vec!["a"]),
                step_with_deps("c", vec!["a"]),
            ],
        );
        let mut completed = HashSet::new();
        let ready = p.ready_steps(&completed);
        assert_eq!(ready, vec![0]); // Only 'a' is ready initially.

        completed.insert("a".to_string());
        let ready = p.ready_steps(&completed);
        assert_eq!(ready, vec![1, 2]); // Both b and c are ready.
    }

    // -- Execution tests --

    #[test]
    fn execute_simple_pipeline_succeeds() {
        let p = simple_pipeline("simple", vec![noop_step("a"), noop_step("b")]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert_eq!(result.status, PipelineStatus::Succeeded);
        assert_eq!(result.step_outcomes.len(), 2);
    }

    #[test]
    fn execute_with_dependencies() {
        let p = simple_pipeline(
            "deps",
            vec![
                noop_step("init"),
                step_with_deps("process", vec!["init"]),
                step_with_deps("finalize", vec!["process"]),
            ],
        );
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert_eq!(result.status, PipelineStatus::Succeeded);
        assert_eq!(result.step_outcomes.len(), 3);
        for outcome in result.step_outcomes.values() {
            assert_eq!(outcome.status, StepStatus::Succeeded);
        }
    }

    #[test]
    fn execute_failing_step_halts_pipeline() {
        let mut step = noop_step("fail");
        step.action = StepAction::Command {
            command: String::new(),
            args: Vec::new(),
        };
        step.recovery.max_retries = 0;
        let p = simple_pipeline("fail-test", vec![step]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
    }

    #[test]
    fn execute_optional_step_failure_continues() {
        let mut fail_step = noop_step("optional-fail");
        fail_step.action = StepAction::DispatchWork {
            work_item_id: String::new(),
            priority: 1,
        };
        fail_step.optional = true;
        fail_step.recovery.max_retries = 0;

        let p = simple_pipeline("optional", vec![fail_step, noop_step("after")]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert_eq!(result.status, PipelineStatus::Succeeded);
    }

    #[test]
    fn execute_dependency_on_failed_step_is_blocked() {
        let mut fail_step = noop_step("required-source");
        fail_step.action = StepAction::Command {
            command: String::new(),
            args: Vec::new(),
        };
        fail_step.optional = true;
        fail_step.recovery.max_retries = 0;

        let dependent = step_with_deps("consumer", vec!["required-source"]);
        let p = simple_pipeline("dep-block", vec![fail_step, dependent]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();

        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        let consumer = result
            .step_outcomes
            .get(&1)
            .expect("dependent step outcome should be recorded");
        match &consumer.status {
            StepStatus::Failed { error } => assert!(error.contains("unmet dependencies")),
            other => panic!("expected dependency failure, got {other:?}"),
        }
    }

    #[test]
    fn execute_with_precondition_failure() {
        let mut step = noop_step("needs-key");
        step.preconditions = vec!["required_key".to_string()];
        let p = simple_pipeline("precond", vec![step]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
    }

    #[test]
    fn execute_with_precondition_met() {
        let mut step = noop_step("needs-key");
        step.preconditions = vec!["required_key".to_string()];
        let mut p = simple_pipeline("precond-ok", vec![step]);
        p.metadata
            .insert("required_key".to_string(), "present".to_string());
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert_eq!(result.status, PipelineStatus::Succeeded);
    }

    // -- Hook tests --

    #[test]
    fn hook_registry_dispatch_by_phase() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "log-start".to_string(),
            phases: [HookPhase::PipelineStart].into(),
            priority: 100,
            enabled: true,
            handler: HookHandler::Log {
                level: LogLevel::Info,
                template: "pipeline started".to_string(),
            },
        });
        registry.register(HookRegistration {
            name: "log-step".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 100,
            enabled: true,
            handler: HookHandler::Log {
                level: LogLevel::Debug,
                template: "step starting".to_string(),
            },
        });

        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: None,
            step_label: None,
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 1,
            last_result: None,
            metadata: HashMap::new(),
        };

        let start_hooks = registry.dispatch(HookPhase::PipelineStart, &ctx);
        assert_eq!(start_hooks.len(), 1);
        assert_eq!(start_hooks[0].0, "log-start");

        let step_hooks = registry.dispatch(HookPhase::PreStep, &ctx);
        assert_eq!(step_hooks.len(), 1);
        assert_eq!(step_hooks[0].0, "log-step");
    }

    #[test]
    fn hook_priority_ordering() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "low-priority".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 200,
            enabled: true,
            handler: HookHandler::Custom {
                tag: "low".to_string(),
            },
        });
        registry.register(HookRegistration {
            name: "high-priority".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 50,
            enabled: true,
            handler: HookHandler::Custom {
                tag: "high".to_string(),
            },
        });

        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: Some(0),
            step_label: Some("step-a".to_string()),
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 1,
            last_result: None,
            metadata: HashMap::new(),
        };

        let results = registry.dispatch(HookPhase::PreStep, &ctx);
        assert_eq!(results[0].0, "high-priority");
        assert_eq!(results[1].0, "low-priority");
    }

    #[test]
    fn hook_disable_skips_execution() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "disabled-hook".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 100,
            enabled: false,
            handler: HookHandler::Custom {
                tag: "disabled".to_string(),
            },
        });

        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: None,
            step_label: None,
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 0,
            last_result: None,
            metadata: HashMap::new(),
        };

        assert!(registry.dispatch(HookPhase::PreStep, &ctx).is_empty());
    }

    #[test]
    fn hook_precondition_aborts_pipeline() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "require-auth".to_string(),
            phases: [HookPhase::PipelineStart].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Precondition {
                check: PreconditionCheck::MetadataPresent {
                    key: "auth_token".to_string(),
                },
            },
        });

        let p = simple_pipeline("guarded", vec![noop_step("a")]);
        let mut executor = PipelineExecutor::with_hooks(registry);
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Aborted { .. }));
    }

    #[test]
    fn hook_metadata_injection() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "inject-env".to_string(),
            phases: [HookPhase::PipelineStart].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Metadata {
                key: "env".to_string(),
                value: "production".to_string(),
            },
        });

        let mut step = noop_step("check-env");
        step.preconditions = vec!["env".to_string()];
        let p = simple_pipeline("injected", vec![step]);
        let mut executor = PipelineExecutor::with_hooks(registry);
        let result = executor.execute(&p, 1000).unwrap();
        assert_eq!(result.status, PipelineStatus::Succeeded);
        assert_eq!(result.metadata.get("env").unwrap(), "production");
    }

    #[test]
    fn hook_time_limit_uses_elapsed_time() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "elapsed-limit".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Precondition {
                check: PreconditionCheck::TimeLimit { max_ms: 0 },
            },
        });

        let p = simple_pipeline("elapsed-check", vec![noop_step("a")]);
        let mut executor = PipelineExecutor::with_hooks(registry);
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Aborted { .. }));
    }

    // -- Recovery policy tests --

    #[test]
    fn backoff_fixed_delay() {
        let strategy = BackoffStrategy::Fixed { delay_ms: 500 };
        assert_eq!(strategy.delay_for_attempt(0), Duration::from_millis(500));
        assert_eq!(strategy.delay_for_attempt(5), Duration::from_millis(500));
    }

    #[test]
    fn backoff_exponential() {
        let strategy = BackoffStrategy::Exponential {
            base_ms: 100,
            multiplier: 2.0,
            max_delay_ms: 5000,
        };
        assert_eq!(strategy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(strategy.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(strategy.delay_for_attempt(2), Duration::from_millis(400));
        assert_eq!(strategy.delay_for_attempt(3), Duration::from_millis(800));
        // Capped at max.
        assert_eq!(strategy.delay_for_attempt(10), Duration::from_millis(5000));
    }

    #[test]
    fn backoff_linear() {
        let strategy = BackoffStrategy::Linear {
            initial_ms: 100,
            increment_ms: 200,
            max_delay_ms: 1000,
        };
        assert_eq!(strategy.delay_for_attempt(0), Duration::from_millis(100));
        assert_eq!(strategy.delay_for_attempt(1), Duration::from_millis(300));
        assert_eq!(strategy.delay_for_attempt(2), Duration::from_millis(500));
        assert_eq!(strategy.delay_for_attempt(5), Duration::from_millis(1000)); // capped
    }

    #[test]
    fn non_retryable_error_detection() {
        let policy = RecoveryPolicy {
            non_retryable_errors: vec!["auth_failed".to_string(), "not_found".to_string()],
            ..Default::default()
        };
        assert!(policy.is_non_retryable("auth_failed: invalid token"));
        assert!(policy.is_non_retryable("resource not_found"));
        assert!(!policy.is_non_retryable("timeout occurred"));
    }

    // -- Circuit breaker tests --

    #[test]
    fn circuit_breaker_opens_on_threshold() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            reset_timeout_ms: 10_000,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);
        assert!(cb.allow_request(100));

        cb.record_failure(100);
        cb.record_failure(200);
        assert!(cb.allow_request(300)); // Still closed.
        cb.record_failure(300);
        assert!(!cb.allow_request(400)); // Now open.
        assert!(matches!(cb.state, CircuitState::Open { .. }));
    }

    #[test]
    fn circuit_breaker_resets_after_timeout() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            reset_timeout_ms: 5000,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);
        cb.record_failure(100);
        cb.record_failure(200);
        assert!(!cb.allow_request(300)); // Open.
        assert!(cb.allow_request(5300)); // After timeout, allowed.
    }

    #[test]
    fn circuit_breaker_failed_probe_reopens_with_fresh_timeout() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            reset_timeout_ms: 1000,
            success_threshold: 1,
        };
        let mut cb = CircuitBreaker::new(config);
        cb.record_failure(100);
        assert!(!cb.allow_request(200));

        // Timeout elapsed: allow one probe and transition to half-open.
        assert!(cb.allow_request_and_advance(1200));
        assert!(matches!(cb.state, CircuitState::HalfOpen { .. }));

        // Probe fails: must reopen and reset the timeout window to the latest failure.
        cb.record_failure(1200);
        assert!(!cb.allow_request(1500));
        assert!(cb.allow_request(2201));
    }

    #[test]
    fn circuit_breaker_half_open_to_closed() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            reset_timeout_ms: 1000,
            success_threshold: 2,
        };
        let mut cb = CircuitBreaker::new(config);
        cb.record_failure(100);
        assert!(!cb.allow_request(200));

        // Transition to half-open after timeout.
        assert!(cb.allow_request(1200));
        cb.record_success(1200);
        assert!(matches!(cb.state, CircuitState::HalfOpen { .. }));
        cb.record_success(1300);
        assert_eq!(cb.state, CircuitState::Closed);
    }

    // -- Compensating action tests --

    #[test]
    fn compensation_runs_on_failure() {
        let mut step_a = noop_step("a");
        step_a.compensation = Some(CompensatingAction {
            label: "undo-a".to_string(),
            compensates_step: "a".to_string(),
            action: CompensationKind::Log {
                message: "rolling back a".to_string(),
            },
            timeout_ms: 5000,
            required: true,
        });

        let mut step_b = noop_step("b");
        step_b.action = StepAction::Command {
            command: String::new(),
            args: Vec::new(),
        };
        step_b.recovery.max_retries = 0;
        step_b.depends_on = vec!["a".to_string()];

        let p = simple_pipeline("compensate-test", vec![step_a, step_b]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        assert!(
            result
                .compensations_executed
                .contains(&"undo-a".to_string())
        );
    }

    #[test]
    fn no_compensation_when_disabled() {
        let mut step_a = noop_step("a");
        step_a.compensation = Some(CompensatingAction {
            label: "undo-a".to_string(),
            compensates_step: "a".to_string(),
            action: CompensationKind::Log {
                message: "rollback".to_string(),
            },
            timeout_ms: 5000,
            required: true,
        });

        let mut step_b = noop_step("b");
        step_b.action = StepAction::Command {
            command: String::new(),
            args: Vec::new(),
        };
        step_b.recovery.max_retries = 0;
        step_b.depends_on = vec!["a".to_string()];

        let mut p = simple_pipeline("no-compensate", vec![step_a, step_b]);
        p.compensate_on_failure = false;
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        assert!(result.compensations_executed.is_empty());
    }

    #[test]
    fn compensation_hooks_dispatch_and_inject_metadata() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "comp-pre".to_string(),
            phases: [HookPhase::PreCompensation].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Metadata {
                key: "comp.pre".to_string(),
                value: "seen".to_string(),
            },
        });
        registry.register(HookRegistration {
            name: "comp-post".to_string(),
            phases: [HookPhase::PostCompensation].into(),
            priority: 20,
            enabled: true,
            handler: HookHandler::Metadata {
                key: "comp.post".to_string(),
                value: "seen".to_string(),
            },
        });

        let mut step_a = noop_step("a");
        step_a.compensation = Some(CompensatingAction {
            label: "undo-a".to_string(),
            compensates_step: "a".to_string(),
            action: CompensationKind::Log {
                message: "rollback a".to_string(),
            },
            timeout_ms: 5000,
            required: true,
        });

        let mut step_b = noop_step("b");
        step_b.action = StepAction::Command {
            command: String::new(),
            args: Vec::new(),
        };
        step_b.recovery.max_retries = 0;
        step_b.depends_on = vec!["a".to_string()];

        let p = simple_pipeline("comp-hook", vec![step_a, step_b]);
        let mut executor = PipelineExecutor::with_hooks(registry);
        let result = executor.execute(&p, 1000).unwrap();

        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        assert!(
            result
                .compensations_executed
                .contains(&"undo-a".to_string())
        );
        assert_eq!(result.metadata.get("comp.pre"), Some(&"seen".to_string()));
        assert_eq!(result.metadata.get("comp.post"), Some(&"seen".to_string()));
    }

    // -- Execution ID and metadata tests --

    #[test]
    fn execution_id_contains_pipeline_name() {
        let p = simple_pipeline("my-pipeline", vec![noop_step("a")]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert!(result.execution_id.contains("my-pipeline"));
    }

    #[test]
    fn pipeline_metadata_propagates() {
        let mut p = simple_pipeline("meta-test", vec![noop_step("a")]);
        p.metadata.insert("version".to_string(), "1.0".to_string());
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert_eq!(result.metadata.get("version").unwrap(), "1.0");
    }

    #[test]
    fn hook_registry_unregister() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "temp-hook".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 100,
            enabled: true,
            handler: HookHandler::Custom {
                tag: "temp".to_string(),
            },
        });
        assert_eq!(registry.len(), 1);
        assert!(registry.unregister("temp-hook"));
        assert_eq!(registry.len(), 0);
        assert!(!registry.unregister("nonexistent"));
    }

    #[test]
    fn hook_set_enabled_toggle() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "toggle-hook".to_string(),
            phases: [HookPhase::PreStep].into(),
            priority: 100,
            enabled: true,
            handler: HookHandler::Custom {
                tag: "toggle".to_string(),
            },
        });

        let ctx = HookContext {
            execution_id: "test".to_string(),
            pipeline_name: "test".to_string(),
            step_index: None,
            step_label: None,
            elapsed_ms: 0,
            steps_completed: 0,
            total_steps: 0,
            last_result: None,
            metadata: HashMap::new(),
        };

        assert_eq!(registry.dispatch(HookPhase::PreStep, &ctx).len(), 1);
        registry.set_enabled("toggle-hook", false);
        assert_eq!(registry.dispatch(HookPhase::PreStep, &ctx).len(), 0);
        registry.set_enabled("toggle-hook", true);
        assert_eq!(registry.dispatch(HookPhase::PreStep, &ctx).len(), 1);
    }

    #[test]
    fn step_retry_with_eventual_success() {
        // A step that fails via empty work_item_id on first action
        // but the recovery policy allows retries - since the action is deterministic,
        // it will keep failing. Test verifies retry counting.
        let mut step = noop_step("retry-test");
        step.action = StepAction::DispatchWork {
            work_item_id: String::new(),
            priority: 1,
        };
        step.recovery = RecoveryPolicy {
            max_retries: 3,
            backoff: BackoffStrategy::Fixed { delay_ms: 100 },
            ..Default::default()
        };
        let p = simple_pipeline("retry", vec![step]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        let outcome = result.step_outcomes.get(&0).unwrap();
        assert_eq!(outcome.attempts, 4); // 1 initial + 3 retries
        assert_eq!(outcome.recovery_attempts, 3);
    }

    #[test]
    fn recovery_hooks_inject_metadata() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "recovery-pre".to_string(),
            phases: [HookPhase::PreRecovery].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Metadata {
                key: "recovery.pre".to_string(),
                value: "1".to_string(),
            },
        });
        registry.register(HookRegistration {
            name: "recovery-post".to_string(),
            phases: [HookPhase::PostRecovery].into(),
            priority: 20,
            enabled: true,
            handler: HookHandler::Metadata {
                key: "recovery.post".to_string(),
                value: "1".to_string(),
            },
        });

        let mut step = noop_step("retry-hooked");
        step.action = StepAction::Command {
            command: String::new(),
            args: Vec::new(),
        };
        step.recovery.max_retries = 1;

        let p = simple_pipeline("retry-hooks", vec![step]);
        let mut executor = PipelineExecutor::with_hooks(registry);
        let result = executor.execute(&p, 1000).unwrap();

        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        assert_eq!(result.metadata.get("recovery.pre"), Some(&"1".to_string()));
        assert_eq!(result.metadata.get("recovery.post"), Some(&"1".to_string()));

        let outcome = result.step_outcomes.get(&0).unwrap();
        assert_eq!(outcome.recovery_attempts, 1);
    }

    #[test]
    fn recovery_hook_abort_stops_retry() {
        let mut registry = HookRegistry::new();
        registry.register(HookRegistration {
            name: "gate-recovery".to_string(),
            phases: [HookPhase::PreRecovery].into(),
            priority: 10,
            enabled: true,
            handler: HookHandler::Precondition {
                check: PreconditionCheck::MetadataPresent {
                    key: "allow_retry".to_string(),
                },
            },
        });

        let mut step = noop_step("retry-abort");
        step.action = StepAction::DispatchWork {
            work_item_id: String::new(),
            priority: 1,
        };
        step.recovery.max_retries = 3;

        let p = simple_pipeline("recovery-abort", vec![step]);
        let mut executor = PipelineExecutor::with_hooks(registry);
        let result = executor.execute(&p, 1000).unwrap();
        let outcome = result.step_outcomes.get(&0).unwrap();

        assert!(matches!(result.status, PipelineStatus::Failed { .. }));
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.recovery_attempts, 0);
        match &outcome.status {
            StepStatus::Failed { error } => {
                assert!(error.contains("recovery aborted by hook"));
            }
            _ => panic!("expected failed outcome"),
        }
    }

    #[test]
    fn pipeline_serde_roundtrip() {
        let p = simple_pipeline(
            "serde-test",
            vec![noop_step("a"), step_with_deps("b", vec!["a"])],
        );
        let json = serde_json::to_string(&p).unwrap();
        let deserialized: PipelineDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "serde-test");
        assert_eq!(deserialized.steps.len(), 2);
    }

    #[test]
    fn pipeline_execution_serde_roundtrip() {
        let p = simple_pipeline("serde-exec", vec![noop_step("a")]);
        let mut executor = PipelineExecutor::new();
        let result = executor.execute(&p, 1000).unwrap();
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: PipelineExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.status, PipelineStatus::Succeeded);
    }
}
