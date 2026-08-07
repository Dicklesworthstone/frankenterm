//! Event-driven workflow runner.
//!
//! Provides WorkflowRunner, WorkflowRunnerConfig, WorkflowStartResult,
//! and the event-driven execution loop that dispatches detections to
//! registered workflows.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;
use tracing::{debug, warn};

const MAX_WORKFLOW_TOTAL_DEADLINE_MS: u64 = 24 * 60 * 60 * 1000;
const WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const WORKFLOW_RUNNER_CHILD_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowRunnerChildDrainOutcome {
    Settled,
    TimedOut {
        active_tasks: usize,
        unacknowledged_tasks: usize,
    },
    Incomplete {
        active_tasks: usize,
        unacknowledged_tasks: usize,
    },
}

fn classify_workflow_runner_child_drain(
    timed_out: bool,
    settlement: crate::runtime_async::task::JoinSetSettlement,
) -> WorkflowRunnerChildDrainOutcome {
    match settlement {
        crate::runtime_async::task::JoinSetSettlement::Settled => {
            WorkflowRunnerChildDrainOutcome::Settled
        }
        crate::runtime_async::task::JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } if timed_out => WorkflowRunnerChildDrainOutcome::TimedOut {
            active_tasks,
            unacknowledged_tasks,
        },
        crate::runtime_async::task::JoinSetSettlement::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        } => WorkflowRunnerChildDrainOutcome::Incomplete {
            active_tasks,
            unacknowledged_tasks,
        },
    }
}

async fn settle_workflow_runner_children(
    child_tasks: &mut crate::runtime_async::task::JoinSet<()>,
) -> WorkflowRunnerChildDrainOutcome {
    child_tasks.abort_all();
    let drain_cx = crate::cx::for_request();
    let drain_result = crate::runtime_async::timeout_with_cx(
        &drain_cx,
        WORKFLOW_RUNNER_CHILD_DRAIN_TIMEOUT,
        async {
            loop {
                match child_tasks.drain_next_with_cx(&drain_cx).await {
                    Ok(Some(Ok(()))) => {}
                    Ok(Some(Err(error))) => {
                        if matches!(
                            error.kind(),
                            crate::runtime_async::task::JoinErrorKind::Aborted
                                | crate::runtime_async::task::JoinErrorKind::ContextCancelled
                                | crate::runtime_async::task::JoinErrorKind::WakerRegistrationFailed
                        ) {
                            tracing::debug!(
                                failure_class = ?error.kind(),
                                "Workflow child task stopped during runner shutdown"
                            );
                        } else {
                            tracing::warn!(
                                failure_class = ?error.kind(),
                                "Workflow child task failed during runner shutdown"
                            );
                        }
                    }
                    Ok(None) => return Ok(()),
                    Err(drain_error) => return Err(drain_error),
                }
            }
        },
    )
    .await;
    if let Ok(Err(drain_error)) = &drain_result {
        tracing::warn!(
            failure_class = ?drain_error.kind(),
            "Workflow child task drain context failed before terminal settlement"
        );
    }
    classify_workflow_runner_child_drain(drain_result.is_err(), child_tasks.settlement())
}

type WorkflowChildJoinResult = Result<(), crate::runtime_async::task::JoinError>;
type WorkflowChildDrainResult = Result<
    Option<WorkflowChildJoinResult>,
    crate::runtime_async::task::JoinError,
>;

enum WorkflowRunnerWake {
    Shutdown,
    Event(Box<Result<crate::events::Event, crate::events::RecvError>>),
    Child(WorkflowChildDrainResult),
}

async fn wait_for_workflow_runner_activity(
    cx: &crate::cx::Cx,
    subscriber: &mut crate::events::EventSubscriber,
    child_tasks: &mut crate::runtime_async::task::JoinSet<()>,
    shutdown: Option<(
        &std::sync::atomic::AtomicBool,
        &crate::runtime_async::notify::Notify,
    )>,
) -> WorkflowRunnerWake {
    use futures::future::{Either, select};

    if child_tasks.is_empty() {
        if let Some((shutdown_flag, shutdown_notify)) = shutdown {
            let shutdown_wait = std::pin::pin!(shutdown_notify.wait_until(|| {
                shutdown_flag.load(Ordering::SeqCst)
            }));
            let event_wait = std::pin::pin!(subscriber.recv_cx(cx));
            return match select(shutdown_wait, event_wait).await {
                Either::Left(((), _)) => WorkflowRunnerWake::Shutdown,
                Either::Right((event, _)) => WorkflowRunnerWake::Event(Box::new(event)),
            };
        }
        return WorkflowRunnerWake::Event(Box::new(subscriber.recv_cx(cx).await));
    }

    if let Some((shutdown_flag, shutdown_notify)) = shutdown {
        let shutdown_wait = std::pin::pin!(shutdown_notify.wait_until(|| {
            shutdown_flag.load(Ordering::SeqCst)
        }));
        let activity_wait = std::pin::pin!(async {
            let event_wait = std::pin::pin!(subscriber.recv_cx(cx));
            let child_wait = std::pin::pin!(child_tasks.drain_next_with_cx(cx));
            match select(event_wait, child_wait).await {
                Either::Left((event, _)) => WorkflowRunnerWake::Event(Box::new(event)),
                Either::Right((child, _)) => WorkflowRunnerWake::Child(child),
            }
        });
        return match select(shutdown_wait, activity_wait).await {
            Either::Left(((), _)) => WorkflowRunnerWake::Shutdown,
            Either::Right((wake, _)) => wake,
        };
    }

    let event_wait = std::pin::pin!(subscriber.recv_cx(cx));
    let child_wait = std::pin::pin!(child_tasks.drain_next_with_cx(cx));
    match select(event_wait, child_wait).await {
        Either::Left((event, _)) => WorkflowRunnerWake::Event(Box::new(event)),
        Either::Right((child, _)) => WorkflowRunnerWake::Child(child),
    }
}

fn workflow_wait_aborted(label: &str, err: impl std::fmt::Display) -> crate::Error {
    crate::Error::Workflow(crate::error::WorkflowError::Aborted(format!(
        "{label} cancelled: {err}"
    )))
}

fn workflow_runner_cancelled(operation: &'static str, detail: impl Into<String>) -> crate::Error {
    crate::Error::RuntimeOperation {
        operation,
        source: crate::error::RuntimeOperationSource::Cancelled(detail.into()),
    }
}

fn workflow_execution_error(
    execution_id: &str,
    error: impl std::fmt::Display,
) -> WorkflowExecutionResult {
    WorkflowExecutionResult::Error {
        execution_id: Some(execution_id.to_string()),
        error: error.to_string(),
    }
}

async fn wait_duration_with_cx(
    cx: &crate::cx::Cx,
    duration: Duration,
    label: &str,
) -> Result<(), crate::Error> {
    std::time::Instant::now()
        .checked_add(duration)
        .ok_or_else(|| {
            crate::Error::Workflow(crate::error::WorkflowError::Aborted(format!(
                "{label} duration is too large: {duration:?}"
            )))
        })?;
    let mut remaining = duration;
    while !remaining.is_zero() {
        cx.checkpoint()
            .map_err(|err| workflow_wait_aborted(label, err))?;
        let chunk = remaining.min(Duration::from_millis(50));
        crate::runtime_async::sleep_with_cx(cx, chunk)
            .await
            .map_err(|err| workflow_wait_aborted(label, err))?;
        remaining = remaining.saturating_sub(chunk);
    }
    Ok(())
}

async fn wait_required_duration_with_cx(
    cx: &crate::cx::Cx,
    required: Duration,
    timeout: Duration,
    label: &str,
    condition: &str,
) -> Result<(), crate::Error> {
    wait_duration_with_cx(cx, required.min(timeout), label).await?;
    if required > timeout {
        return Err(crate::Error::Workflow(
            crate::error::WorkflowError::Aborted(format!(
                "{label}: {condition} timed out after {}ms before required {}ms elapsed",
                timeout.as_millis(),
                required.as_millis()
            )),
        ));
    }
    Ok(())
}

fn cap_wait_by_workflow_deadline(
    requested: Duration,
    workflow_started_at: Instant,
    workflow_total_deadline_ms: u64,
) -> Duration {
    if workflow_total_deadline_ms == 0 {
        return requested;
    }
    if workflow_total_deadline_ms > MAX_WORKFLOW_TOTAL_DEADLINE_MS {
        return Duration::ZERO;
    }

    let Some(deadline) =
        workflow_started_at.checked_add(Duration::from_millis(workflow_total_deadline_ms))
    else {
        return Duration::ZERO;
    };
    requested.min(deadline.saturating_duration_since(Instant::now()))
}

fn normalized_retry_backoff_multiplier(multiplier: f64) -> f64 {
    if multiplier.is_finite() && multiplier >= 1.0 {
        multiplier
    } else {
        1.0
    }
}

fn admit_retry_ordinal(current: usize, maximum: usize) -> Option<usize> {
    current
        .checked_add(1)
        .filter(|next_ordinal| *next_ordinal <= maximum)
}

fn next_up_nonnegative(value: f64) -> f64 {
    debug_assert!(!value.is_sign_negative());
    if value.is_infinite() {
        value
    } else {
        f64::from_bits(value.to_bits().saturating_add(1))
    }
}

fn conservative_nonnegative_product(lhs: f64, rhs: f64) -> f64 {
    let product = lhs * rhs;
    if !product.is_finite() {
        return f64::INFINITY;
    }
    let rounding_residual = lhs.mul_add(rhs, -product);
    if rounding_residual > 0.0 {
        next_up_nonnegative(product)
    } else {
        product
    }
}

fn retry_backoff_scale(multiplier: f64, mut exponent: usize) -> f64 {
    let mut scale = 1.0;
    let mut factor = normalized_retry_backoff_multiplier(multiplier);
    while exponent > 0 {
        if exponent & 1 == 1 {
            scale = conservative_nonnegative_product(scale, factor);
            if !scale.is_finite() {
                return f64::INFINITY;
            }
        }
        exponent >>= 1;
        if exponent > 0 {
            factor = conservative_nonnegative_product(factor, factor);
            if !factor.is_finite() {
                return f64::INFINITY;
            }
        }
    }
    scale
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn retry_backoff_delay(
    base_delay_ms: u64,
    retry_attempt: usize,
    multiplier: f64,
) -> Duration {
    let base_delay = Duration::from_millis(base_delay_ms);
    if base_delay.is_zero() || retry_attempt <= 1 {
        return base_delay;
    }

    let scale = retry_backoff_scale(multiplier, retry_attempt.saturating_sub(1));
    let approximate_base_ms = base_delay_ms as f64;
    let base_ms_upper_bound = if (approximate_base_ms as u64) < base_delay_ms {
        next_up_nonnegative(approximate_base_ms)
    } else {
        approximate_base_ms
    };
    let scaled_ms = conservative_nonnegative_product(base_ms_upper_bound, scale);
    if !scaled_ms.is_finite() || scaled_ms >= u64::MAX as f64 {
        return Duration::from_millis(u64::MAX);
    }
    Duration::from_millis((scaled_ms.ceil() as u64).max(base_delay_ms))
}

fn invalid_jump_target_reason(step: usize, step_count: usize) -> Option<String> {
    if step < step_count {
        None
    } else {
        Some(format!(
            "jump target {step} is outside workflow step range 0..{step_count}"
        ))
    }
}

fn invalid_resume_step_reason(step: usize, step_count: usize) -> Option<String> {
    if step <= step_count {
        None
    } else {
        Some(format!(
            "resume step {step} is outside workflow resume range 0..={step_count}"
        ))
    }
}

async fn wait_external_signal_with_cx(
    cx: &crate::cx::Cx,
    registry: &ExternalSignalRegistry,
    key: &str,
    timeout: Duration,
    label: &str,
) -> Result<(), crate::Error> {
    std::time::Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| {
            crate::Error::Workflow(crate::error::WorkflowError::Aborted(format!(
                "{label} duration is too large: {timeout:?}"
            )))
        })?;
    let mut remaining = timeout;
    let mut interval = Duration::from_millis(5);
    let max_interval = Duration::from_millis(50);
    loop {
        cx.checkpoint()
            .map_err(|err| workflow_wait_aborted(label, err))?;
        if registry.is_signaled(key) {
            return Ok(());
        }
        if remaining.is_zero() {
            return Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "{label}: external signal '{key}' timed out after {}ms",
                    timeout.as_millis()
                )),
            ));
        }
        let chunk = interval.min(remaining);
        crate::runtime_async::sleep_with_cx(cx, chunk)
            .await
            .map_err(|err| workflow_wait_aborted(label, err))?;
        remaining = remaining.saturating_sub(chunk);
        interval = interval.saturating_mul(2).min(max_interval);
    }
}

async fn wait_condition_pause_with_cx(
    cx: &crate::cx::Cx,
    condition: &WaitCondition,
    timeout: Duration,
    external_signals: Option<&ExternalSignalRegistry>,
    label: &str,
) -> Result<(), crate::Error> {
    match condition {
        WaitCondition::PaneIdle {
            idle_threshold_ms, ..
        } => {
            wait_required_duration_with_cx(
                cx,
                Duration::from_millis(*idle_threshold_ms),
                timeout,
                label,
                "pane idle wait",
            )
            .await
        }
        WaitCondition::Pattern { rule_id, .. } => Err(crate::Error::Workflow(
            crate::error::WorkflowError::Aborted(format!(
                "{label}: pattern wait '{rule_id}' requires a pane text source; use \
                 WaitConditionExecutor instead of the WorkflowRunner timeout-sleep fallback"
            )),
        )),
        WaitCondition::TextMatch { matcher, .. } => Err(crate::Error::Workflow(
            crate::error::WorkflowError::Aborted(format!(
                "{label}: text-match wait {} requires a pane text source; use \
                 WaitConditionExecutor instead of the WorkflowRunner timeout-sleep fallback",
                matcher.description()
            )),
        )),
        WaitCondition::External { key } => {
            if key.trim().is_empty() {
                return Err(crate::Error::Workflow(
                    crate::error::WorkflowError::Aborted(format!(
                        "{label}: external signal key cannot be empty"
                    )),
                ));
            }
            let Some(registry) = external_signals else {
                return Err(crate::Error::Workflow(
                    crate::error::WorkflowError::Aborted(format!(
                        "{label}: external signal '{key}' requires registry; wire one via \
                         WorkflowRunner::with_external_signals(registry)"
                    )),
                ));
            };
            wait_external_signal_with_cx(cx, registry, key, timeout, label).await
        }
        WaitCondition::StableTail { stable_for_ms, .. } => {
            wait_required_duration_with_cx(
                cx,
                Duration::from_millis(*stable_for_ms),
                timeout,
                label,
                "stable tail wait",
            )
            .await
        }
        WaitCondition::Sleep { duration_ms } => {
            wait_required_duration_with_cx(
                cx,
                Duration::from_millis(*duration_ms),
                timeout,
                label,
                "sleep wait",
            )
            .await
        }
    }
}

// ============================================================================
// WorkflowRunner - Event-driven workflow execution
// ============================================================================

/// Result of attempting to start a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowStartResult {
    /// Workflow started successfully
    Started {
        /// Unique execution ID
        execution_id: String,
        /// Name of the workflow that was started
        workflow_name: String,
    },
    /// No workflow handles this detection
    NoMatchingWorkflow {
        /// The rule_id from the detection
        rule_id: String,
    },
    /// The pane is already locked by another workflow
    PaneLocked {
        /// The pane that is locked
        pane_id: u64,
        /// Workflow name holding the lock
        held_by_workflow: String,
        /// Execution ID holding the lock
        held_by_execution: String,
    },
    /// Global concurrent workflow limit reached.
    ConcurrencyLimitReached {
        /// Current number of active workflows.
        active: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// The detection's source pane is not in the workflow's trust scope (ft-j0ufc).
    SourcePaneNotTrusted {
        /// The source pane that produced the trigger text.
        source_pane_id: u64,
        /// The matched workflow's name.
        workflow_name: String,
        /// The detection rule_id that would have fired.
        rule_id: String,
    },
    /// The target pane is rate/usage-limited and the workflow declared
    /// `requires_unlimited_pane()` (ft-7h5da.8.3, W7.3). Declined before any
    /// lock, engine state, or audit row is created.
    PaneRateLimited {
        /// The pane that is currently limited.
        pane_id: u64,
        /// The matched workflow's name.
        workflow_name: String,
        /// The detection rule_id that would have fired.
        rule_id: String,
        /// Effective reset deadline (epoch ms) when the pane is expected to
        /// become usable again — the latest active limit window's deadline.
        reset_at_ms: i64,
        /// False when `reset_at_ms` is a conservative fallback (the underlying
        /// window had no parseable reset, `reset_source == "unknown_ttl"`).
        reset_known: bool,
    },
    /// An error occurred
    Error {
        /// Error message
        error: String,
    },
}

/// Outcome of a manually invoked workflow run (ft-cli44).
///
/// The manual entry points — MCP `wa.workflow_run`, `ft robot workflows run`,
/// and `ft workflow run` — must use the same lock + execution-record protocol
/// as the detection path. See [`WorkflowRunner::run_workflow_manual_with_cx`].
#[derive(Debug)]
pub enum ManualWorkflowRunOutcome {
    /// Lock acquired, execution record persisted, workflow ran to a terminal
    /// result.
    Ran(WorkflowExecutionResult),
    /// The target pane is already locked by another workflow execution; the
    /// workflow was not started.
    PaneLocked {
        /// The pane that is locked.
        pane_id: u64,
        /// Workflow name holding the lock.
        held_by_workflow: String,
        /// Execution ID holding the lock.
        held_by_execution: String,
    },
    /// Runner-wide concurrency limit reached; the workflow was not started.
    ConcurrencyLimitReached {
        /// Current number of active workflows.
        active: usize,
        /// Configured maximum.
        limit: usize,
    },
    /// The execution record could not be created; the workflow was not run.
    StartError {
        /// Error message from the engine/storage start handshake.
        error: String,
    },
}

impl WorkflowStartResult {
    /// Returns true if a workflow was started.
    #[must_use]
    pub fn is_started(&self) -> bool {
        matches!(self, Self::Started { .. })
    }

    /// Returns true if the pane was locked by another workflow.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        matches!(self, Self::PaneLocked { .. })
    }

    /// Returns true if the trigger was refused by the workflow's
    /// source-pane trust scope (ft-j0ufc).
    #[must_use]
    pub fn is_source_pane_not_trusted(&self) -> bool {
        matches!(self, Self::SourcePaneNotTrusted { .. })
    }

    /// Returns true if the trigger was declined because the target pane is
    /// rate/usage-limited and the workflow requires an unlimited pane
    /// (ft-7h5da.8.3).
    #[must_use]
    pub fn is_pane_rate_limited(&self) -> bool {
        matches!(self, Self::PaneRateLimited { .. })
    }

    /// Returns the execution ID if the workflow was started.
    #[must_use]
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Started { execution_id, .. } => Some(execution_id),
            _ => None,
        }
    }
}

/// Result of workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowExecutionResult {
    /// Workflow completed successfully
    Completed {
        /// Execution ID
        execution_id: String,
        /// Final result value
        result: serde_json::Value,
        /// Total elapsed time in milliseconds
        elapsed_ms: u64,
        /// Number of steps executed
        steps_executed: usize,
    },
    /// Workflow was aborted
    Aborted {
        /// Execution ID
        execution_id: String,
        /// Reason for abort
        reason: String,
        /// Step index where abort occurred
        step_index: usize,
        /// Elapsed time in milliseconds
        elapsed_ms: u64,
    },
    /// Workflow step was denied by policy
    PolicyDenied {
        /// Execution ID
        execution_id: String,
        /// Step index where denial occurred
        step_index: usize,
        /// Reason for denial
        reason: String,
    },
    /// An error occurred during execution
    Error {
        /// Execution ID (if available)
        execution_id: Option<String>,
        /// Error message
        error: String,
    },
}

impl WorkflowExecutionResult {
    /// Returns true if the workflow completed successfully.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed { .. })
    }

    /// Returns true if the workflow was aborted.
    #[must_use]
    pub fn is_aborted(&self) -> bool {
        matches!(self, Self::Aborted { .. })
    }

    /// Returns the execution ID.
    #[must_use]
    pub fn execution_id(&self) -> Option<&str> {
        match self {
            Self::Completed { execution_id, .. }
            | Self::Aborted { execution_id, .. }
            | Self::PolicyDenied { execution_id, .. } => Some(execution_id),
            Self::Error { execution_id, .. } => execution_id.as_deref(),
        }
    }
}

/// Configuration for the workflow runner.
#[derive(Debug, Clone)]
pub struct WorkflowRunnerConfig {
    /// Maximum concurrent workflow executions
    pub max_concurrent: usize,
    /// Default timeout for step execution (milliseconds)
    pub step_timeout_ms: u64,
    /// Retry delay multiplier for exponential backoff. A step's returned
    /// `Retry::delay_ms` is the first-attempt base; retry N waits
    /// `base * multiplier^(N - 1)`.
    pub retry_backoff_multiplier: f64,
    /// Maximum retries per step
    pub max_retries_per_step: usize,
    /// ft-3p7re: maximum total wall-clock time a single workflow execution
    /// may run from `run_workflow` entry to natural completion. Exceeding
    /// this deadline triggers the same cleanup path as a cx-cancellation:
    /// `fail_execution`, `mark_trigger_event_handled("error")`, lock
    /// release, and a terminal-action audit row, then returns
    /// `WorkflowExecutionResult::Error`. Default 600_000 ms (10 min) — a
    /// pathologically retrying workflow can no longer pin a pane forever.
    /// Set to `0` to disable the overall deadline (legacy behavior).
    pub workflow_total_deadline_ms: u64,
}

impl Default for WorkflowRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            step_timeout_ms: 30_000,
            retry_backoff_multiplier: 2.0,
            max_retries_per_step: 3,
            workflow_total_deadline_ms: 600_000,
        }
    }
}

/// Event-driven workflow runner that subscribes to detection events
/// and executes matching workflows.
///
/// # Architecture
///
/// ```text
/// EventBus (detections) -> WorkflowRunner -> find_matching_workflow
///                                         -> acquire_pane_lock
///                                         -> WorkflowEngine (persist)
///                                         -> execute_steps
///                                         -> release_pane_lock
/// ```
///
/// # Usage
///
/// ```ignore
/// let runner = WorkflowRunner::new(
///     engine,
///     lock_manager,
///     storage,
///     injector,
///     config,
/// );
///
/// // Register workflows
/// runner.register_workflow(Arc::new(MyWorkflow::new()));
///
/// // Run the event loop
/// runner.run(event_bus).await;
/// ```
pub struct WorkflowRunner {
    /// Registered workflows
    workflows: std::sync::RwLock<Vec<Arc<dyn Workflow>>>,
    /// Workflow engine for persistence
    pub engine: WorkflowEngine,
    /// Per-pane lock manager
    lock_manager: Arc<PaneWorkflowLockManager>,
    /// Storage handle for persistence
    storage: Arc<crate::storage::StorageHandle>,
    /// Policy-gated injector for terminal input
    injector: CxPolicyInjector,
    /// Optional replay capture adapter for decision provenance.
    replay_capture: Option<crate::replay_capture::SharedCaptureAdapter>,
    /// Optional external signal registry for `WaitCondition::External` (ft-ao9k9).
    /// When unset, External waits return an explicit `WorkflowError::Aborted`
    /// instead of the legacy timeout-sleep mock.
    external_signals: Option<Arc<ExternalSignalRegistry>>,
    /// Configuration
    config: WorkflowRunnerConfig,
}

impl WorkflowRunner {
    /// Create a new workflow runner.
    ///
    /// This stays public because the ft CLI and other integration surfaces
    /// construct runners directly around a shared engine/storage/injector set.
    pub fn new(
        engine: WorkflowEngine,
        lock_manager: Arc<PaneWorkflowLockManager>,
        storage: Arc<crate::storage::StorageHandle>,
        injector: CxPolicyInjector,
        mut config: WorkflowRunnerConfig,
    ) -> Self {
        let normalized_multiplier =
            normalized_retry_backoff_multiplier(config.retry_backoff_multiplier);
        if config.retry_backoff_multiplier.to_bits() != normalized_multiplier.to_bits() {
            warn!(
                failure_class = "invalid_retry_backoff_multiplier",
                fallback_multiplier = normalized_multiplier,
                "Workflow runner retry multiplier must be finite and at least 1.0; using safe constant-delay fallback"
            );
            config.retry_backoff_multiplier = normalized_multiplier;
        }
        Self {
            workflows: std::sync::RwLock::new(Vec::new()),
            engine,
            lock_manager,
            storage,
            injector,
            replay_capture: None,
            external_signals: None,
            config,
        }
    }

    /// Attach a replay capture adapter for workflow step decision provenance.
    #[must_use]
    pub fn with_replay_capture_adapter(
        mut self,
        replay_capture: crate::replay_capture::SharedCaptureAdapter,
    ) -> Self {
        self.replay_capture = Some(replay_capture);
        self
    }

    /// Attach an external signal registry consulted by `WaitCondition::External`
    /// (ft-ao9k9). Without a registry, External waits abort with an explicit
    /// error naming the signal key and the wiring API instead of silently
    /// sleeping until the configured timeout.
    #[must_use]
    pub fn with_external_signals(mut self, registry: Arc<ExternalSignalRegistry>) -> Self {
        self.external_signals = Some(registry);
        self
    }

    /// Borrow the external signal registry, if any.
    #[must_use]
    pub fn external_signals(&self) -> Option<&Arc<ExternalSignalRegistry>> {
        self.external_signals.as_ref()
    }

    /// Get the lock manager.
    pub fn lock_manager(&self) -> &Arc<PaneWorkflowLockManager> {
        &self.lock_manager
    }

    /// Register a workflow.
    ///
    /// Recovers from a poisoned `workflows` RwLock instead of
    /// panicking. The `Vec<Arc<dyn Workflow>>` is append-only;
    /// the worst-case mid-push panic state is "the panicking
    /// registration didn't add the workflow" — the correct
    /// recovery state. See ft-o2t7l for the analysis: the
    /// runner is shared across every detection event, so a
    /// poisoned lock would brick the entire event-driven
    /// workflow surface until process restart.
    pub fn register_workflow(&self, workflow: Arc<dyn Workflow>) {
        let mut workflows = self
            .workflows
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        workflows.push(workflow);
    }

    /// Find a workflow that handles the given detection.
    ///
    /// Recovers from a poisoned `workflows` RwLock — see
    /// `register_workflow` for the rationale.
    pub fn find_matching_workflow(
        &self,
        detection: &crate::patterns::Detection,
    ) -> Option<Arc<dyn Workflow>> {
        let workflows = self
            .workflows
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        workflows
            .iter()
            .find(|w| w.is_enabled() && w.handles(detection))
            .cloned()
    }

    /// Find a workflow by name.
    ///
    /// Recovers from a poisoned `workflows` RwLock.
    pub fn find_workflow_by_name(&self, name: &str) -> Option<Arc<dyn Workflow>> {
        let workflows = self
            .workflows
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        workflows.iter().find(|w| w.name() == name).cloned()
    }

    /// Handle a detection event, potentially starting a workflow.
    ///
    /// Returns immediately with `WorkflowStartResult`. The actual workflow
    /// execution happens asynchronously if started.
    pub async fn handle_detection(
        &self,
        pane_id: u64,
        detection: &crate::patterns::Detection,
        event_id: Option<i64>,
    ) -> WorkflowStartResult {
        // ft-dit9w: ergonomic wrapper around `handle_detection_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.handle_detection_with_cx(&cx, pane_id, detection, event_id)
            .await
    }

    /// Cx-first variant of [`WorkflowRunner::handle_detection`]
    /// (ft-xbnl0.2.2).
    ///
    /// Honours the caller's asupersync capability context by
    /// short-circuiting with `WorkflowStartResult::Error` when the Cx is
    /// already cancelled on entry, before doing any pane-lock
    /// acquisition or engine work. This prevents a detection dispatched
    /// to a cancelled context from acquiring a lock that no workflow
    /// will ever release on the inner path.
    ///
    /// Tick 190 (ft-xbnl0.2.3): inlines the detection body and routes
    /// the engine start through `engine.start_with_id_cx(cx, ...)` so
    /// the persist threads cx into `upsert_workflow_with_cx` (tick 189).
    /// Legacy `handle_detection` preserved verbatim; this variant no
    /// longer delegates so the storage insert is cancel-observant.
    pub async fn handle_detection_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        detection: &crate::patterns::Detection,
        event_id: Option<i64>,
    ) -> WorkflowStartResult {
        if cx.is_cancel_requested() {
            return WorkflowStartResult::Error {
                error: "capability context already cancelled".to_owned(),
            };
        }

        let Some(workflow) = self.find_matching_workflow(detection) else {
            return WorkflowStartResult::NoMatchingWorkflow {
                rule_id: detection.rule_id.clone(),
            };
        };

        let workflow_name = workflow.name().to_string();

        // ft-j0ufc: enforce source-pane trust scope (mirrors the legacy
        // `handle_detection` check). Run before lock acquisition so a
        // refused trigger leaves no lock, no engine state, and no audit
        // row.
        let trigger_policy = workflow.trigger_policy();
        let source_pane_id = pane_id;
        if !trigger_policy.allows_source_pane(source_pane_id) {
            tracing::warn!(
                source_pane_id,
                workflow = %workflow_name,
                rule_id = %detection.rule_id,
                explicit_cx = true,
                "workflow trigger refused: source pane not in trust scope (ft-j0ufc)"
            );
            return WorkflowStartResult::SourcePaneNotTrusted {
                source_pane_id,
                workflow_name,
                rule_id: detection.rule_id.clone(),
            };
        }

        // ft-7h5da.8.3 (W7.3): decline workflows that require an unlimited pane
        // when the target pane has an active rate/usage-limit window. Runs
        // before lock acquisition so a declined trigger leaves no lock, engine
        // state, or audit row (mirrors the ft-j0ufc check above). Fails closed:
        // if the ledger cannot be consulted, the workflow is not run.
        if workflow.requires_unlimited_pane() {
            match self
                .storage
                .list_active_limit_windows_with_cx(cx, now_ms())
                .await
            {
                Ok(windows) => {
                    if let Some(window) = windows
                        .iter()
                        .filter(|w| w.pane_id == pane_id)
                        .max_by_key(|w| w.effective_reset_at_ms())
                    {
                        let reset_at_ms = window.effective_reset_at_ms();
                        let reset_known = window.reset_known();
                        tracing::info!(
                            pane_id,
                            workflow = %workflow_name,
                            rule_id = %detection.rule_id,
                            reset_at_ms,
                            reset_known,
                            "workflow trigger declined: target pane rate-limited (ft-7h5da.8.3)"
                        );
                        return WorkflowStartResult::PaneRateLimited {
                            pane_id,
                            workflow_name,
                            rule_id: detection.rule_id.clone(),
                            reset_at_ms,
                            reset_known,
                        };
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        pane_id,
                        workflow = %workflow_name,
                        error = %err,
                        "limit-window lookup failed; declining requires_unlimited_pane workflow"
                    );
                    return WorkflowStartResult::Error {
                        error: format!("limit-window lookup failed: {err}"),
                    };
                }
            }
        }

        let execution_id = generate_workflow_id(&workflow_name);
        // ft-rlbvg: use the owned-guard variant of try_acquire_with_limit
        // so the post-acquire engine-error path drops the guard
        // automatically — including under panic unwind.
        let lock_guard = match self.lock_manager.try_acquire_with_limit_owned_full(
            pane_id,
            &workflow_name,
            &execution_id,
            self.config.max_concurrent,
        ) {
            Ok(crate::workflows::lock::OwnedLockAcquisitionResult::Acquired(g)) => g,
            Ok(crate::workflows::lock::OwnedLockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                ..
            }) => {
                return WorkflowStartResult::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                };
            }
            Err(limit_info) => {
                return WorkflowStartResult::ConcurrencyLimitReached {
                    active: limit_info.active,
                    limit: limit_info.limit,
                };
            }
        };

        let agent_type_str = match detection.agent_type {
            crate::patterns::AgentType::Codex => "codex",
            crate::patterns::AgentType::ClaudeCode => "claude_code",
            crate::patterns::AgentType::Gemini => "gemini",
            crate::patterns::AgentType::Wezterm => "wezterm",
            crate::patterns::AgentType::Unknown => "unknown",
        };
        let severity_str = format!("{:?}", detection.severity).to_lowercase();

        // ft-j0ufc: persist `source_pane_id` for audit/forensics; see
        // the legacy `handle_detection` body for the rationale.
        let context = serde_json::json!({
            "rule_id": detection.rule_id,
            "agent_type": agent_type_str,
            "event_type": detection.event_type,
            "severity": severity_str,
            "confidence": detection.confidence,
            "extracted": detection.extracted,
            "matched_text": detection.matched_text,
            "span": { "start": detection.span.0, "end": detection.span.1 },
            "source_pane_id": source_pane_id,
            "detection": {
                "rule_id": detection.rule_id,
                "matched_text": detection.matched_text,
                "severity": format!("{:?}", detection.severity),
            }
        });

        match self
            .engine
            .start_with_id_cx(
                cx,
                &self.storage,
                super::engine::WorkflowStartInput {
                    execution_id: execution_id.clone(),
                    workflow_name: workflow_name.clone(),
                    pane_id,
                    trigger_event_id: event_id,
                    context: Some(context),
                },
            )
            .await
        {
            Ok(_execution) => {
                // ft-rlbvg: handoff to downstream `run_workflow_inner`,
                // which takes its own `held_lock_release_guard` at
                // entry. We must keep the lock entry alive for that
                // handoff — `defuse()` consumes the guard without
                // releasing (and without leaking the Arc).
                lock_guard.defuse();
                WorkflowStartResult::Started {
                    execution_id,
                    workflow_name,
                }
            }
            Err(e) => {
                // lock_guard drops here → release(pane_id, &execution_id).
                drop(lock_guard);
                WorkflowStartResult::Error {
                    error: e.to_string(),
                }
            }
        }
    }

    /// Manually start and run a workflow to completion (ft-cli44).
    ///
    /// The detection path ([`Self::handle_detection_with_cx`]) acquires the
    /// pane workflow lock and persists the execution record via
    /// `engine.start_with_id_cx` BEFORE `run_workflow`. The manual entry
    /// points (MCP `wa.workflow_run`, `ft robot workflows run`,
    /// `ft workflow run`) used to skip both and call [`Self::run_workflow`]
    /// directly with a fabricated execution id: every storage helper that
    /// requires the execution record (`update_execution_step`,
    /// abort/complete persistence) then failed with
    /// `WorkflowError::NotFound(execution_id)`, so manual runs could not
    /// persist progress, were invisible to status/abort surfaces, and
    /// bypassed the pane lock + concurrency limit.
    ///
    /// This method is the supported manual entry point: same lock +
    /// execution-record protocol as the detection path, minus the
    /// detection-only trigger checks (source-pane trust scope and the
    /// rate-limit decline are properties of pane-output-triggered
    /// automation; manual runs are operator-initiated and remain
    /// policy-gated per step).
    pub async fn run_workflow_manual_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        trigger_context: Option<serde_json::Value>,
    ) -> ManualWorkflowRunOutcome {
        let workflow_name = workflow.name().to_string();
        let lock_guard = match self.lock_manager.try_acquire_with_limit_owned_full(
            pane_id,
            &workflow_name,
            execution_id,
            self.config.max_concurrent,
        ) {
            Ok(crate::workflows::lock::OwnedLockAcquisitionResult::Acquired(g)) => g,
            Ok(crate::workflows::lock::OwnedLockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                ..
            }) => {
                return ManualWorkflowRunOutcome::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                };
            }
            Err(limit_info) => {
                return ManualWorkflowRunOutcome::ConcurrencyLimitReached {
                    active: limit_info.active,
                    limit: limit_info.limit,
                };
            }
        };

        match self
            .engine
            .start_with_id_cx(
                cx,
                &self.storage,
                super::engine::WorkflowStartInput {
                    execution_id: execution_id.to_string(),
                    workflow_name: workflow_name.clone(),
                    pane_id,
                    trigger_event_id: None,
                    context: trigger_context,
                },
            )
            .await
        {
            Ok(_execution) => {
                // Handoff to `run_workflow_inner`, which takes its own RAII
                // release guard at entry — defuse (consume without
                // releasing) exactly like the detection path.
                lock_guard.defuse();
                ManualWorkflowRunOutcome::Ran(
                    self.run_workflow_with_cx(cx, pane_id, workflow, execution_id, 0)
                        .await,
                )
            }
            Err(e) => {
                // lock_guard drops here → release(pane_id, &execution_id).
                drop(lock_guard);
                ManualWorkflowRunOutcome::StartError {
                    error: e.to_string(),
                }
            }
        }
    }

    /// Run a workflow execution to completion.
    ///
    /// This method executes all steps of a workflow, handling retries,
    /// wait conditions, and policy gates.
    ///
    /// # Plan-first execution (wa-upg.2.3)
    ///
    /// If the workflow implements `to_action_plan`, the plan is generated and
    /// attached to the context before execution begins. This enables:
    /// - Deterministic step descriptions for audit trails
    /// - Idempotency keys for safe replay
    /// - Structured verification and failure handling
    pub async fn run_workflow(
        &self,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        start_step: usize,
    ) -> WorkflowExecutionResult {
        // ft-dit9w: ergonomic wrapper around `run_workflow_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_workflow_with_cx(&cx, pane_id, workflow, execution_id, start_step)
            .await
    }

    /// Shared Cx-first implementation for [`run_workflow`] and
    /// [`run_workflow_with_cx`]. The ambient entry constructs a concrete Cx
    /// before reaching this function, so optional proof would only hide gaps
    /// in cancellation propagation.
    #[allow(clippy::too_many_arguments)]
    async fn run_workflow_inner(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        start_step: usize,
    ) -> WorkflowExecutionResult {
        // ft-haa2b: pane workflow lock is acquired upstream (in
        // `handle_detection_with_cx` or the resume loop). Take an RAII
        // release guard at function entry so every early-return path
        // — and any panic unwind — drops the lock by construction. This
        // replaces a chain of 17 manual `lock_manager.release(...)`
        // call sites that previously had to be threaded through every
        // branch of the step loop.
        let _release_guard = self
            .lock_manager
            .held_lock_release_guard(pane_id, execution_id);
        let start_time = Instant::now();
        let workflow_name = workflow.name().to_string();
        let step_count = workflow.step_count();
        let mut current_step = start_step;
        let mut retries = 0;
        let mut jump_count: usize = 0;
        // Prevent infinite loops from backward JumpTo cycles.
        // A workflow with N steps should never need more than N*10 jumps.
        let max_total_jumps = step_count.saturating_mul(10).max(100);
        let start_action_id_result = if start_step == 0 {
            record_workflow_start_action_with_cx(
                cx,
                &self.storage,
                &workflow_name,
                execution_id,
                pane_id,
                step_count,
                start_step,
            )
            .await
            .map(Some)
        } else {
            fetch_workflow_start_action_id_with_cx(cx, &self.storage, execution_id).await
        };
        let start_action_id = match start_action_id_result {
            Ok(action_id) => action_id,
            Err(error) => {
                let reason = format!("Workflow start-audit setup failed: {error}");
                let error = self
                    .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                    .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error,
                };
            }
        };

        if let Some(reason) = invalid_resume_step_reason(start_step, step_count) {
            tracing::error!(
                execution_id,
                start_step,
                step_count,
                "Workflow resume step is outside the executable step range"
            );
            if let Err(e) = self
                .persist_aborted_execution_with_cx(cx, execution_id, &reason, "aborted")
                .await
            {
                tracing::error!(
                    execution_id,
                    error = %e,
                    "Failed to persist abort after invalid resume step; not reporting aborted"
                );
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: e.to_string(),
                };
            }
            if let Err(error) = record_workflow_terminal_action_with_cx(
                cx,
                &self.storage,
                &workflow_name,
                execution_id,
                pane_id,
                "workflow_aborted",
                "aborted",
                Some(&reason),
                Some(start_step),
                None,
                start_action_id,
            )
            .await
            {
                return workflow_execution_error(execution_id, error);
            }

            return WorkflowExecutionResult::Aborted {
                execution_id: execution_id.to_string(),
                reason,
                step_index: start_step,
                elapsed_ms: elapsed_ms(start_time),
            };
        }

        // Create workflow context with injector for policy-gated actions.
        // Use prompt() capabilities (alt_screen: Some(false)) as the baseline —
        // workflows are triggered by detections on active panes where normal-screen
        // is the expected state. PaneCapabilities::default() leaves alt_screen as
        // None which causes the policy engine to require approval for SendText.
        let mut ctx = WorkflowContext::new(
            self.storage.clone(),
            pane_id,
            PaneCapabilities::prompt(),
            execution_id,
        )
        .with_injector(self.injector.clone());

        // Attach persisted trigger context (if any) so workflows can interpret extracted fields.
        let maybe_wf = match self.storage.get_workflow_with_cx(cx, execution_id).await {
            Ok(record) => record,
            Err(error) => {
                let reason = format!("Workflow trigger-context lookup failed: {error}");
                let error = self
                    .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                    .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error,
                };
            }
        };
        if let Some(record) = maybe_wf {
            if let Some(trigger) = record.context {
                ctx = ctx.with_trigger(trigger);
            }
        }

        let maybe_pane = match self.storage.get_pane_with_cx(cx, pane_id).await {
            Ok(record) => record,
            Err(error) => {
                let reason = format!("Workflow pane-metadata lookup failed: {error}");
                let error = self
                    .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                    .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error,
                };
            }
        };
        if let Some(record) = maybe_pane {
            ctx.set_pane_meta(PaneMetadata::from_record(&record));
        }

        if let Some(adapter) = self.replay_capture.as_ref() {
            if let Err(error) = self
                .injector
                .set_decision_capture(cx, adapter.clone())
                .await
            {
                let reason = format!("Workflow decision-capture setup failed: {error}");
                let mut reported_error = reason.clone();
                if let Err(cleanup_error) = self
                    .persist_terminal_failure_with_fresh_cx(
                        &workflow_name,
                        execution_id,
                        pane_id,
                        &reason,
                        current_step,
                        start_action_id,
                        "error",
                        "workflow_error",
                        "error",
                    )
                    .await
                {
                    reported_error.push_str(&format!(
                        "; independent cleanup also failed: {cleanup_error}"
                    ));
                }
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: reported_error,
                };
            }
        }

        // Plan-first execution: generate ActionPlan if workflow supports it (wa-upg.2.3)
        if let Some(plan) = workflow.to_action_plan(&ctx, execution_id) {
            tracing::info!(
                execution_id,
                workflow_name = %workflow_name,
                plan_id = %plan.plan_id,
                step_count = plan.step_count(),
                "Generated action plan for workflow"
            );

            // Validate the plan before execution
            if let Err(validation_error) = plan.validate() {
                tracing::error!(
                    execution_id,
                    error = %validation_error,
                    "Action plan validation failed"
                );
                let reason = format!("Plan validation failed: {validation_error}");
                if let Err(cleanup_error) =
                    self.fail_execution_with_cx(cx, execution_id, &reason).await
                {
                    return workflow_execution_error(
                        execution_id,
                        format!("{reason}; failure-state persistence also failed: {cleanup_error}"),
                    );
                }
                if let Err(cleanup_error) = self
                    .mark_trigger_event_handled_with_cx(cx, execution_id, "error")
                    .await
                {
                    return workflow_execution_error(
                        execution_id,
                        format!("{reason}; trigger-state persistence also failed: {cleanup_error}"),
                    );
                }
                if let Err(error) = record_workflow_terminal_action_with_cx(
                    cx,
                    &self.storage,
                    &workflow_name,
                    execution_id,
                    pane_id,
                    "workflow_error",
                    "error",
                    Some(&reason),
                    Some(current_step),
                    None,
                    start_action_id,
                )
                .await
                {
                    return workflow_execution_error(execution_id, error);
                }
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: reason,
                };
            }

            let persist_result = self
                .storage
                .upsert_action_plan_with_cx(cx, execution_id, &plan)
                .await;
            if let Err(e) = persist_result {
                let reason = format!("Workflow action-plan persistence failed: {e}");
                let error = self
                    .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                    .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error,
                };
            }

            ctx.set_action_plan(plan);
        }

        while current_step < step_count {
            // ft-3p7re: per-step overall-deadline seam. If the workflow
            // has been running longer than `workflow_total_deadline_ms`,
            // treat it as a runaway and run the same cleanup sequence as
            // a cx-cancel (fail_execution + mark_trigger_event_handled +
            // lock release + terminal-action audit) so the pane lock and
            // trigger state don't leak. `workflow_total_deadline_ms == 0`
            // disables the deadline (legacy behavior).
            let deadline_ms = self.config.workflow_total_deadline_ms;
            if deadline_ms > 0 {
                let elapsed_ms =
                    u64::try_from(start_time.elapsed().as_millis()).unwrap_or(u64::MAX);
                let invalid_deadline_ms = deadline_ms > MAX_WORKFLOW_TOTAL_DEADLINE_MS;
                if invalid_deadline_ms || elapsed_ms >= deadline_ms {
                    let reason = if invalid_deadline_ms {
                        format!(
                            "run_workflow invalid overall deadline at step {current_step}: \
                             workflow_total_deadline_ms {deadline_ms}ms exceeds max \
                             {MAX_WORKFLOW_TOTAL_DEADLINE_MS}ms"
                        )
                    } else {
                        format!(
                            "run_workflow exceeded overall deadline at step {current_step}: \
                             elapsed {elapsed_ms}ms >= {deadline_ms}ms (workflow_total_deadline_ms)"
                        )
                    };
                    let mut reported_error = reason.clone();
                    if let Err(error) = self.fail_execution_with_cx(cx, execution_id, &reason).await
                    {
                        reported_error
                            .push_str(&format!("; failure-state persistence also failed: {error}"));
                    }
                    if let Err(error) = self
                        .mark_trigger_event_handled_with_cx(cx, execution_id, "error")
                        .await
                    {
                        reported_error
                            .push_str(&format!("; trigger-state persistence also failed: {error}"));
                    }
                    if let Err(error) = record_workflow_terminal_action_with_cx(
                        cx,
                        &self.storage,
                        &workflow_name,
                        execution_id,
                        pane_id,
                        "workflow_error",
                        "error",
                        Some(&reason),
                        Some(current_step),
                        None,
                        start_action_id,
                    )
                    .await
                    {
                        reported_error.push_str(&format!("; terminal audit also failed: {error}"));
                    }
                    return WorkflowExecutionResult::Error {
                        execution_id: Some(execution_id.to_string()),
                        error: reported_error,
                    };
                }
            }

            // Tick 181: per-step cancellation seam. On cx-cancel
            // we do the same cleanup sequence as a plan-validation
            // error (fail_execution + mark_trigger_event_handled +
            // lock release + terminal-action audit) so the pane
            // lock and trigger state don't leak under a cancelled
            // parent. Cleanup deliberately uses a new request-rooted Cx because
            // the caller's Cx has already crossed its cancellation boundary.
            if let Err(err) = cx.checkpoint() {
                let reason = format!("run_workflow cancelled at step {current_step}: {err}");
                let mut reported_error = reason.clone();
                if let Err(error) = self
                    .persist_terminal_failure_with_fresh_cx(
                        &workflow_name,
                        execution_id,
                        pane_id,
                        &reason,
                        current_step,
                        start_action_id,
                        "error",
                        "workflow_error",
                        "error",
                    )
                    .await
                {
                    reported_error.push_str(&format!("; independent cleanup also failed: {error}"));
                }
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: reported_error,
                };
            }

            let step_plan = ctx.get_step_plan(current_step).cloned();
            let mut idempotency_skip: Option<(i64, Option<String>)> = None;
            let mut idempotency_abort: Option<String> = None;
            let step_started_at = now_ms();

            if let Some(step_plan) = step_plan.as_ref() {
                if step_plan.idempotent {
                    match check_step_idempotency_with_cx(
                        cx,
                        &self.storage,
                        execution_id,
                        &step_plan.step_id,
                        current_step,
                    )
                    .await
                    {
                        IdempotencyCheckResult::AlreadyCompleted {
                            completed_at,
                            previous_result,
                        } => {
                            tracing::info!(
                                execution_id,
                                step_index = current_step,
                                step_id = %step_plan.step_id,
                                "Skipping idempotent step already completed"
                            );
                            idempotency_skip = Some((completed_at, previous_result));
                        }
                        IdempotencyCheckResult::PartiallyExecuted { started_at } => {
                            let reason = format!(
                                "Idempotent step {} was started at {} but not completed",
                                step_plan.step_id, started_at
                            );
                            tracing::warn!(
                                execution_id,
                                step_index = current_step,
                                step_id = %step_plan.step_id,
                                "Idempotent step partially executed; aborting"
                            );
                            idempotency_abort = Some(reason);
                        }
                        IdempotencyCheckResult::LedgerUnavailable => {
                            let reason = format!(
                                "Idempotency ledger unavailable for step {}; \
                                 failing closed without replay",
                                step_plan.step_id
                            );
                            tracing::warn!(
                                execution_id,
                                step_index = current_step,
                                step_id = %step_plan.step_id,
                                "Idempotency ledger unavailable; aborting"
                            );
                            idempotency_abort = Some(reason);
                        }
                        IdempotencyCheckResult::NotExecuted => {}
                    }
                }
            }

            // Execute the step (or skip/abort based on idempotency)
            let step_result = if let Some(reason) = idempotency_abort {
                StepResult::Abort { reason }
            } else if idempotency_skip.is_some() {
                StepResult::Continue
            } else {
                workflow.execute_step_cx(cx, &mut ctx, current_step).await
            };

            // Log step result
            let result_type = match &step_result {
                StepResult::Continue => "continue",
                StepResult::Done { .. } => "done",
                StepResult::Retry { .. } => "retry",
                StepResult::Abort { .. } => "abort",
                StepResult::WaitFor { .. } => "wait_for",
                StepResult::SendText { .. } => "send_text",
                StepResult::JumpTo { .. } => "jump_to",
            };

            let steps = workflow.steps();
            let step_name = steps
                .get(current_step)
                .map_or("unknown", |s| s.name.as_str());

            let step_plan_ref = step_plan.as_ref();
            let step_id = step_plan_ref.map(|step| step.step_id.0.clone());
            let step_kind = step_plan_ref.map(|step| step.action.action_type_name().to_string());
            let verification_refs = match build_verification_refs(&step_result, step_plan_ref) {
                Ok(refs) => refs,
                Err(error) => {
                    let reason = format!(
                        "Workflow verification metadata serialization failed at step \
                         {current_step}: {error}"
                    );
                    let error = self
                        .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                        .await;
                    return workflow_execution_error(execution_id, error);
                }
            };
            let step_error_code = step_error_code_from_result(&step_result);
            let log_step_result = redacted_step_result_for_logging(&step_result);

            if let Some(adapter) = self.replay_capture.as_ref() {
                let Some(step_definition) = steps.get(current_step) else {
                    let reason = format!(
                        "Workflow step definition missing at index {current_step} while step_count \
                         reported {step_count}"
                    );
                    let error = self
                        .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                        .await;
                    return workflow_execution_error(execution_id, error);
                };
                let step_definition_text = match serde_json::to_string(step_definition) {
                    Ok(text) => text,
                    Err(error) => {
                        let reason = format!(
                            "Workflow replay step-definition serialization failed at step \
                             {current_step}: {error}"
                        );
                        let error = self
                            .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                            .await;
                        return workflow_execution_error(execution_id, error);
                    }
                };
                let step_input = serde_json::json!({
                    "workflow_name": workflow_name.as_str(),
                    "execution_id": execution_id,
                    "pane_id": pane_id,
                    "step_index": current_step,
                    "step_name": step_name,
                    "trigger": ctx.trigger().cloned().unwrap_or(serde_json::Value::Null),
                });
                let step_input_text = match serde_json::to_string(&step_input) {
                    Ok(text) => text,
                    Err(error) => {
                        let reason = format!(
                            "Workflow replay input serialization failed at step {current_step}: \
                             {error}"
                        );
                        let error = self
                            .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                            .await;
                        return workflow_execution_error(execution_id, error);
                    }
                };
                let step_output = match serde_json::to_value(&log_step_result) {
                    Ok(output) => output,
                    Err(error) => {
                        let reason = format!(
                            "Workflow replay output serialization failed at step {current_step}: \
                             {error}"
                        );
                        let error = self
                            .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                            .await;
                        return workflow_execution_error(execution_id, error);
                    }
                };
                let decision_event = crate::replay_capture::DecisionEvent::new(
                    crate::replay_capture::DecisionType::WorkflowStep,
                    pane_id,
                    format!("workflow.{workflow_name}.step.{current_step}"),
                    &step_definition_text,
                    &step_input_text,
                    step_output,
                    Some(format!("workflow_execution:{execution_id}")),
                    None,
                    crate::recording::epoch_ms_now(),
                );
                if let Err(error) = adapter.capture_decision(
                    crate::recording::RecorderEventSource::WorkflowEngine,
                    Some(execution_id.to_string()),
                    decision_event,
                ) {
                    return WorkflowExecutionResult::Error {
                        execution_id: Some(execution_id.to_string()),
                        error: format!("replay capture rejected workflow step decision: {error}"),
                    };
                }
            }

            // Build result data, enriching with plan information if available (wa-upg.2.3)
            let result_data = {
                let mut data = serde_json::json!({
                    "step_result": &log_step_result,
                });

                // Include idempotency key from plan if executing in plan-first mode
                if let Some(idempotency_key) = ctx.get_step_idempotency_key(current_step) {
                    data["idempotency_key"] = serde_json::json!(idempotency_key.0);
                }

                // Include step action type from plan if available
                if let Some(step_plan) = step_plan_ref {
                    data["action_type"] = serde_json::json!(step_plan.action.action_type_name());
                    data["step_description"] = serde_json::json!(step_plan.description);
                }

                if let Some((completed_at, previous_result)) = &idempotency_skip {
                    data["idempotency_skip"] = serde_json::json!(true);
                    data["previous_completed_at"] = serde_json::json!(completed_at);
                    if let Some(previous_result) = previous_result {
                        data["previous_result"] = serde_json::json!(previous_result);
                    }
                }

                match serde_json::to_string(&data) {
                    Ok(serialized) => Some(serialized),
                    Err(error) => {
                        let reason = format!(
                            "Workflow step-data serialization failed at step {current_step}: \
                             {error}"
                        );
                        let error = self
                            .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                            .await;
                        return workflow_execution_error(execution_id, error);
                    }
                }
            };
            let step_completed_at = now_ms();

            // Persist step log for non-SendText steps
            // SendText steps are logged after injection to capture the audit_action_id (wa-nu4.1.1.11)
            if !matches!(&step_result, StepResult::SendText { .. }) {
                let step_audit_action_id = match record_workflow_step_action_with_cx(
                    cx,
                    &self.storage,
                    WorkflowStepActionInput {
                        workflow_name: &workflow_name,
                        execution_id,
                        pane_id,
                        step_index: current_step,
                        step_name,
                        step_id: step_id.clone(),
                        step_kind: step_kind.clone(),
                        result_type,
                        parent_action_id: start_action_id,
                    },
                )
                .await
                {
                    Ok(action_id) => action_id,
                    Err(error) => {
                        let reason =
                            format!("Workflow step audit failed at step {current_step}: {error}");
                        let error = self
                            .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                            .await;
                        return workflow_execution_error(execution_id, error);
                    }
                };

                if let Err(error) = self
                    .storage
                    .insert_step_log_with_cx(
                        cx,
                        execution_id,
                        Some(step_audit_action_id),
                        current_step,
                        step_name,
                        step_id.clone(),
                        step_kind.clone(),
                        result_type,
                        result_data.clone(),
                        None,
                        verification_refs.clone(),
                        step_error_code,
                        step_started_at,
                        step_completed_at,
                    )
                    .await
                {
                    let reason = format!(
                        "Workflow step-log persistence failed at step {current_step}: {error}"
                    );
                    let error = self
                        .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                        .await;
                    return workflow_execution_error(execution_id, error);
                }
            }

            // Handle step result
            match step_result {
                StepResult::Continue => {
                    current_step += 1;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self
                        .update_execution_step_with_cx(cx, execution_id, current_step)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step"
                        );
                        match e {
                            crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                                reason,
                            )) => {
                                if let Err(error) = record_workflow_terminal_action_with_cx(
                                    cx,
                                    &self.storage,
                                    &workflow_name,
                                    execution_id,
                                    pane_id,
                                    "workflow_aborted",
                                    "aborted",
                                    Some(&reason),
                                    Some(current_step),
                                    None,
                                    start_action_id,
                                )
                                .await
                                {
                                    return workflow_execution_error(
                                        execution_id,
                                        format!("{reason}; terminal audit failed: {error}"),
                                    );
                                }
                                return WorkflowExecutionResult::Aborted {
                                    execution_id: execution_id.to_string(),
                                    reason,
                                    step_index: current_step,
                                    elapsed_ms: elapsed_ms(start_time),
                                };
                            }
                            other => {
                                return WorkflowExecutionResult::Error {
                                    execution_id: Some(execution_id.to_string()),
                                    error: other.to_string(),
                                };
                            }
                        }
                    }
                }
                StepResult::JumpTo { step } => {
                    jump_count += 1;
                    if jump_count > max_total_jumps {
                        let reason = format!("exceeded maximum jump count ({max_total_jumps})");
                        tracing::error!(
                            execution_id,
                            jump_count,
                            "Workflow exceeded maximum jump count ({max_total_jumps}); aborting to prevent infinite loop"
                        );
                        workflow.cleanup(&mut ctx).await;
                        if let Err(error) = self
                            .persist_aborted_execution_with_cx(
                                cx,
                                execution_id,
                                &reason,
                                "aborted",
                            )
                            .await
                        {
                            return workflow_execution_error(
                                execution_id,
                                format!("{reason}; abort settlement failed: {error}"),
                            );
                        }
                        if let Err(error) = record_workflow_terminal_action_with_cx(
                            cx,
                            &self.storage,
                            &workflow_name,
                            execution_id,
                            pane_id,
                            "workflow_aborted",
                            "aborted",
                            Some(&reason),
                            Some(current_step),
                            Some(current_step.saturating_add(1)),
                            start_action_id,
                        )
                        .await
                        {
                            return workflow_execution_error(
                                execution_id,
                                format!("{reason}; terminal audit failed: {error}"),
                            );
                        }
                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason,
                            step_index: current_step,
                            elapsed_ms: elapsed_ms(start_time),
                        };
                    }
                    if let Some(reason) = invalid_jump_target_reason(step, step_count) {
                        tracing::error!(
                            execution_id,
                            current_step,
                            target_step = step,
                            step_count,
                            "Workflow jump target is outside the executable step range"
                        );

                        if let Err(e) = self
                            .persist_aborted_execution_with_cx(cx, execution_id, &reason, "aborted")
                            .await
                        {
                            tracing::error!(
                                execution_id,
                                error = %e,
                                "Failed to persist abort after invalid jump; not reporting aborted"
                            );
                            return WorkflowExecutionResult::Error {
                                execution_id: Some(execution_id.to_string()),
                                error: e.to_string(),
                            };
                        }
                        workflow.cleanup(&mut ctx).await;
                        if let Err(error) = record_workflow_terminal_action_with_cx(
                            cx,
                            &self.storage,
                            &workflow_name,
                            execution_id,
                            pane_id,
                            "workflow_aborted",
                            "aborted",
                            Some(&reason),
                            Some(current_step),
                            Some(current_step + 1),
                            start_action_id,
                        )
                        .await
                        {
                            return workflow_execution_error(
                                execution_id,
                                format!("{reason}; terminal audit failed: {error}"),
                            );
                        }

                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason,
                            step_index: current_step,
                            elapsed_ms: elapsed_ms(start_time),
                        };
                    }
                    current_step = step;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self
                        .update_execution_step_with_cx(cx, execution_id, current_step)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after jump"
                        );
                        match e {
                            crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                                reason,
                            )) => {
                                if let Err(error) = record_workflow_terminal_action_with_cx(
                                    cx,
                                    &self.storage,
                                    &workflow_name,
                                    execution_id,
                                    pane_id,
                                    "workflow_aborted",
                                    "aborted",
                                    Some(&reason),
                                    Some(current_step),
                                    None,
                                    start_action_id,
                                )
                                .await
                                {
                                    return workflow_execution_error(
                                        execution_id,
                                        format!("{reason}; terminal audit failed: {error}"),
                                    );
                                }
                                return WorkflowExecutionResult::Aborted {
                                    execution_id: execution_id.to_string(),
                                    reason,
                                    step_index: current_step,
                                    elapsed_ms: elapsed_ms(start_time),
                                };
                            }
                            other => {
                                return WorkflowExecutionResult::Error {
                                    execution_id: Some(execution_id.to_string()),
                                    error: other.to_string(),
                                };
                            }
                        }
                    }
                }
                StepResult::Done { result } => {
                    // Workflow completed
                    let elapsed_ms = elapsed_ms(start_time);

                    if let Err(e) = self
                        .persist_completed_execution_with_cx(cx, execution_id, Some(result.clone()))
                        .await
                    {
                        tracing::error!(
                            execution_id,
                            error = %e,
                            "Workflow completion persistence failed; not reporting completed"
                        );
                        return WorkflowExecutionResult::Error {
                            execution_id: Some(execution_id.to_string()),
                            error: e.to_string(),
                        };
                    }

                    // Release lock

                    if let Err(error) = record_workflow_terminal_action_with_cx(
                        cx,
                        &self.storage,
                        &workflow_name,
                        execution_id,
                        pane_id,
                        "workflow_completed",
                        "completed",
                        None,
                        Some(current_step),
                        Some(current_step + 1),
                        start_action_id,
                    )
                    .await
                    {
                        return workflow_execution_error(
                            execution_id,
                            format!("Workflow completed but terminal audit failed: {error}"),
                        );
                    }

                    return WorkflowExecutionResult::Completed {
                        execution_id: execution_id.to_string(),
                        result,
                        elapsed_ms,
                        steps_executed: current_step + 1,
                    };
                }
                StepResult::Retry { delay_ms } => {
                    let Some(next_retry_ordinal) =
                        admit_retry_ordinal(retries, self.config.max_retries_per_step)
                    else {
                        let elapsed_ms = elapsed_ms(start_time);
                        let reason = format!(
                            "Max retries ({}) exceeded at step {}",
                            self.config.max_retries_per_step, current_step
                        );

                        // Cleanup and release lock
                        workflow.cleanup(&mut ctx).await;

                        if let Err(error) = self
                            .persist_aborted_execution_with_cx(cx, execution_id, &reason, "failed")
                            .await
                        {
                            return workflow_execution_error(
                                execution_id,
                                format!("{reason}; failure settlement failed: {error}"),
                            );
                        }

                        if let Err(error) = record_workflow_terminal_action_with_cx(
                            cx,
                            &self.storage,
                            &workflow_name,
                            execution_id,
                            pane_id,
                            "workflow_aborted",
                            "aborted",
                            Some(&reason),
                            Some(current_step),
                            Some(current_step + 1),
                            start_action_id,
                        )
                        .await
                        {
                            return workflow_execution_error(
                                execution_id,
                                format!("{reason}; terminal audit failed: {error}"),
                            );
                        }

                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason,
                            step_index: current_step,
                            elapsed_ms,
                        };
                    };
                    retries = next_retry_ordinal;

                    let retry_delay = retry_backoff_delay(
                        delay_ms,
                        retries,
                        self.config.retry_backoff_multiplier,
                    );
                    let effective_delay_ms =
                        u64::try_from(retry_delay.as_millis()).unwrap_or(u64::MAX);
                    debug!(
                        execution_id,
                        step_index = current_step,
                        retry_attempt = retries,
                        base_delay_ms = delay_ms,
                        effective_delay_ms,
                        retry_backoff_multiplier = self.config.retry_backoff_multiplier,
                        "Workflow step scheduled for exponential-backoff retry"
                    );
                    // Cancellation during backoff must abort promptly instead
                    // of sleeping until the retry delay elapses.
                    if let Err(e) = wait_duration_with_cx(
                        cx,
                        cap_wait_by_workflow_deadline(
                            retry_delay,
                            start_time,
                            self.config.workflow_total_deadline_ms,
                        ),
                        "workflow retry backoff",
                    )
                    .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Workflow retry backoff aborted"
                        );
                        match e {
                            crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                                reason,
                            )) => {
                                if let Err(cleanup_error) = self
                                    .persist_cancelled_execution_with_fresh_cx(
                                        &workflow_name,
                                        execution_id,
                                        pane_id,
                                        &reason,
                                        current_step,
                                        start_action_id,
                                    )
                                    .await
                                {
                                    return workflow_execution_error(
                                        execution_id,
                                        format!(
                                            "{reason}; independent cancellation cleanup failed: \
                                             {cleanup_error}"
                                        ),
                                    );
                                }
                                return WorkflowExecutionResult::Aborted {
                                    execution_id: execution_id.to_string(),
                                    reason,
                                    step_index: current_step,
                                    elapsed_ms: elapsed_ms(start_time),
                                };
                            }
                            other => {
                                return WorkflowExecutionResult::Error {
                                    execution_id: Some(execution_id.to_string()),
                                    error: other.to_string(),
                                };
                            }
                        }
                    }
                }
                StepResult::Abort { reason } => {
                    let elapsed_ms = elapsed_ms(start_time);

                    // Cleanup and release lock
                    workflow.cleanup(&mut ctx).await;

                    if let Err(error) = self
                        .persist_aborted_execution_with_cx(cx, execution_id, &reason, "aborted")
                        .await
                    {
                        return workflow_execution_error(
                            execution_id,
                            format!("{reason}; abort settlement failed: {error}"),
                        );
                    }

                    if let Err(error) = record_workflow_terminal_action_with_cx(
                        cx,
                        &self.storage,
                        &workflow_name,
                        execution_id,
                        pane_id,
                        "workflow_aborted",
                        "aborted",
                        Some(&reason),
                        Some(current_step),
                        Some(current_step + 1),
                        start_action_id,
                    )
                    .await
                    {
                        return workflow_execution_error(
                            execution_id,
                            format!("{reason}; terminal audit failed: {error}"),
                        );
                    }

                    return WorkflowExecutionResult::Aborted {
                        execution_id: execution_id.to_string(),
                        reason,
                        step_index: current_step,
                        elapsed_ms,
                    };
                }
                StepResult::WaitFor {
                    condition,
                    timeout_ms,
                } => {
                    // Update execution to waiting
                    if let Err(e) = self
                        .set_execution_waiting_with_cx(cx, execution_id, current_step, &condition)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to set waiting state"
                        );
                        match e {
                            crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                                reason,
                            )) => {
                                if let Err(cleanup_error) = self
                                    .persist_cancelled_execution_with_fresh_cx(
                                        &workflow_name,
                                        execution_id,
                                        pane_id,
                                        &reason,
                                        current_step,
                                        start_action_id,
                                    )
                                    .await
                                {
                                    return workflow_execution_error(
                                        execution_id,
                                        format!(
                                            "{reason}; independent cancellation cleanup failed: \
                                             {cleanup_error}"
                                        ),
                                    );
                                }
                                return WorkflowExecutionResult::Aborted {
                                    execution_id: execution_id.to_string(),
                                    reason,
                                    step_index: current_step,
                                    elapsed_ms: elapsed_ms(start_time),
                                };
                            }
                            other => {
                                return WorkflowExecutionResult::Error {
                                    execution_id: Some(execution_id.to_string()),
                                    error: other.to_string(),
                                };
                            }
                        }
                    }

                    // Execute wait condition
                    let timeout = timeout_ms.map_or_else(
                        || Duration::from_millis(self.config.step_timeout_ms),
                        Duration::from_millis,
                    );
                    let timeout = cap_wait_by_workflow_deadline(
                        timeout,
                        start_time,
                        self.config.workflow_total_deadline_ms,
                    );

                    if let Err(e) = wait_condition_pause_with_cx(
                        cx,
                        &condition,
                        timeout,
                        self.external_signals.as_deref(),
                        "workflow wait condition",
                    )
                    .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Workflow wait condition aborted"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            if let Err(cleanup_error) = self
                                .persist_cancelled_execution_with_fresh_cx(
                                    &workflow_name,
                                    execution_id,
                                    pane_id,
                                    &reason,
                                    current_step,
                                    start_action_id,
                                )
                                .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!(
                                        "{reason}; independent cancellation cleanup failed: \
                                         {cleanup_error}"
                                    ),
                                );
                            }
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }

                        return WorkflowExecutionResult::Error {
                            execution_id: Some(execution_id.to_string()),
                            error: e.to_string(),
                        };
                    }

                    // Continue to next step after wait
                    current_step += 1;
                    retries = 0;

                    // Update execution back to running
                    if let Err(e) = self
                        .update_execution_step_with_cx(cx, execution_id, current_step)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after wait"
                        );
                        match e {
                            crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                                reason,
                            )) => {
                                if let Err(error) = record_workflow_terminal_action_with_cx(
                                    cx,
                                    &self.storage,
                                    &workflow_name,
                                    execution_id,
                                    pane_id,
                                    "workflow_aborted",
                                    "aborted",
                                    Some(&reason),
                                    Some(current_step),
                                    None,
                                    start_action_id,
                                )
                                .await
                                {
                                    return workflow_execution_error(
                                        execution_id,
                                        format!("{reason}; terminal audit failed: {error}"),
                                    );
                                }
                                return WorkflowExecutionResult::Aborted {
                                    execution_id: execution_id.to_string(),
                                    reason,
                                    step_index: current_step,
                                    elapsed_ms: elapsed_ms(start_time),
                                };
                            }
                            other => {
                                return WorkflowExecutionResult::Error {
                                    execution_id: Some(execution_id.to_string()),
                                    error: other.to_string(),
                                };
                            }
                        }
                    }
                }
                StepResult::SendText {
                    text,
                    wait_for,
                    wait_timeout_ms,
                } => {
                    // Attempt to send text via policy-gated injector
                    tracing::info!(
                        pane_id,
                        execution_id,
                        text_len = text.len(),
                        "Workflow requesting text injection"
                    );

                    let send_result = match self
                        .injector
                        .send_text(
                            cx,
                            pane_id,
                            &text,
                            crate::policy::ActorKind::Workflow,
                            ctx.capabilities(),
                            Some(execution_id),
                        )
                        .await
                    {
                        Ok(result) => result,
                        Err(error) => {
                            let reason = format!("Workflow text injection failed: {error}");
                            let mut reported_error = reason.clone();
                            if let Err(cleanup_error) = self
                                .persist_terminal_failure_with_fresh_cx(
                                    &workflow_name,
                                    execution_id,
                                    pane_id,
                                    &reason,
                                    current_step,
                                    start_action_id,
                                    "error",
                                    "workflow_error",
                                    "error",
                                )
                                .await
                            {
                                reported_error.push_str(&format!(
                                    "; independent failure cleanup also failed: {cleanup_error}"
                                ));
                            }
                            return workflow_execution_error(execution_id, reported_error);
                        }
                    };

                    // Log the SendText step with audit_action_id (wa-nu4.1.1.11)
                    let audit_action_id = send_result.audit_action_id();
                    let policy_summary = match policy_summary_from_injection(&send_result) {
                        Ok(summary) => Some(summary),
                        Err(error) => {
                            let reason = format!(
                                "Workflow policy-summary serialization failed at step \
                                 {current_step}: {error}"
                            );
                            let error = self
                                .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                                .await;
                            return workflow_execution_error(execution_id, error);
                        }
                    };
                    let policy_error_code = policy_error_code_from_injection(&send_result);
                    if let Err(error) = self
                        .storage
                        .insert_step_log_with_cx(
                            cx,
                            execution_id,
                            audit_action_id,
                            current_step,
                            step_name,
                            step_id.clone(),
                            step_kind.clone(),
                            "send_text",
                            result_data.clone(),
                            policy_summary,
                            verification_refs.clone(),
                            policy_error_code,
                            step_started_at,
                            now_ms(), // Use current time as completion
                        )
                        .await
                    {
                        let reason = format!(
                            "Workflow SendText step-log persistence failed at step \
                             {current_step}: {error}"
                        );
                        let error = self
                            .persist_failure_with_fresh_cx(execution_id, &reason, "error")
                            .await;
                        return workflow_execution_error(execution_id, error);
                    }

                    match send_result {
                        crate::policy::InjectionResult::Allowed { .. } => {
                            tracing::info!(pane_id, execution_id, "Text injection succeeded");

                            // If there's a wait condition, handle it
                            if let Some(condition) = wait_for {
                                let timeout = wait_timeout_ms.map_or_else(
                                    || Duration::from_millis(self.config.step_timeout_ms),
                                    Duration::from_millis,
                                );
                                let timeout = cap_wait_by_workflow_deadline(
                                    timeout,
                                    start_time,
                                    self.config.workflow_total_deadline_ms,
                                );

                                if let Err(e) = wait_condition_pause_with_cx(
                                    cx,
                                    &condition,
                                    timeout,
                                    self.external_signals.as_deref(),
                                    "workflow send-text verification wait",
                                )
                                .await
                                {
                                    tracing::warn!(
                                        execution_id,
                                        error = %e,
                                        "Workflow send-text wait aborted"
                                    );
                                    if let crate::Error::Workflow(
                                        crate::error::WorkflowError::Aborted(reason),
                                    ) = e
                                    {
                                        if let Err(cleanup_error) = self
                                            .persist_cancelled_execution_with_fresh_cx(
                                                &workflow_name,
                                                execution_id,
                                                pane_id,
                                                &reason,
                                                current_step,
                                                start_action_id,
                                            )
                                            .await
                                        {
                                            return workflow_execution_error(
                                                execution_id,
                                                format!(
                                                    "{reason}; independent cancellation cleanup \
                                                     failed: {cleanup_error}"
                                                ),
                                            );
                                        }
                                        return WorkflowExecutionResult::Aborted {
                                            execution_id: execution_id.to_string(),
                                            reason,
                                            step_index: current_step,
                                            elapsed_ms: elapsed_ms(start_time),
                                        };
                                    }

                                    return WorkflowExecutionResult::Error {
                                        execution_id: Some(execution_id.to_string()),
                                        error: e.to_string(),
                                    };
                                }
                            }

                            // Continue to next step
                            current_step += 1;
                            retries = 0;

                            if let Err(e) = self
                                .update_execution_step_with_cx(cx, execution_id, current_step)
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to update execution step after send"
                                );
                                match e {
                                    crate::Error::Workflow(
                                        crate::error::WorkflowError::Aborted(reason),
                                    ) => {
                                        if let Err(error) = record_workflow_terminal_action_with_cx(
                                            cx,
                                            &self.storage,
                                            &workflow_name,
                                            execution_id,
                                            pane_id,
                                            "workflow_aborted",
                                            "aborted",
                                            Some(&reason),
                                            Some(current_step),
                                            None,
                                            start_action_id,
                                        )
                                        .await
                                        {
                                            return workflow_execution_error(
                                                execution_id,
                                                format!("{reason}; terminal audit failed: {error}"),
                                            );
                                        }
                                        return WorkflowExecutionResult::Aborted {
                                            execution_id: execution_id.to_string(),
                                            reason,
                                            step_index: current_step,
                                            elapsed_ms: elapsed_ms(start_time),
                                        };
                                    }
                                    other => {
                                        return WorkflowExecutionResult::Error {
                                            execution_id: Some(execution_id.to_string()),
                                            error: other.to_string(),
                                        };
                                    }
                                }
                            }
                        }
                        crate::policy::InjectionResult::Denied { decision, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let reason = match &decision {
                                crate::policy::PolicyDecision::Deny { reason, .. } => {
                                    reason.clone()
                                }
                                _ => "Unknown denial reason".to_string(),
                            };
                            let abort_reason = format!("Policy denied text injection: {reason}");

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                reason = %reason,
                                "Text injection denied by policy"
                            );

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;

                            if let Err(error) = self
                                .persist_aborted_execution_with_cx(
                                    cx,
                                    execution_id,
                                    &abort_reason,
                                    "denied",
                                )
                                .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!(
                                        "{abort_reason}; policy-denial settlement failed: {error}"
                                    ),
                                );
                            }

                            if let Err(error) = record_workflow_terminal_action_with_cx(
                                cx,
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_policy_denied",
                                "policy_denied",
                                Some(&abort_reason),
                                Some(current_step),
                                Some(current_step + 1),
                                start_action_id,
                            )
                            .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!("{abort_reason}; terminal audit failed: {error}"),
                                );
                            }

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                        crate::policy::InjectionResult::RequiresApproval { decision, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let code = match &decision {
                                crate::policy::PolicyDecision::RequireApproval {
                                    approval, ..
                                } => approval.as_ref().map_or_else(
                                    || "unknown".to_string(),
                                    |a| a.allow_once_code.clone(),
                                ),
                                _ => "unknown".to_string(),
                            };
                            // Workflows do not currently suspend to wait for manual approval of steps.
                            // The approval code may be 'unknown' if not persisted by the injector.
                            let abort_reason = if code == "unknown" {
                                "Text injection requires human approval (workflow aborted)"
                                    .to_string()
                            } else {
                                format!("Text injection requires approval (code: {code})")
                            };

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                code = %code,
                                "Text injection requires approval"
                            );

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;

                            if let Err(error) = self
                                .persist_aborted_execution_with_cx(
                                    cx,
                                    execution_id,
                                    &abort_reason,
                                    "requires_approval",
                                )
                                .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!("{abort_reason}; approval settlement failed: {error}"),
                                );
                            }

                            if let Err(error) = record_workflow_terminal_action_with_cx(
                                cx,
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_requires_approval",
                                "requires_approval",
                                Some(&abort_reason),
                                Some(current_step),
                                Some(current_step + 1),
                                start_action_id,
                            )
                            .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!("{abort_reason}; terminal audit failed: {error}"),
                                );
                            }

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                        crate::policy::InjectionResult::Error { error, .. } => {
                            let elapsed_ms = elapsed_ms(start_time);
                            let abort_reason =
                                format!("Text injection failed after policy allowed: {error}");

                            tracing::error!(
                                pane_id,
                                execution_id,
                                error = %error,
                                "Text injection failed after policy allowed"
                            );

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;

                            if let Err(settlement_error) = self
                                .persist_aborted_execution_with_cx(
                                    cx,
                                    execution_id,
                                    &abort_reason,
                                    "error",
                                )
                                .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!(
                                        "{abort_reason}; injection-error settlement failed: \
                                         {settlement_error}"
                                    ),
                                );
                            }

                            if let Err(error) = record_workflow_terminal_action_with_cx(
                                cx,
                                &self.storage,
                                &workflow_name,
                                execution_id,
                                pane_id,
                                "workflow_error",
                                "error",
                                Some(&abort_reason),
                                Some(current_step),
                                Some(current_step + 1),
                                start_action_id,
                            )
                            .await
                            {
                                return workflow_execution_error(
                                    execution_id,
                                    format!("{abort_reason}; terminal audit failed: {error}"),
                                );
                            }

                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason: abort_reason,
                                step_index: current_step,
                                elapsed_ms,
                            };
                        }
                    }
                }
            }
        }

        // All steps completed without explicit Done
        let elapsed_ms = elapsed_ms(start_time);
        let result = serde_json::json!({ "status": "completed" });

        if let Err(e) = self
            .persist_completed_execution_with_cx(cx, execution_id, Some(result.clone()))
            .await
        {
            tracing::error!(
                execution_id,
                error = %e,
                "Workflow completion persistence failed; not reporting completed"
            );
            return WorkflowExecutionResult::Error {
                execution_id: Some(execution_id.to_string()),
                error: e.to_string(),
            };
        }

        if let Err(error) = record_workflow_terminal_action_with_cx(
            cx,
            &self.storage,
            &workflow_name,
            execution_id,
            pane_id,
            "workflow_completed",
            "completed",
            None,
            Some(step_count.saturating_sub(1)),
            Some(step_count),
            start_action_id,
        )
        .await
        {
            return workflow_execution_error(
                execution_id,
                format!("Workflow completed but terminal audit failed: {error}"),
            );
        }

        WorkflowExecutionResult::Completed {
            execution_id: execution_id.to_string(),
            result,
            elapsed_ms,
            steps_executed: step_count,
        }
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`run_workflow`].
    ///
    /// Tick 178: entry-point shim. Pre-flight `cx.checkpoint()`
    /// gates the workflow start — if the caller's cx is already
    /// cancelled after an earlier `handle_detection_with_cx` acquired
    /// the pane lock and persisted the execution record, this entry
    /// point now performs best-effort cleanup (release lock, mark the
    /// execution failed, mark the trigger handled) before surfacing
    /// `WorkflowExecutionResult::Error`.
    ///
    /// Routes through the shared `run_workflow_inner(cx, ...)` path so the
    /// complete internal workflow call graph carries one concrete Cx.
    ///
    /// Using the `Error` variant (not a new variant) keeps the
    /// match surface stable — every `run_workflow` caller already
    /// handles Error.
    pub async fn run_workflow_with_cx(
        &self,
        cx: &crate::cx::Cx,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        start_step: usize,
    ) -> WorkflowExecutionResult {
        let capacity_timer = crate::runtime_telemetry::SwarmCapacityStageTimer::start(
            crate::runtime_telemetry::SwarmCapacityStage::WorkflowRunner,
            0,
        );
        if let Err(err) = cx.checkpoint() {
            // ft-rlbvg: pre-start cancel path. Take a release
            // guard so the lock drops on every exit branch
            // (including panics inside `fail_execution` /
            // `mark_trigger_event_handled`).
            let _release_guard = self
                .lock_manager
                .held_lock_release_guard(pane_id, execution_id);
            let reason = format!("run_workflow cancelled pre-start: {err}");
            let cleanup_result = self
                .persist_cancelled_execution_with_fresh_cx(
                    workflow.name(),
                    execution_id,
                    pane_id,
                    &reason,
                    start_step,
                    None,
                )
                .await
                .map_err(|error| {
                    format!("{reason}; independent cancellation cleanup failed: {error}")
                });
            capacity_timer.finish_error(crate::runtime_telemetry::FailureClass::Safety);
            return WorkflowExecutionResult::Error {
                execution_id: Some(execution_id.to_string()),
                error: cleanup_result.err().unwrap_or(reason),
            };
        }
        let result = self
            .run_workflow_inner(cx, pane_id, workflow, execution_id, start_step)
            .await;
        match &result {
            WorkflowExecutionResult::Completed { .. } => capacity_timer.finish_completion(),
            WorkflowExecutionResult::Aborted { .. } => capacity_timer.finish_cancellation(),
            WorkflowExecutionResult::PolicyDenied { .. } => {
                capacity_timer.finish_error(crate::runtime_telemetry::FailureClass::Safety);
            }
            WorkflowExecutionResult::Error { .. } => {
                capacity_timer.finish_error(crate::runtime_telemetry::FailureClass::Transient);
            }
        }
        result
    }

    /// Run the event loop, subscribing to detection events.
    ///
    /// This spawns workflow executions for matching detections. The loop
    /// runs until the event bus channel is closed.
    ///
    /// On startup, resumes any incomplete workflows that were interrupted
    /// (e.g., by a previous watcher crash or restart).
    /// Run the workflow runner.
    ///
    /// ft-dit9w: ergonomic wrapper around [`Self::run_with_cx`].
    /// Constructs a request-rooted cx (or borrows the ambient one)
    /// so the entire runner loop — including the spawned per-workflow
    /// execution tasks — runs under the RuntimeProof seal.
    pub async fn run(&self, event_bus: &crate::events::EventBus) {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.run_with_cx(&cx, event_bus).await;
    }
    /// ft-xbnl0.2.3 Cx-first sibling of [`run`] (tick 223).
    ///
    /// Threads the caller's cx through the entire event-loop
    /// driver:
    /// - `resume_incomplete_with_cx(cx)` on startup (tick 222)
    /// - `subscriber.recv_cx(cx)` for detection reads (cancel-aware
    ///   instead of ambient `recv`)
    /// - Top-of-loop `cx.checkpoint()` lets a cx-cancelled caller
    ///   break the loop cleanly between events.
    /// - `handle_detection_with_cx(cx, ...)` instead of ambient
    ///   `handle_detection` — tick-190 migration means this threads
    ///   cx into `engine.start_with_id_cx`.
    /// - Per-spawn `child_cx = cx.clone()` into each spawned
    ///   workflow-execution task, and `run_workflow_with_cx`
    ///   inside the spawn so the execution chain is cx-threaded
    ///   end-to-end (ticks 178-187).
    ///
    /// Callers (watcher startup) can now thread an operator-scoped
    /// cx into the runner loop. A cx-cancel at any point bubbles
    /// through cleanly: the loop exits on cancel, and the
    /// child tasks already in flight terminate at their next
    /// cx-observing await.
    ///
    /// Legacy [`run`](Self::run) preserved unchanged.
    pub async fn run_with_cx(&self, cx: &crate::cx::Cx, event_bus: &crate::events::EventBus) {
        self.run_loop_with_cx(cx, event_bus, None).await;
    }

    /// Run the event loop with an explicit cooperative shutdown channel.
    ///
    /// The atomic flag is the durable predicate and `shutdown_notify` is its
    /// wake accelerator. They must be the same pair owned by the surrounding
    /// [`crate::watchdog::WatchdogHandle`]. On shutdown, the runner stops
    /// admitting detections, aborts every owned per-workflow task, and performs
    /// a bounded trusted terminal drain before returning.
    pub async fn run_with_shutdown_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_bus: &crate::events::EventBus,
        shutdown_flag: &std::sync::atomic::AtomicBool,
        shutdown_notify: &crate::runtime_async::notify::Notify,
    ) {
        self.run_loop_with_cx(
            cx,
            event_bus,
            Some((shutdown_flag, shutdown_notify)),
        )
        .await;
    }

    async fn run_loop_with_cx(
        &self,
        cx: &crate::cx::Cx,
        event_bus: &crate::events::EventBus,
        shutdown: Option<(
            &std::sync::atomic::AtomicBool,
            &crate::runtime_async::notify::Notify,
        )>,
    ) {
        if shutdown.is_some_and(|(flag, _)| flag.load(Ordering::SeqCst)) {
            tracing::info!(
                explicit_cx = true,
                "Workflow runner shutdown was already requested before resume"
            );
            return;
        }
        let resumed = self.resume_incomplete_with_cx(cx).await;
        if !resumed.is_empty() {
            tracing::info!(
                count = resumed.len(),
                explicit_cx = true,
                "Resumed incomplete workflows from previous run (cx)"
            );
            for result in &resumed {
                match result {
                    WorkflowExecutionResult::Completed { execution_id, .. } => {
                        tracing::info!(
                            execution_id,
                            explicit_cx = true,
                            "Resumed workflow completed (cx)"
                        );
                    }
                    WorkflowExecutionResult::Error {
                        execution_id,
                        error,
                    } => {
                        tracing::warn!(
                            ?execution_id,
                            error,
                            explicit_cx = true,
                            "Resumed workflow errored (cx)"
                        );
                    }
                    _ => {}
                }
            }
        }

        let mut subscriber = event_bus.subscribe_detections();
        let mut child_tasks = crate::runtime_async::task::JoinSet::new();

        loop {
            while let Some(join_result) = child_tasks.try_join_next() {
                if let Err(error) = join_result {
                    tracing::warn!(
                        failure_class = ?error.kind(),
                        "Workflow child task failed"
                    );
                }
            }

            if cx.checkpoint().is_err() {
                tracing::info!(
                    explicit_cx = true,
                    "Workflow runner loop cancelled via Cx, stopping"
                );
                break;
            }
            if shutdown.is_some_and(|(flag, _)| flag.load(Ordering::SeqCst)) {
                tracing::info!(
                    explicit_cx = true,
                    "Workflow runner received shutdown signal"
                );
                break;
            }

            let next_event = match wait_for_workflow_runner_activity(
                cx,
                &mut subscriber,
                &mut child_tasks,
                shutdown,
            )
            .await
            {
                WorkflowRunnerWake::Shutdown => {
                    tracing::info!(
                        explicit_cx = true,
                        "Workflow runner received shutdown notification"
                    );
                    break;
                }
                WorkflowRunnerWake::Event(event) => *event,
                WorkflowRunnerWake::Child(Ok(Some(Ok(())))) => continue,
                WorkflowRunnerWake::Child(Ok(Some(Err(error)))) => {
                    tracing::warn!(
                        failure_class = ?error.kind(),
                        "Workflow child task failed"
                    );
                    continue;
                }
                WorkflowRunnerWake::Child(Ok(None)) => continue,
                WorkflowRunnerWake::Child(Err(error)) => {
                    tracing::warn!(
                        failure_class = ?error.kind(),
                        "Workflow child task drain wait failed"
                    );
                    continue;
                }
            };

            match next_event {
                Ok(event) => {
                    if let crate::events::Event::PatternDetected {
                        pane_id,
                        pane_uuid: _,
                        detection,
                        event_id,
                    } = event
                    {
                        let result = self
                            .handle_detection_with_cx(cx, pane_id, &detection, event_id)
                            .await;

                        match result {
                            WorkflowStartResult::Started {
                                execution_id,
                                workflow_name,
                            } => {
                                if let Some(workflow) = self.find_workflow_by_name(&workflow_name) {
                                    let execution_id_clone = execution_id.clone();
                                    let workflow_clone = Arc::clone(&workflow);
                                    let storage = Arc::clone(&self.storage);
                                    let lock_manager = Arc::clone(&self.lock_manager);
                                    let config = self.config.clone();
                                    let engine = WorkflowEngine::new(config.max_concurrent);

                                    let runner = Self {
                                        workflows: std::sync::RwLock::new(vec![
                                            workflow_clone.clone(),
                                        ]),
                                        engine,
                                        lock_manager,
                                        storage,
                                        injector: self.injector.clone(),
                                        config,
                                        replay_capture: self.replay_capture.clone(),
                                        external_signals: self.external_signals.clone(),
                                    };

                                    child_tasks.spawn_with_cx(cx, move |child_cx| async move {
                                        let result = runner
                                            .run_workflow_with_cx(
                                                &child_cx,
                                                pane_id,
                                                workflow_clone,
                                                &execution_id_clone,
                                                0,
                                            )
                                            .await;

                                        match &result {
                                            WorkflowExecutionResult::Completed {
                                                execution_id,
                                                steps_executed,
                                                elapsed_ms,
                                                ..
                                            } => {
                                                tracing::info!(
                                                    execution_id,
                                                    steps = steps_executed,
                                                    elapsed_ms,
                                                    explicit_cx = true,
                                                    "Workflow completed (cx)"
                                                );
                                            }
                                            WorkflowExecutionResult::Aborted {
                                                execution_id,
                                                reason,
                                                step_index,
                                                ..
                                            } => {
                                                tracing::warn!(
                                                    execution_id,
                                                    step = step_index,
                                                    reason,
                                                    explicit_cx = true,
                                                    "Workflow aborted (cx)"
                                                );
                                            }
                                            WorkflowExecutionResult::PolicyDenied {
                                                execution_id,
                                                step_index,
                                                reason,
                                            } => {
                                                tracing::warn!(
                                                    execution_id,
                                                    step = step_index,
                                                    reason,
                                                    explicit_cx = true,
                                                    "Workflow denied by policy (cx)"
                                                );
                                            }
                                            WorkflowExecutionResult::Error {
                                                execution_id,
                                                error,
                                            } => {
                                                tracing::error!(
                                                    execution_id = execution_id.as_deref(),
                                                    error,
                                                    explicit_cx = true,
                                                    "Workflow error (cx)"
                                                );
                                            }
                                        }
                                    });
                                } else {
                                    // The workflow that
                                    // `handle_detection_with_cx` matched and
                                    // started vanished from the registry before
                                    // we could spawn its runner. The success
                                    // path already `defuse()`d the pane lock
                                    // (handing release ownership to the spawned
                                    // `run_workflow_inner`), so skipping the
                                    // spawn would leak the lock and brick the
                                    // pane for every future workflow
                                    // (`PaneLocked` forever). Release the
                                    // orphaned lock so the pane recovers.
                                    tracing::error!(
                                        workflow_name = %workflow_name,
                                        execution_id = %execution_id,
                                        pane_id,
                                        explicit_cx = true,
                                        "Started workflow missing from registry before spawn; releasing orphaned pane lock (cx)"
                                    );
                                    self.lock_manager.release(pane_id, &execution_id);
                                }
                            }
                            WorkflowStartResult::NoMatchingWorkflow { rule_id } => {
                                tracing::debug!(
                                    rule_id,
                                    explicit_cx = true,
                                    "No workflow handles detection (cx)"
                                );
                            }
                            WorkflowStartResult::PaneLocked {
                                pane_id,
                                held_by_workflow,
                                ..
                            } => {
                                tracing::debug!(
                                    pane_id,
                                    held_by = %held_by_workflow,
                                    explicit_cx = true,
                                    "Pane locked, skipping detection (cx)"
                                );
                            }
                            WorkflowStartResult::ConcurrencyLimitReached { active, limit } => {
                                tracing::debug!(
                                    active,
                                    limit,
                                    explicit_cx = true,
                                    "Workflow concurrency limit reached, skipping detection (cx)"
                                );
                            }
                            WorkflowStartResult::SourcePaneNotTrusted {
                                source_pane_id,
                                workflow_name,
                                rule_id,
                            } => {
                                tracing::warn!(
                                    source_pane_id,
                                    workflow = %workflow_name,
                                    rule_id,
                                    explicit_cx = true,
                                    "ft-j0ufc: trigger refused (source pane not in trust scope) (cx)"
                                );
                            }
                            WorkflowStartResult::PaneRateLimited {
                                pane_id,
                                workflow_name,
                                rule_id,
                                reset_at_ms,
                                reset_known,
                            } => {
                                tracing::info!(
                                    pane_id,
                                    workflow = %workflow_name,
                                    rule_id,
                                    reset_at_ms,
                                    reset_known,
                                    explicit_cx = true,
                                    "ft-7h5da.8.3: trigger declined (target pane rate-limited) (cx)"
                                );
                            }
                            WorkflowStartResult::Error { error } => {
                                tracing::error!(
                                    error,
                                    explicit_cx = true,
                                    "Failed to start workflow (cx)"
                                );
                            }
                        }
                    }
                }
                Err(crate::events::RecvError::Lagged { missed_count }) => {
                    tracing::warn!(
                        skipped = missed_count,
                        explicit_cx = true,
                        "Workflow runner lagged, skipped events (cx)"
                    );
                }
                Err(crate::events::RecvError::Cancelled) => {
                    tracing::info!(
                        explicit_cx = true,
                        "Workflow runner subscriber cancelled via Cx, stopping"
                    );
                    break;
                }
                Err(crate::events::RecvError::Closed) => {
                    tracing::info!(
                        explicit_cx = true,
                        "Event bus closed, workflow runner stopping (cx)"
                    );
                    break;
                }
            }
        }

        match settle_workflow_runner_children(&mut child_tasks).await {
            WorkflowRunnerChildDrainOutcome::Settled => {}
            WorkflowRunnerChildDrainOutcome::TimedOut {
                active_tasks,
                unacknowledged_tasks,
            } => {
                tracing::warn!(
                    event = "workflow_runner_child_task_drain_timeout",
                    active_tasks,
                    unacknowledged_tasks,
                    remaining_tasks = child_tasks.len(),
                    orphan_risk = true,
                    "Workflow child tasks missed bounded terminal settlement"
                );
            }
            WorkflowRunnerChildDrainOutcome::Incomplete {
                active_tasks,
                unacknowledged_tasks,
            } => {
                tracing::warn!(
                    event = "workflow_runner_child_task_settlement_incomplete",
                    active_tasks,
                    unacknowledged_tasks,
                    remaining_tasks = child_tasks.len(),
                    orphan_risk = true,
                    "Workflow child task drain ended without terminal settlement"
                );
            }
        }
    }

    /// Resume incomplete workflows after restart.
    ///
    /// Queries storage for workflows with status 'running' or 'waiting'
    /// and attempts to resume them.
    /// Resume incomplete workflows after restart.
    ///
    /// ft-dit9w: ergonomic wrapper around [`Self::resume_incomplete_with_cx`].
    pub async fn resume_incomplete(&self) -> Vec<WorkflowExecutionResult> {
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.resume_incomplete_with_cx(&cx).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`resume_incomplete`]
    /// (tick 222). Final orphan from tick-187's pending list.
    ///
    /// Threads caller cx through the entire restart-resume chain:
    /// - `find_incomplete_workflows_with_cx(cx)` for the initial read
    /// - Per-record `cx.checkpoint()` gates entry to each resume
    /// - `engine.resume_cx(cx, &storage, &record.id)` for the state
    ///   load (tick 188 migration)
    /// - `run_workflow_with_cx(cx, ...)` for the actual execution
    ///   (tick 178 entry — runs through run_workflow_inner which is
    ///   fully cx-threaded as of ticks 178-187)
    ///
    /// A cancel fired mid-restart-resume cleanly aborts between
    /// workflow records instead of resuming all N incomplete ones.
    pub async fn resume_incomplete_with_cx(
        &self,
        cx: &crate::cx::Cx,
    ) -> Vec<WorkflowExecutionResult> {
        let incomplete = match self.storage.find_incomplete_workflows_with_cx(cx).await {
            Ok(workflows) => workflows,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    explicit_cx = true,
                    "Failed to query incomplete workflows (cx)"
                );
                return vec![];
            }
        };

        let mut results = Vec::new();

        for record in incomplete {
            if cx.checkpoint().is_err() {
                tracing::info!(
                    explicit_cx = true,
                    "resume_incomplete_with_cx: cancelled between workflow records"
                );
                break;
            }

            let Some(workflow) = self.find_workflow_by_name(&record.workflow_name) else {
                tracing::warn!(
                    workflow_name = %record.workflow_name,
                    execution_id = %record.id,
                    explicit_cx = true,
                    "Cannot resume: workflow not registered (cx)"
                );
                continue;
            };

            let (execution, next_step) =
                match self.engine.resume_cx(cx, &self.storage, &record.id).await {
                    Ok(Some(resume)) => resume,
                    Ok(None) => {
                        tracing::debug!(
                            execution_id = %record.id,
                            explicit_cx = true,
                            "Skipping resume for workflow already in a terminal state (cx)"
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            execution_id = %record.id,
                            error = %e,
                            explicit_cx = true,
                            "Failed to load workflow state for resume (cx)"
                        );
                        continue;
                    }
                };

            // ft-j0ufc: re-enforce the source-pane trust scope on resume. The
            // trigger-time check in `handle_detection_with_cx` is point-in-time;
            // a restart must not blindly resume a workflow whose source pane is
            // no longer in the (possibly tightened) trust scope and then run its
            // remaining `SendText` steps. Gate before lock acquisition, exactly
            // as the trigger path does. The source pane equals the acted-on pane
            // at trigger time (`source_pane_id = pane_id`), so `execution.pane_id`
            // is the correct re-check key.
            if !workflow
                .trigger_policy()
                .allows_source_pane(execution.pane_id)
            {
                tracing::warn!(
                    execution_id = %execution.id,
                    pane_id = execution.pane_id,
                    workflow = %execution.workflow_name,
                    explicit_cx = true,
                    "ft-j0ufc: refusing resume; source pane no longer in trust scope (cx)"
                );
                continue;
            }

            let lock_result = self.lock_manager.try_acquire(
                execution.pane_id,
                &execution.workflow_name,
                &execution.id,
            );

            match lock_result {
                LockAcquisitionResult::AlreadyLocked { .. } => {
                    tracing::warn!(
                        execution_id = %execution.id,
                        pane_id = execution.pane_id,
                        explicit_cx = true,
                        "Cannot resume: pane locked (cx)"
                    );
                    continue;
                }
                LockAcquisitionResult::Acquired => {}
            }

            tracing::info!(
                execution_id = %execution.id,
                workflow = %execution.workflow_name,
                pane_id = execution.pane_id,
                resume_step = next_step,
                explicit_cx = true,
                "Resuming workflow (cx)"
            );

            let result = self
                .run_workflow_with_cx(cx, execution.pane_id, workflow, &execution.id, next_step)
                .await;

            results.push(result);
        }

        results
    }

    // --- Private helper methods ---

    #[allow(clippy::too_many_arguments)]
    async fn persist_terminal_failure_with_fresh_cx(
        &self,
        workflow_name: &str,
        execution_id: &str,
        pane_id: u64,
        reason: &str,
        current_step: usize,
        start_action_id: Option<i64>,
        handled_status: &str,
        action_kind: &str,
        action_result: &str,
    ) -> crate::Result<()> {
        let cleanup_cx = crate::cx::for_request();
        let cleanup = async {
            let mut failures = Vec::new();
            if let Err(error) = self
                .persist_aborted_execution_with_cx(
                    &cleanup_cx,
                    execution_id,
                    reason,
                    handled_status,
                )
                .await
            {
                failures.push(format!("state/trigger settlement failed: {error}"));
            }
            if let Err(error) = record_workflow_terminal_action_with_cx(
                &cleanup_cx,
                &self.storage,
                workflow_name,
                execution_id,
                pane_id,
                action_kind,
                action_result,
                Some(reason),
                Some(current_step),
                None,
                start_action_id,
            )
            .await
            {
                failures.push(format!("terminal audit failed: {error}"));
            }
            if failures.is_empty() {
                Ok(())
            } else {
                Err(crate::Error::Workflow(
                    crate::error::WorkflowError::Aborted(format!(
                        "independent terminal cleanup incomplete: {}",
                        failures.join("; ")
                    )),
                ))
            }
        };

        match crate::runtime_async::timeout_with_cx(
            &cleanup_cx,
            WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT,
            cleanup,
        )
        .await
        {
            Ok(result) => result,
            Err(error) => Err(workflow_runner_cancelled(
                "workflow.terminal_cleanup",
                format!(
                    "independent cleanup exceeded {:?}: {error}",
                    WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT
                ),
            )),
        }
    }

    async fn persist_failure_with_fresh_cx(
        &self,
        execution_id: &str,
        reason: &str,
        handled_status: &str,
    ) -> String {
        let cleanup_cx = crate::cx::for_request();
        let cleanup = self.persist_aborted_execution_with_cx(
            &cleanup_cx,
            execution_id,
            reason,
            handled_status,
        );
        match crate::runtime_async::timeout_with_cx(
            &cleanup_cx,
            WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT,
            cleanup,
        )
        .await
        {
            Ok(Ok(())) => reason.to_string(),
            Ok(Err(cleanup_error)) => {
                format!("{reason}; independent cleanup also failed: {cleanup_error}")
            }
            Err(timeout_error) => format!(
                "{reason}; independent cleanup exceeded {:?}: {timeout_error}",
                WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT
            ),
        }
    }

    /// Updates per-step progress using one explicit capability context.
    /// Routes both the read (get_workflow_with_cx) and
    /// the write (upsert_workflow_with_cx) through storage cx-first
    /// siblings. The "externally modified" check stays inline —
    /// it's a pure status-enum match on the already-fetched record
    /// so no cx threading is needed.
    ///
    /// This is the per-step progress-tracking write, called 4x
    /// inside `run_workflow_inner`'s step loop on the normal
    /// (non-error) path. Threading cx here means a cancelled
    /// parent cx releases the writer-queue reserve immediately
    /// under backpressure instead of waiting for drain.
    async fn update_execution_step_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        step: usize,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow_with_cx(cx, execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        if record.status == "aborted" || record.status == "failed" || record.status == "completed" {
            return Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "Workflow externally modified to status: {}",
                    record.status
                )),
            ));
        }

        record.current_step = step;
        record.status = "running".to_string();
        record.wait_condition = None;
        record.updated_at = now_ms();

        self.storage.upsert_workflow_with_cx(cx, record).await
    }

    /// Persists a waiting state under the workflow's explicit Cx.
    async fn set_execution_waiting_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        step: usize,
        condition: &WaitCondition,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow_with_cx(cx, execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        if record.status == "aborted" || record.status == "failed" || record.status == "completed" {
            return Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "Workflow externally modified to status: {}",
                    record.status
                )),
            ));
        }

        record.current_step = step;
        record.status = "waiting".to_string();
        record.wait_condition = Some(serde_json::to_value(condition)?);
        record.updated_at = now_ms();

        self.storage.upsert_workflow_with_cx(cx, record).await
    }

    /// Persists completion and its usage metric with the workflow Cx.
    /// Routes get_workflow + upsert_workflow + record_usage_metric
    /// through Cx-first calls and preserves every persistence failure.
    async fn complete_execution_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        result: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow_with_cx(cx, execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        record.status = "completed".to_string();
        record.result = result;
        let now = now_ms();
        record.updated_at = now;
        record.completed_at = Some(now);

        let duration_ms = now.saturating_sub(record.started_at);
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let metric = crate::storage::UsageMetricRecord {
            id: 0,
            timestamp: now,
            metric_type: crate::storage::MetricType::WorkflowCost,
            pane_id: Some(pane_id),
            agent_type: None,
            account_id: None,
            workflow_id: Some(record.id.clone()),
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: Some(
                serde_json::json!({
                    "source": "workflow.runner",
                    "workflow_name": workflow_name,
                    "status": "completed",
                    "duration_ms": duration_ms,
                })
                .to_string(),
            ),
            created_at: now,
        };

        self.storage.upsert_workflow_with_cx(cx, record).await?;
        self.storage
            .record_usage_metric_with_cx(cx, metric)
            .await
            .map(|_| ())
    }

    async fn persist_completed_execution_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        result: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        // Completion persistence can fail after the workflow row itself is
        // durable (for example, while writing the usage metric). Trigger
        // settlement must still be attempted or the source event remains
        // replayable and can duplicate already-completed side effects.
        let completion_result = self
            .complete_execution_with_cx(cx, execution_id, result)
            .await;
        let trigger_result = self
            .mark_trigger_event_handled_with_cx(cx, execution_id, "completed")
            .await;
        match (completion_result, trigger_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Err(completion_error), Err(trigger_error)) => Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "workflow completion persistence failed: {completion_error}; trigger \
                     settlement failed: {trigger_error}"
                )),
            )),
        }
    }

    async fn persist_aborted_execution_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        reason: &str,
        handled_status: &str,
    ) -> crate::Result<()> {
        let failure_result = self.fail_execution_with_cx(cx, execution_id, reason).await;
        let trigger_result = self
            .mark_trigger_event_handled_with_cx(cx, execution_id, handled_status)
            .await;
        match (failure_result, trigger_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Err(failure_error), Err(trigger_error)) => Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "workflow failure-state persistence failed: {failure_error}; trigger \
                     settlement failed: {trigger_error}"
                )),
            )),
        }
    }

    /// Persists failure and its usage metric with an explicit Cx.
    /// All storage calls route through Cx-first siblings, and metric failures
    /// remain typed instead of being mistaken for a fully persisted outcome.
    async fn fail_execution_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        error: &str,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow_with_cx(cx, execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        record.status = "failed".to_string();
        record.error = Some(error.to_string());
        let now = now_ms();
        record.updated_at = now;
        record.completed_at = Some(now);

        let duration_ms = now.saturating_sub(record.started_at);
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let metric = crate::storage::UsageMetricRecord {
            id: 0,
            timestamp: now,
            metric_type: crate::storage::MetricType::WorkflowCost,
            pane_id: Some(pane_id),
            agent_type: None,
            account_id: None,
            workflow_id: Some(record.id.clone()),
            count: Some(1),
            amount: None,
            tokens: None,
            metadata: Some(
                serde_json::json!({
                    "source": "workflow.runner",
                    "workflow_name": workflow_name,
                    "status": "failed",
                    "duration_ms": duration_ms,
                })
                .to_string(),
            ),
            created_at: now,
        };

        self.storage.upsert_workflow_with_cx(cx, record).await?;
        self.storage
            .record_usage_metric_with_cx(cx, metric)
            .await
            .map(|_| ())
    }

    /// Mark the triggering event as handled after workflow completion.
    ///
    /// This ensures proper event lifecycle management - events that triggered
    /// workflows are marked with the outcome so they won't be re-processed.
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID
    /// * `status` - The handling status ("completed", "failed", "aborted", "denied")
    ///
    /// Routes get_workflow + mark_event_handled through one explicit Cx.
    async fn mark_trigger_event_handled_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        status: &str,
    ) -> crate::Result<()> {
        let record = self.storage.get_workflow_with_cx(cx, execution_id).await?;

        if let Some(record) = record {
            if let Some(event_id) = record.trigger_event_id {
                self.storage
                    .mark_event_handled_with_cx(
                        cx,
                        event_id,
                        Some(execution_id.to_string()),
                        status,
                    )
                    .await?;

                tracing::debug!(
                    execution_id,
                    event_id,
                    status,
                    "Marked trigger event as handled (cx path)"
                );
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn persist_cancelled_execution_with_fresh_cx(
        &self,
        workflow_name: &str,
        execution_id: &str,
        pane_id: u64,
        reason: &str,
        current_step: usize,
        start_action_id: Option<i64>,
    ) -> crate::Result<()> {
        self.persist_terminal_failure_with_fresh_cx(
            workflow_name,
            execution_id,
            pane_id,
            reason,
            current_step,
            start_action_id,
            "aborted",
            "workflow_aborted",
            "aborted",
        )
        .await
    }

    async fn settle_aborted_trigger_with_fresh_cx(&self, execution_id: &str) -> crate::Result<()> {
        let cleanup_cx = crate::cx::for_request();
        match crate::runtime_async::timeout_with_cx(
            &cleanup_cx,
            WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT,
            self.mark_trigger_event_handled_with_cx(&cleanup_cx, execution_id, "aborted"),
        )
        .await
        {
            Ok(result) => result,
            Err(error) => Err(workflow_runner_cancelled(
                "workflow.abort_trigger_settlement",
                format!(
                    "independent trigger settlement exceeded {:?}: {error}",
                    WORKFLOW_INDEPENDENT_CLEANUP_TIMEOUT
                ),
            )),
        }
    }

    /// Abort a running workflow execution.
    ///
    /// This is the external API for aborting workflows (e.g., from robot mode).
    /// It differs from internal abort handling in that:
    /// 1. It validates the execution state before aborting
    /// 2. It releases the pane lock if held
    /// 3. It returns detailed abort information
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID to abort
    /// * `reason` - Optional reason for the abort (recorded in audit)
    /// * `force` - If true, skip cleanup steps
    ///
    /// # Returns
    /// * `Ok(AbortResult)` - Details about the aborted workflow
    /// * `Err` - If the workflow doesn't exist or is in invalid state
    pub async fn abort_execution(
        &self,
        execution_id: &str,
        reason: Option<&str>,
        force: bool,
    ) -> crate::Result<AbortResult> {
        // ft-dit9w: ergonomic wrapper around `abort_execution_with_cx`.
        let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        self.abort_execution_with_cx(&cx, execution_id, reason, force)
            .await
    }

    /// Cx-first variant of [`WorkflowRunner::abort_execution`]
    /// (ft-xbnl0.2.2).
    ///
    /// Short-circuits with a cancelled-error when the caller's
    /// asupersync capability context is already cancelled on entry,
    /// before issuing any storage reads or lock releases. This matches
    /// the `handle_detection_with_cx` contract so a cancelled operator
    /// request cannot accidentally mutate workflow state during
    /// teardown.
    pub async fn abort_execution_with_cx(
        &self,
        cx: &crate::cx::Cx,
        execution_id: &str,
        reason: Option<&str>,
        _force: bool,
    ) -> crate::Result<AbortResult> {
        if cx.is_cancel_requested() {
            return Err(workflow_runner_cancelled(
                "workflow.abort_execution",
                "capability context already cancelled; abort_execution refused",
            ));
        }

        // ft-xbnl0.2.3 tick 131: deepened from pre-flight delegate
        // to fully cx-threaded body. Routes both internal storage
        // calls through cx-first siblings so caller cancellation
        // propagates into storage's pre-flight checkpoints.

        let record = self
            .storage
            .get_workflow_with_cx(cx, execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if already in terminal state
        match record.status.as_str() {
            "completed" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_completed".to_string()),
                });
            }
            "aborted" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_aborted".to_string()),
                });
            }
            "failed" => {
                return Ok(AbortResult {
                    aborted: false,
                    execution_id: execution_id.to_string(),
                    workflow_name: record.workflow_name,
                    pane_id: record.pane_id,
                    previous_status: record.status.clone(),
                    aborted_at_step: record.current_step,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_failed".to_string()),
                });
            }
            _ => {}
        }

        cx.checkpoint().map_err(|err| {
            let detail = format!(
                "abort_execution cancelled between get_workflow and upsert_workflow (exec_id={execution_id}): {err}"
            );
            workflow_runner_cancelled("workflow.abort_execution", detail)
        })?;

        let previous_status = record.status.clone();
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let aborted_at_step = record.current_step;
        let now = now_ms();

        let mut updated_record = record;
        updated_record.status = "aborted".to_string();
        updated_record.error = reason.map(|r| format!("Aborted: {r}"));
        updated_record.updated_at = now;
        updated_record.completed_at = Some(now);

        // ft-rlbvg: take a release guard for the abort sequence so the lock
        // drops by Drop on every return path, including panic unwind.
        let _release_guard = self
            .lock_manager
            .held_lock_release_guard(pane_id, execution_id);

        self.storage
            .upsert_workflow_with_cx(cx, updated_record)
            .await?;

        // The workflow abort is durable after the upsert returns. Caller
        // cancellation at that boundary must not suppress trigger settlement
        // and leave the event replayable, so use an independent bounded Cx.
        if let Err(error) = self
            .settle_aborted_trigger_with_fresh_cx(execution_id)
            .await
        {
            return Err(crate::Error::Workflow(
                crate::error::WorkflowError::Aborted(format!(
                    "workflow abort committed for {execution_id}, but trigger settlement failed: \
                     {error}"
                )),
            ));
        }

        tracing::info!(
            execution_id,
            workflow_name,
            pane_id,
            reason = reason.unwrap_or("no reason provided"),
            "Workflow aborted (cx-first)"
        );

        Ok(AbortResult {
            aborted: true,
            execution_id: execution_id.to_string(),
            workflow_name,
            pane_id,
            previous_status,
            aborted_at_step,
            reason: reason.map(std::string::ToString::to_string),
            aborted_at: Some(now as u64),
            error_reason: None,
        })
    }
}

/// Result of an abort operation
#[derive(Debug, Clone, serde::Serialize)]
pub struct AbortResult {
    /// Whether the abort was successful
    pub aborted: bool,
    /// Execution ID
    pub execution_id: String,
    /// Workflow name
    pub workflow_name: String,
    /// Pane ID
    pub pane_id: u64,
    /// Status before abort
    pub previous_status: String,
    /// Step index where abort occurred
    pub aborted_at_step: usize,
    /// Reason for abort (if provided)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Timestamp of abort (epoch ms)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aborted_at: Option<u64>,
    /// Error reason if abort failed (e.g., "already_completed")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_async::CompatRuntime;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for async test");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        crate::runtime_async::clear_runtime_handle();
        if let Err(payload) = test_result {
            std::panic::resume_unwind(payload);
        }
    }

    #[test]
    fn workflow_runner_child_drain_truth_gives_settlement_precedence() {
        assert_eq!(
            classify_workflow_runner_child_drain(
                true,
                crate::runtime_async::task::JoinSetSettlement::Settled,
            ),
            WorkflowRunnerChildDrainOutcome::Settled,
        );
        assert_eq!(
            classify_workflow_runner_child_drain(
                true,
                crate::runtime_async::task::JoinSetSettlement::Incomplete {
                    active_tasks: 2,
                    unacknowledged_tasks: 1,
                },
            ),
            WorkflowRunnerChildDrainOutcome::TimedOut {
                active_tasks: 2,
                unacknowledged_tasks: 1,
            },
        );
    }

    #[test]
    fn workflow_runner_child_drain_trusted_polls_registration_failure_to_settlement() {
        run_async_test(async {
            let mut child_tasks = crate::runtime_async::task::JoinSet::new();
            child_tasks.spawn(std::future::pending::<()>());
            child_tasks.force_join_registration_failure_for_test();

            assert_eq!(
                settle_workflow_runner_children(&mut child_tasks).await,
                WorkflowRunnerChildDrainOutcome::Settled,
                "forced caller-waker registration failure must retain and trusted-drain the aborted workflow task",
            );
            assert_eq!(
                child_tasks.settlement(),
                crate::runtime_async::task::JoinSetSettlement::Settled,
            );
        });
    }

    #[test]
    fn workflow_runner_reaps_completed_child_while_event_stream_is_idle() {
        run_async_test(async {
            let event_bus = crate::events::EventBus::new(8);
            let mut subscriber = event_bus.subscribe_detections();
            let mut child_tasks = crate::runtime_async::task::JoinSet::new();
            child_tasks.spawn(async {});
            let cx = crate::cx::for_testing();

            let wake = crate::runtime_async::timeout_with_cx(
                &cx,
                Duration::from_secs(1),
                wait_for_workflow_runner_activity(
                    &cx,
                    &mut subscriber,
                    &mut child_tasks,
                    None,
                ),
            )
            .await
            .expect("child completion must wake an otherwise-idle runner");
            assert!(matches!(wake, WorkflowRunnerWake::Child(Ok(Some(Ok(()))))));
            assert_eq!(
                child_tasks.settlement(),
                crate::runtime_async::task::JoinSetSettlement::Settled,
            );
        });
    }

    // ========================================================================
    // WorkflowStartResult predicates
    // ========================================================================

    #[test]
    fn start_result_started_is_started() {
        let r = WorkflowStartResult::Started {
            execution_id: "exec-1".to_string(),
            workflow_name: "wf".to_string(),
        };
        assert!(r.is_started());
        assert!(!r.is_locked());
        assert_eq!(r.execution_id(), Some("exec-1"));
    }

    #[test]
    fn start_result_no_matching_workflow() {
        let r = WorkflowStartResult::NoMatchingWorkflow {
            rule_id: "rule-1".to_string(),
        };
        assert!(!r.is_started());
        assert!(!r.is_locked());
        assert!(r.execution_id().is_none());
    }

    #[test]
    fn start_result_pane_locked() {
        let r = WorkflowStartResult::PaneLocked {
            pane_id: 42,
            held_by_workflow: "wf-other".to_string(),
            held_by_execution: "exec-other".to_string(),
        };
        assert!(!r.is_started());
        assert!(r.is_locked());
        assert!(r.execution_id().is_none());
    }

    #[test]
    fn start_result_error() {
        let r = WorkflowStartResult::Error {
            error: "boom".to_string(),
        };
        assert!(!r.is_started());
        assert!(!r.is_locked());
        assert!(r.execution_id().is_none());
    }

    #[test]
    fn start_result_concurrency_limit_reached() {
        let r = WorkflowStartResult::ConcurrencyLimitReached {
            active: 3,
            limit: 3,
        };
        assert!(!r.is_started());
        assert!(!r.is_locked());
        assert!(r.execution_id().is_none());
    }

    // ========================================================================
    // WorkflowStartResult serde roundtrip
    // ========================================================================

    #[test]
    fn start_result_serde_started() {
        let r = WorkflowStartResult::Started {
            execution_id: "e1".to_string(),
            workflow_name: "w1".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        assert!(back.is_started());
        assert_eq!(back.execution_id(), Some("e1"));
    }

    #[test]
    fn start_result_serde_no_matching() {
        let r = WorkflowStartResult::NoMatchingWorkflow {
            rule_id: "r1".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        assert!(!back.is_started());
    }

    #[test]
    fn start_result_serde_pane_locked() {
        let r = WorkflowStartResult::PaneLocked {
            pane_id: 10,
            held_by_workflow: "hw".to_string(),
            held_by_execution: "he".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        assert!(back.is_locked());
    }

    #[test]
    fn start_result_serde_error() {
        let r = WorkflowStartResult::Error {
            error: "err".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        assert!(!back.is_started());
    }

    #[test]
    fn start_result_serde_concurrency_limit_reached() {
        let r = WorkflowStartResult::ConcurrencyLimitReached {
            active: 2,
            limit: 3,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowStartResult = serde_json::from_str(&json).unwrap();
        assert!(!back.is_started());
        assert!(!back.is_locked());
        assert!(back.execution_id().is_none());
    }

    // ========================================================================
    // WorkflowExecutionResult predicates
    // ========================================================================

    #[test]
    fn exec_result_completed() {
        let r = WorkflowExecutionResult::Completed {
            execution_id: "e1".to_string(),
            result: serde_json::json!({"ok": true}),
            elapsed_ms: 500,
            steps_executed: 3,
        };
        assert!(r.is_completed());
        assert!(!r.is_aborted());
        assert_eq!(r.execution_id(), Some("e1"));
    }

    #[test]
    fn exec_result_aborted() {
        let r = WorkflowExecutionResult::Aborted {
            execution_id: "e2".to_string(),
            reason: "timeout".to_string(),
            step_index: 1,
            elapsed_ms: 30_000,
        };
        assert!(!r.is_completed());
        assert!(r.is_aborted());
        assert_eq!(r.execution_id(), Some("e2"));
    }

    #[test]
    fn exec_result_policy_denied() {
        let r = WorkflowExecutionResult::PolicyDenied {
            execution_id: "e3".to_string(),
            step_index: 0,
            reason: "alt_screen".to_string(),
        };
        assert!(!r.is_completed());
        assert!(!r.is_aborted());
        assert_eq!(r.execution_id(), Some("e3"));
    }

    #[test]
    fn exec_result_error_with_id() {
        let r = WorkflowExecutionResult::Error {
            execution_id: Some("e4".to_string()),
            error: "oops".to_string(),
        };
        assert!(!r.is_completed());
        assert!(!r.is_aborted());
        assert_eq!(r.execution_id(), Some("e4"));
    }

    #[test]
    fn exec_result_error_without_id() {
        let r = WorkflowExecutionResult::Error {
            execution_id: None,
            error: "oops".to_string(),
        };
        assert!(r.execution_id().is_none());
    }

    // ========================================================================
    // WorkflowExecutionResult serde roundtrip
    // ========================================================================

    #[test]
    fn exec_result_serde_completed() {
        let r = WorkflowExecutionResult::Completed {
            execution_id: "e1".to_string(),
            result: serde_json::json!(42),
            elapsed_ms: 100,
            steps_executed: 2,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        assert!(back.is_completed());
    }

    #[test]
    fn exec_result_serde_aborted() {
        let r = WorkflowExecutionResult::Aborted {
            execution_id: "e2".to_string(),
            reason: "err".to_string(),
            step_index: 1,
            elapsed_ms: 500,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        assert!(back.is_aborted());
    }

    // ========================================================================
    // WorkflowRunnerConfig defaults
    // ========================================================================

    #[test]
    fn runner_config_defaults() {
        let config = WorkflowRunnerConfig::default();
        assert_eq!(config.max_concurrent, 3);
        assert_eq!(config.step_timeout_ms, 30_000);
        assert!((config.retry_backoff_multiplier - 2.0).abs() < f64::EPSILON);
        assert_eq!(config.max_retries_per_step, 3);
        // ft-3p7re: overall workflow deadline default is 600s (10 min).
        // A pathologically retrying workflow can no longer pin a pane
        // forever. `0` disables.
        assert_eq!(config.workflow_total_deadline_ms, 600_000);
    }

    #[test]
    fn retry_backoff_uses_step_delay_as_first_attempt_base() {
        assert_eq!(
            retry_backoff_delay(100, 1, 2.0),
            Duration::from_millis(100)
        );
        assert_eq!(
            retry_backoff_delay(100, 2, 2.0),
            Duration::from_millis(200)
        );
        assert_eq!(
            retry_backoff_delay(100, 3, 2.0),
            Duration::from_millis(400)
        );
        assert_eq!(
            retry_backoff_delay(100, 4, 1.5),
            Duration::from_millis(338),
            "fractional backoff rounds up so configured delay is never shortened"
        );
    }

    #[test]
    fn retry_backoff_invalid_multiplier_uses_safe_constant_delay() {
        for multiplier in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 0.5] {
            assert_eq!(normalized_retry_backoff_multiplier(multiplier), 1.0);
            assert_eq!(
                retry_backoff_delay(250, 10, multiplier),
                Duration::from_millis(250)
            );
        }
    }

    #[test]
    fn retry_backoff_saturates_instead_of_wrapping() {
        assert_eq!(
            retry_backoff_delay(u64::MAX, 2, 2.0),
            Duration::from_millis(u64::MAX)
        );
        assert_eq!(retry_backoff_delay(0, usize::MAX, f64::MAX), Duration::ZERO);
    }

    #[test]
    fn retry_admission_never_overflows_at_usize_max() {
        assert_eq!(admit_retry_ordinal(0, 0), None);
        assert_eq!(admit_retry_ordinal(0, 1), Some(1));
        assert_eq!(admit_retry_ordinal(1, 1), None);
        assert_eq!(
            admit_retry_ordinal(usize::MAX - 1, usize::MAX),
            Some(usize::MAX)
        );
        assert_eq!(admit_retry_ordinal(usize::MAX, usize::MAX), None);
    }

    #[test]
    fn retry_backoff_never_rounds_large_integer_base_down() {
        for base_delay_ms in [
            (1_u64 << 53) + 1,
            (1_u64 << 53) + 3,
            (1_u64 << 54) + 1,
            u64::MAX - 2_048,
        ] {
            let effective = retry_backoff_delay(base_delay_ms, 2, 1.0);
            assert!(
                effective >= Duration::from_millis(base_delay_ms),
                "large integer base {base_delay_ms}ms rounded down to {effective:?}"
            );
        }
    }

    // ========================================================================
    // AbortResult serialization
    // ========================================================================

    #[test]
    fn abort_result_serializes() {
        let r = AbortResult {
            aborted: true,
            execution_id: "e1".to_string(),
            workflow_name: "wf1".to_string(),
            pane_id: 42,
            previous_status: "running".to_string(),
            aborted_at_step: 2,
            reason: Some("user requested".to_string()),
            aborted_at: Some(1234567890),
            error_reason: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["aborted"], true);
        assert_eq!(json["execution_id"], "e1");
        assert_eq!(json["pane_id"], 42);
        assert_eq!(json["reason"], "user requested");
        // error_reason should be skipped (None)
        assert!(json.get("error_reason").is_none());
    }

    #[test]
    fn abort_result_skips_none_fields() {
        let r = AbortResult {
            aborted: false,
            execution_id: "e2".to_string(),
            workflow_name: "wf2".to_string(),
            pane_id: 1,
            previous_status: "waiting".to_string(),
            aborted_at_step: 0,
            reason: None,
            aborted_at: None,
            error_reason: Some("already_completed".to_string()),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert!(json.get("reason").is_none());
        assert!(json.get("aborted_at").is_none());
        assert_eq!(json["error_reason"], "already_completed");
    }

    // ========================================================================
    // Serde roundtrip for PolicyDenied and Error variants
    // ========================================================================

    #[test]
    fn exec_result_serde_policy_denied() {
        let r = WorkflowExecutionResult::PolicyDenied {
            execution_id: "e-denied".to_string(),
            step_index: 0,
            reason: "pane in alt-screen".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        assert!(!back.is_completed());
        assert!(!back.is_aborted());
        assert_eq!(back.execution_id(), Some("e-denied"));
    }

    #[test]
    fn exec_result_serde_error_with_id() {
        let r = WorkflowExecutionResult::Error {
            execution_id: Some("e-err".to_string()),
            error: "storage unavailable".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        assert!(!back.is_completed());
        assert_eq!(back.execution_id(), Some("e-err"));
    }

    #[test]
    fn exec_result_serde_error_without_id() {
        let r = WorkflowExecutionResult::Error {
            execution_id: None,
            error: "early failure".to_string(),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: WorkflowExecutionResult = serde_json::from_str(&json).unwrap();
        assert!(back.execution_id().is_none());
    }

    // ========================================================================
    // AbortResult roundtrip
    // ========================================================================

    #[test]
    fn abort_result_serializes_all_fields() {
        let r = AbortResult {
            aborted: true,
            execution_id: "e-abort".to_string(),
            workflow_name: "handle_usage_limits".to_string(),
            pane_id: 99,
            previous_status: "running".to_string(),
            aborted_at_step: 3,
            reason: Some("operator initiated".to_string()),
            aborted_at: Some(1710403200000),
            error_reason: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["aborted"], true);
        assert_eq!(json["execution_id"], "e-abort");
        assert_eq!(json["workflow_name"], "handle_usage_limits");
        assert_eq!(json["pane_id"], 99);
        assert_eq!(json["previous_status"], "running");
        assert_eq!(json["aborted_at_step"], 3);
        assert_eq!(json["reason"], "operator initiated");
        assert_eq!(json["aborted_at"], 1710403200000_i64);
        assert!(json.get("error_reason").is_none());
    }

    // ========================================================================
    // WorkflowRunnerConfig custom values
    // ========================================================================

    #[test]
    fn runner_config_custom_values() {
        let config = WorkflowRunnerConfig {
            max_concurrent: 10,
            step_timeout_ms: 60_000,
            retry_backoff_multiplier: 1.5,
            max_retries_per_step: 5,
            workflow_total_deadline_ms: 1_800_000, // 30 min
        };
        assert_eq!(config.max_concurrent, 10);
        assert_eq!(config.step_timeout_ms, 60_000);
        assert!((config.retry_backoff_multiplier - 1.5).abs() < f64::EPSILON);
        assert_eq!(config.max_retries_per_step, 5);
        assert_eq!(config.workflow_total_deadline_ms, 1_800_000);
    }

    #[test]
    fn runner_new_normalizes_invalid_retry_multiplier() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("runner_invalid_retry_multiplier.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
            let handle: crate::wezterm::WeztermHandle =
                Arc::new(crate::wezterm::MockWezterm::new());
            let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ));
            let runner = WorkflowRunner::new(
                WorkflowEngine::default(),
                Arc::new(PaneWorkflowLockManager::new()),
                Arc::clone(&storage),
                injector,
                WorkflowRunnerConfig {
                    retry_backoff_multiplier: f64::NAN,
                    ..WorkflowRunnerConfig::default()
                },
            );

            assert_eq!(runner.config.retry_backoff_multiplier, 1.0);
            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn runner_config_disabled_overall_deadline_is_zero() {
        // ft-3p7re: explicit zero disables the overall-workflow deadline
        // (legacy behavior). The runner's deadline check is gated on
        // `> 0`. This test pins the contract so a future change of the
        // sentinel value cannot silently break opt-out callers.
        let config = WorkflowRunnerConfig {
            workflow_total_deadline_ms: 0,
            ..WorkflowRunnerConfig::default()
        };
        assert_eq!(config.workflow_total_deadline_ms, 0);
    }

    struct CompletionPersistenceProbeWorkflow;

    impl Workflow for CompletionPersistenceProbeWorkflow {
        fn name(&self) -> &'static str {
            "completion_persistence_probe"
        }

        fn description(&self) -> &'static str {
            "Completes immediately to exercise terminal persistence"
        }

        fn handles(&self, _detection: &crate::patterns::Detection) -> bool {
            true
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("finish", "Finish")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done(serde_json::json!({ "ok": true })),
                    _ => StepResult::abort("unexpected step"),
                }
            })
        }
    }

    #[test]
    fn workflow_completion_persistence_failure_is_not_reported_completed() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("completion_persistence_fail_closed.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
            let handle: crate::wezterm::WeztermHandle =
                Arc::new(crate::wezterm::MockWezterm::new());
            let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ));
            let runner = WorkflowRunner::new(
                WorkflowEngine::default(),
                Arc::new(PaneWorkflowLockManager::new()),
                Arc::clone(&storage),
                injector,
                WorkflowRunnerConfig::default(),
            );
            let execution_id = "missing-completion-record";
            let result = runner
                .run_workflow(
                    77,
                    Arc::new(CompletionPersistenceProbeWorkflow),
                    execution_id,
                    0,
                )
                .await;

            match result {
                WorkflowExecutionResult::Error {
                    execution_id: Some(id),
                    error,
                } => {
                    assert_eq!(id, execution_id);
                    assert!(
                        error.contains(execution_id),
                        "completion persistence error should identify the execution: {error}"
                    );
                }
                other => panic!("missing terminal persistence must not report success: {other:?}"),
            }

            storage.shutdown().await.unwrap();
        });
    }

    // ── ft-7h5da.8.3 (W7.3): requires_unlimited_pane scheduling gate ──

    struct UnlimitedGateProbeWorkflow {
        requires_unlimited: bool,
    }

    impl Workflow for UnlimitedGateProbeWorkflow {
        fn name(&self) -> &'static str {
            "unlimited_gate_probe"
        }

        fn description(&self) -> &'static str {
            "Probes the requires_unlimited_pane scheduling gate (ft-7h5da.8.3)"
        }

        fn handles(&self, _detection: &crate::patterns::Detection) -> bool {
            true
        }

        fn requires_unlimited_pane(&self) -> bool {
            self.requires_unlimited
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("finish", "Finish")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move {
                match step_idx {
                    0 => StepResult::done(serde_json::json!({ "ok": true })),
                    _ => StepResult::abort("unexpected step"),
                }
            })
        }
    }

    fn unlimited_gate_detection() -> crate::patterns::Detection {
        crate::patterns::Detection {
            rule_id: "unlimited_gate.trigger".to_string(),
            agent_type: crate::patterns::AgentType::Codex,
            event_type: "test".to_string(),
            severity: crate::patterns::Severity::Info,
            confidence: 1.0,
            extracted: serde_json::Value::Null,
            matched_text: "trigger".to_string(),
            span: (0, 0),
        }
    }

    async fn unlimited_gate_runner(
        db_path: &str,
    ) -> (
        Arc<crate::storage::StorageHandle>,
        WorkflowRunner,
        Arc<PaneWorkflowLockManager>,
    ) {
        let storage = Arc::new(crate::storage::StorageHandle::new(db_path).await.unwrap());
        let lock_manager = Arc::new(PaneWorkflowLockManager::new());
        let handle: crate::wezterm::WeztermHandle = Arc::new(crate::wezterm::MockWezterm::new());
        let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
            crate::policy::PolicyEngine::permissive(),
            handle,
        ));
        let runner = WorkflowRunner::new(
            WorkflowEngine::default(),
            Arc::clone(&lock_manager),
            Arc::clone(&storage),
            injector,
            WorkflowRunnerConfig::default(),
        );
        (storage, runner, lock_manager)
    }

    #[test]
    fn workflow_runner_shutdown_notify_wakes_idle_loop_and_settles() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().expect("tempdir");
            let db_path = temp_dir
                .path()
                .join("runner_cooperative_shutdown.db")
                .to_string_lossy()
                .to_string();
            let (storage, runner, _lock_manager) = unlimited_gate_runner(&db_path).await;
            let event_bus = Arc::new(crate::events::EventBus::new(8));
            let task_event_bus = Arc::clone(&event_bus);
            let shutdown_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let task_shutdown_flag = Arc::clone(&shutdown_flag);
            let shutdown_notify = Arc::new(crate::runtime_async::notify::Notify::new());
            let task_shutdown_notify = Arc::clone(&shutdown_notify);
            let runner_cx = crate::cx::for_testing();

            let runner_task = crate::runtime_async::task::spawn(async move {
                runner
                    .run_with_shutdown_with_cx(
                        &runner_cx,
                        &task_event_bus,
                        &task_shutdown_flag,
                        &task_shutdown_notify,
                    )
                    .await;
            });

            for _ in 0..4_096 {
                if event_bus.stats().detection_subscribers > 0 {
                    break;
                }
                crate::runtime_async::yield_now().await;
            }
            assert!(
                event_bus.stats().detection_subscribers > 0,
                "runner must reach its idle detection wait before shutdown"
            );

            shutdown_flag.store(true, Ordering::SeqCst);
            shutdown_notify.notify_waiters();
            let join_cx = crate::cx::for_testing();
            crate::runtime_async::timeout_with_cx(
                &join_cx,
                Duration::from_secs(1),
                runner_task,
            )
            .await
            .expect("cooperative shutdown must remain bounded")
            .expect("workflow runner task must settle cleanly");

            storage.shutdown().await.expect("shutdown workflow storage");
        });
    }

    async fn seed_limited_pane(
        storage: &crate::storage::StorageHandle,
        pane_id: u64,
        reset_at: Option<i64>,
        reset_source: &str,
        last_seen_at: i64,
        conservative_ttl_ms: i64,
    ) {
        storage
            .upsert_pane(crate::storage::PaneRecord {
                pane_id,
                pane_uuid: Some(format!("pane-{pane_id}")),
                domain: "local".to_string(),
                window_id: None,
                tab_id: None,
                title: Some("codex".to_string()),
                cwd: None,
                tty_name: None,
                first_seen_at: last_seen_at,
                last_seen_at,
                observed: true,
                ignore_reason: None,
                last_decision_at: None,
            })
            .await
            .unwrap();
        storage
            .upsert_limit_window(crate::storage::LimitWindowRecord {
                id: 0,
                pane_id,
                service: "openai".to_string(),
                account_id: "acct-a".to_string(),
                account_db_id: None,
                account_known: false,
                agent_type: Some("codex".to_string()),
                rule_id: "codex.usage.reached".to_string(),
                event_type: "usage.reached".to_string(),
                limited_at: last_seen_at,
                reset_at,
                reset_source: reset_source.to_string(),
                reset_text: None,
                conservative_ttl_ms,
                last_seen_at,
                seen_count: 1,
                metadata: None,
                created_at: last_seen_at,
                updated_at: last_seen_at,
            })
            .await
            .unwrap();
    }

    fn pre_hold_pane_lock(
        lock_manager: &Arc<PaneWorkflowLockManager>,
        pane_id: u64,
    ) -> crate::workflows::lock::OwnedPaneWorkflowLockGuard {
        let Ok(crate::workflows::lock::OwnedLockAcquisitionResult::Acquired(guard)) =
            lock_manager.try_acquire_with_limit_owned_full(pane_id, "holder", "holder-exec", 8)
        else {
            panic!("failed to pre-acquire pane {pane_id} lock");
        };
        guard
    }

    #[test]
    fn requires_unlimited_pane_declines_with_absolute_reset() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("unlimited_gate_absolute.db")
                .to_string_lossy()
                .to_string();
            let (storage, runner, _locks) = unlimited_gate_runner(&db_path).await;
            let reset_at = crate::storage::now_ms() + 3_600_000;
            seed_limited_pane(
                &storage,
                77,
                Some(reset_at),
                "absolute",
                crate::storage::now_ms(),
                0,
            )
            .await;
            runner.register_workflow(Arc::new(UnlimitedGateProbeWorkflow {
                requires_unlimited: true,
            }));

            let result = runner
                .handle_detection(77, &unlimited_gate_detection(), None)
                .await;

            match result {
                WorkflowStartResult::PaneRateLimited {
                    pane_id,
                    reset_at_ms,
                    reset_known,
                    ..
                } => {
                    assert_eq!(pane_id, 77);
                    assert_eq!(reset_at_ms, reset_at);
                    assert!(reset_known, "absolute reset must be reported as known");
                }
                other => panic!("expected PaneRateLimited, got {other:?}"),
            }

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn requires_unlimited_pane_declines_with_unknown_ttl_reset() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("unlimited_gate_unknown.db")
                .to_string_lossy()
                .to_string();
            let (storage, runner, _locks) = unlimited_gate_runner(&db_path).await;
            let last_seen = crate::storage::now_ms();
            seed_limited_pane(&storage, 88, None, "unknown_ttl", last_seen, 300_000).await;
            runner.register_workflow(Arc::new(UnlimitedGateProbeWorkflow {
                requires_unlimited: true,
            }));

            let result = runner
                .handle_detection(88, &unlimited_gate_detection(), None)
                .await;

            match result {
                WorkflowStartResult::PaneRateLimited {
                    reset_at_ms,
                    reset_known,
                    ..
                } => {
                    assert_eq!(reset_at_ms, last_seen + 300_000);
                    assert!(
                        !reset_known,
                        "unknown_ttl reset must be reported as estimated"
                    );
                }
                other => panic!("expected PaneRateLimited, got {other:?}"),
            }

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn requires_unlimited_pane_allows_when_not_limited() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("unlimited_gate_clear.db")
                .to_string_lossy()
                .to_string();
            let (storage, runner, lock_manager) = unlimited_gate_runner(&db_path).await;
            // Pane exists with only an EXPIRED window — no active limit.
            seed_limited_pane(
                &storage,
                99,
                Some(crate::storage::now_ms() - 1_000),
                "absolute",
                crate::storage::now_ms() - 1_000,
                0,
            )
            .await;
            runner.register_workflow(Arc::new(UnlimitedGateProbeWorkflow {
                requires_unlimited: true,
            }));
            // Pre-hold the pane lock so the post-gate path returns PaneLocked
            // before any engine/spawn work — a deterministic "gate passed" signal.
            let _guard = pre_hold_pane_lock(&lock_manager, 99);

            let result = runner
                .handle_detection(99, &unlimited_gate_detection(), None)
                .await;

            assert!(
                !result.is_pane_rate_limited(),
                "pane with no active limit window must not be declined: {result:?}"
            );
            assert!(
                result.is_locked(),
                "gate should pass through to lock acquisition: {result:?}"
            );

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn default_workflow_runs_on_limited_pane() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("unlimited_gate_default.db")
                .to_string_lossy()
                .to_string();
            let (storage, runner, lock_manager) = unlimited_gate_runner(&db_path).await;
            // Pane IS limited, but the workflow does not require an unlimited pane.
            seed_limited_pane(
                &storage,
                55,
                Some(crate::storage::now_ms() + 3_600_000),
                "absolute",
                crate::storage::now_ms(),
                0,
            )
            .await;
            runner.register_workflow(Arc::new(UnlimitedGateProbeWorkflow {
                requires_unlimited: false,
            }));
            let _guard = pre_hold_pane_lock(&lock_manager, 55);

            let result = runner
                .handle_detection(55, &unlimited_gate_detection(), None)
                .await;

            assert!(
                !result.is_pane_rate_limited(),
                "default workflow must be unaffected by limit state: {result:?}"
            );

            storage.shutdown().await.unwrap();
        });
    }

    struct ProgressPersistenceProbeWorkflow {
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Workflow for ProgressPersistenceProbeWorkflow {
        fn name(&self) -> &'static str {
            "progress_persistence_probe"
        }

        fn description(&self) -> &'static str {
            "Continues once before completing to exercise progress persistence"
        }

        fn handles(&self, _detection: &crate::patterns::Detection) -> bool {
            true
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![
                WorkflowStep::new("advance", "Advance"),
                WorkflowStep::new("finish", "Finish"),
            ]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match step_idx {
                    0 => StepResult::Continue,
                    1 => StepResult::done(serde_json::json!({ "ok": true })),
                    _ => StepResult::abort("unexpected step"),
                }
            })
        }
    }

    #[test]
    fn workflow_progress_persistence_failure_stops_before_later_steps() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("progress_persistence_fail_closed.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
            let handle: crate::wezterm::WeztermHandle =
                Arc::new(crate::wezterm::MockWezterm::new());
            let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ));
            let runner = WorkflowRunner::new(
                WorkflowEngine::default(),
                Arc::new(PaneWorkflowLockManager::new()),
                Arc::clone(&storage),
                injector,
                WorkflowRunnerConfig::default(),
            );
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let execution_id = "missing-progress-record";
            let result = runner
                .run_workflow(
                    77,
                    Arc::new(ProgressPersistenceProbeWorkflow {
                        calls: Arc::clone(&calls),
                    }),
                    execution_id,
                    0,
                )
                .await;

            match result {
                WorkflowExecutionResult::Error {
                    execution_id: Some(id),
                    error,
                } => {
                    assert_eq!(id, execution_id);
                    assert!(
                        error.contains(execution_id),
                        "progress persistence error should identify the execution: {error}"
                    );
                }
                other => panic!("missing progress persistence must not keep running: {other:?}"),
            }
            assert_eq!(
                calls.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "runner must stop before executing later steps after progress persistence fails"
            );

            storage.shutdown().await.unwrap();
        });
    }

    struct InvalidJumpPersistenceProbeWorkflow;

    impl Workflow for InvalidJumpPersistenceProbeWorkflow {
        fn name(&self) -> &'static str {
            "invalid_jump_persistence_probe"
        }

        fn description(&self) -> &'static str {
            "Attempts an invalid jump to exercise abort persistence"
        }

        fn handles(&self, _detection: &crate::patterns::Detection) -> bool {
            true
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("bad_jump", "Bad jump")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            _step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move { StepResult::JumpTo { step: 99 } })
        }
    }

    #[test]
    fn workflow_invalid_jump_abort_persistence_failure_is_not_reported_aborted() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("invalid_jump_persistence_fail_closed.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
            let handle: crate::wezterm::WeztermHandle =
                Arc::new(crate::wezterm::MockWezterm::new());
            let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ));
            let runner = WorkflowRunner::new(
                WorkflowEngine::default(),
                Arc::new(PaneWorkflowLockManager::new()),
                Arc::clone(&storage),
                injector,
                WorkflowRunnerConfig::default(),
            );
            let execution_id = "missing-invalid-jump-record";
            let result = runner
                .run_workflow(
                    77,
                    Arc::new(InvalidJumpPersistenceProbeWorkflow),
                    execution_id,
                    0,
                )
                .await;

            match result {
                WorkflowExecutionResult::Error {
                    execution_id: Some(id),
                    error,
                } => {
                    assert_eq!(id, execution_id);
                    assert!(
                        error.contains(execution_id),
                        "abort persistence error should identify the execution: {error}"
                    );
                }
                other => panic!("missing abort persistence must not report aborted: {other:?}"),
            }

            storage.shutdown().await.unwrap();
        });
    }

    struct JumpCyclePersistenceProbeWorkflow;

    impl Workflow for JumpCyclePersistenceProbeWorkflow {
        fn name(&self) -> &'static str {
            "jump_cycle_persistence_probe"
        }

        fn description(&self) -> &'static str {
            "Loops until the runner's jump-cycle guard aborts"
        }

        fn handles(&self, _detection: &crate::patterns::Detection) -> bool {
            true
        }

        fn steps(&self) -> Vec<WorkflowStep> {
            vec![WorkflowStep::new("loop", "Loop")]
        }

        fn execute_step(
            &self,
            _ctx: &mut WorkflowContext,
            _step_idx: usize,
        ) -> BoxFuture<'_, StepResult> {
            Box::pin(async move { StepResult::JumpTo { step: 0 } })
        }
    }

    #[test]
    fn workflow_jump_cycle_guard_persists_terminal_state() {
        run_async_test(async {
            let temp_dir = tempfile::TempDir::new().unwrap();
            let db_path = temp_dir
                .path()
                .join("jump_cycle_guard_terminal_state.db")
                .to_string_lossy()
                .to_string();
            let storage = Arc::new(crate::storage::StorageHandle::new(&db_path).await.unwrap());
            let handle: crate::wezterm::WeztermHandle =
                Arc::new(crate::wezterm::MockWezterm::new());
            let injector = CxPolicyInjector::new(crate::policy::PolicyGatedInjector::new(
                crate::policy::PolicyEngine::permissive(),
                handle,
            ));
            let runner = WorkflowRunner::new(
                WorkflowEngine::default(),
                Arc::new(PaneWorkflowLockManager::new()),
                Arc::clone(&storage),
                injector,
                WorkflowRunnerConfig::default(),
            );
            let execution_id = "jump-cycle-terminal-state";
            let cx = crate::cx::for_testing();
            let outcome = runner
                .run_workflow_manual_with_cx(
                    &cx,
                    77,
                    Arc::new(JumpCyclePersistenceProbeWorkflow),
                    execution_id,
                    None,
                )
                .await;

            match outcome {
                ManualWorkflowRunOutcome::Ran(WorkflowExecutionResult::Aborted {
                    reason,
                    ..
                }) => assert!(reason.contains("exceeded maximum jump count")),
                other => panic!("jump cycle should terminate as a durable abort: {other:?}"),
            }

            let record = storage
                .get_workflow_with_cx(&cx, execution_id)
                .await
                .expect("terminal workflow lookup should succeed")
                .expect("manual workflow record should exist");
            assert!(
                matches!(record.status.as_str(), "failed" | "aborted"),
                "jump-cycle guard must not leave a resumable record: {record:?}"
            );

            storage.shutdown().await.unwrap();
        });
    }

    #[test]
    fn jump_target_inside_step_range_is_valid() {
        assert!(invalid_jump_target_reason(0, 1).is_none());
        assert!(invalid_jump_target_reason(2, 3).is_none());
    }

    #[test]
    fn jump_target_at_or_beyond_step_count_is_invalid() {
        let at_end = invalid_jump_target_reason(3, 3).expect("step_count is not executable");
        assert!(at_end.contains("jump target 3"));
        assert!(at_end.contains("0..3"));

        let beyond_end = invalid_jump_target_reason(99, 3).expect("beyond step_count is invalid");
        assert!(beyond_end.contains("jump target 99"));
        assert!(beyond_end.contains("0..3"));
    }

    #[test]
    fn resume_step_allows_exact_completion_boundary() {
        assert!(invalid_resume_step_reason(0, 0).is_none());
        assert!(invalid_resume_step_reason(3, 3).is_none());
    }

    #[test]
    fn resume_step_beyond_completion_boundary_is_invalid() {
        let reason = invalid_resume_step_reason(4, 3).expect("resume beyond step_count is invalid");
        assert!(reason.contains("resume step 4"));
        assert!(reason.contains("0..=3"));
    }

    // -------------------------------------------------------------------------
    // LabRuntime deterministic tests for workflows/runner.rs primitives
    // (ft-xbnl0.2.2 slice).
    //
    // Full `WorkflowRunner` construction requires storage, engine,
    // lock_manager, and injector — an integration-scale setup unsuited
    // to LabRuntime's deterministic scheduler. These LabRuntime tests
    // therefore pin the pure-data surface that backs
    // `handle_detection_with_cx` and `abort_execution_with_cx` pre-cancel
    // contracts: result predicates, serde shapes, config defaults.
    // -------------------------------------------------------------------------

    mod labruntime_runner {
        use super::*;

        fn run_lab<F>(seed: u64, f: impl FnOnce() -> F + Send + 'static)
        where
            F: std::future::Future<Output = ()> + Send + 'static,
        {
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(seed)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    f().await;
                })
                .expect("spawn lab task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();
            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime got stuck; termination: {:?}",
                report.termination,
            );
        }

        /// 1. WorkflowRunnerConfig defaults are stable under LabRuntime.
        #[test]
        fn workflow_runner_config_defaults_under_labruntime() {
            run_lab(5001, || async move {
                let config = WorkflowRunnerConfig::default();
                assert_eq!(config.max_concurrent, 3);
                assert_eq!(config.step_timeout_ms, 30_000);
                assert!((config.retry_backoff_multiplier - 2.0).abs() < f64::EPSILON);
                assert_eq!(config.max_retries_per_step, 3);
                // ft-3p7re: overall-deadline default is stable under
                // LabRuntime (deterministic, non-time-sensitive).
                assert_eq!(config.workflow_total_deadline_ms, 600_000);
            });
        }

        #[test]
        fn cx_wait_helpers_use_labruntime_virtual_time() {
            let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let completed_task = Arc::clone(&completed);
            let wall_start = Instant::now();
            let mut runtime = asupersync::LabRuntime::new(
                asupersync::LabConfig::new(5007)
                    .with_auto_advance()
                    .worker_count(2)
                    .max_steps(50_000),
            );
            let region = runtime
                .state
                .create_root_region(asupersync::Budget::INFINITE);
            let (task_id, _handle) = runtime
                .state
                .create_task(region, asupersync::Budget::INFINITE, async move {
                    let cx = crate::cx::Cx::current().expect("LabRuntime task should expose Cx");
                    wait_duration_with_cx(&cx, Duration::from_secs(5), "virtual duration")
                        .await
                        .expect("virtual duration wait should complete");

                    let signals = ExternalSignalRegistry::new();
                    let error = wait_external_signal_with_cx(
                        &cx,
                        &signals,
                        "never-fired",
                        Duration::from_secs(5),
                        "virtual external wait",
                    )
                    .await
                    .expect_err("missing signal should time out");
                    assert!(
                        error.to_string().contains("timed out after 5000ms"),
                        "unexpected timeout error: {error}"
                    );
                    completed_task.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .expect("spawn virtual-time workflow wait task");
            runtime.scheduler.lock().schedule(task_id, 0);

            let report = runtime.run_with_auto_advance();

            assert!(
                !matches!(
                    report.termination,
                    asupersync::lab::AutoAdvanceTermination::StuckBailout
                ),
                "LabRuntime workflow waits should not get stuck: {:?}",
                report.termination
            );
            assert!(
                runtime.now() >= asupersync::Time::from_secs(10),
                "virtual time should advance through both five-second waits"
            );
            assert!(
                wall_start.elapsed() < Duration::from_secs(1),
                "virtual waits must not consume ten wall-clock seconds"
            );
            assert!(
                completed.load(std::sync::atomic::Ordering::SeqCst),
                "workflow wait task should complete"
            );
        }

        /// 2. The "capability context already cancelled" Error variant
        ///    produced by `handle_detection_with_cx` serializes stably:
        ///    downstream telemetry that matches on the `type` tag + the
        ///    error-message prefix must remain reliable.
        #[test]
        fn cancelled_error_variant_serde_roundtrip_under_labruntime() {
            run_lab(5002, || async move {
                let err = WorkflowStartResult::Error {
                    error: "capability context already cancelled".to_owned(),
                };
                let json = serde_json::to_string(&err).expect("serialize");
                assert!(
                    json.contains("\"type\":\"error\""),
                    "serde tag missing in {json}"
                );
                assert!(
                    json.contains("capability context already cancelled"),
                    "cancellation message must round-trip verbatim: {json}"
                );
                let restored: WorkflowStartResult =
                    serde_json::from_str(&json).expect("deserialize");
                match restored {
                    WorkflowStartResult::Error { error } => {
                        assert_eq!(error, "capability context already cancelled");
                    }
                    other => panic!("unexpected variant: {other:?}"),
                }
            });
        }

        /// 3. `WorkflowStartResult::Error` is classified correctly by
        ///    the `is_started` / `is_locked` / `execution_id` helpers
        ///    under LabRuntime virtual time.
        #[test]
        fn error_variant_predicates_under_labruntime() {
            run_lab(5003, || async move {
                let r = WorkflowStartResult::Error {
                    error: "capability context already cancelled".to_owned(),
                };
                assert!(!r.is_started());
                assert!(!r.is_locked());
                assert!(r.execution_id().is_none());
            });
        }

        /// 4. `WorkflowStartResult::Started` remains detectable by
        ///    `is_started` and surfaces its execution_id, so the
        ///    Cx-first pre-cancel path is observably distinct from a
        ///    successful start.
        #[test]
        fn started_variant_predicates_under_labruntime() {
            run_lab(5004, || async move {
                let r = WorkflowStartResult::Started {
                    execution_id: "exec-wa-runner".to_owned(),
                    workflow_name: "wf-lab".to_owned(),
                };
                assert!(r.is_started());
                assert!(!r.is_locked());
                assert_eq!(r.execution_id(), Some("exec-wa-runner"));
            });
        }

        /// 5. AbortResult serde shape: the Cx-first
        ///    `abort_execution_with_cx` relies on the existing
        ///    AbortResult surface staying stable.
        #[test]
        fn abort_result_serde_shape_under_labruntime() {
            run_lab(5005, || async move {
                let result = AbortResult {
                    aborted: true,
                    execution_id: "exec-abort-1".to_owned(),
                    workflow_name: "wf-abort".to_owned(),
                    pane_id: 42,
                    previous_status: "running".to_owned(),
                    aborted_at_step: 3,
                    reason: Some("operator".to_owned()),
                    aborted_at: Some(1_700_000_000_000),
                    error_reason: None,
                };
                let json = serde_json::to_string(&result).expect("serialize");
                assert!(json.contains("\"aborted\":true"));
                assert!(json.contains("\"pane_id\":42"));
                assert!(json.contains("\"aborted_at_step\":3"));
                assert!(
                    !json.contains("error_reason"),
                    "skip_serializing_if must drop None error_reason"
                );
            });
        }

        /// 6. AbortResult with None reason and None aborted_at omits
        ///    those fields (via skip_serializing_if). Pins the
        ///    wire-format contract for already-terminal abort paths.
        #[test]
        fn abort_result_omits_none_fields_under_labruntime() {
            run_lab(5006, || async move {
                let result = AbortResult {
                    aborted: false,
                    execution_id: "exec-done".to_owned(),
                    workflow_name: "wf-done".to_owned(),
                    pane_id: 7,
                    previous_status: "completed".to_owned(),
                    aborted_at_step: 10,
                    reason: None,
                    aborted_at: None,
                    error_reason: Some("already_completed".to_owned()),
                };
                let json = serde_json::to_string(&result).expect("serialize");
                assert!(
                    !json.contains("\"reason\""),
                    "None reason must be skipped: {json}"
                );
                assert!(
                    !json.contains("\"aborted_at\""),
                    "None aborted_at must be skipped: {json}"
                );
                assert!(json.contains("\"error_reason\":\"already_completed\""));
            });
        }

        // ================================================================
        // Wait helper coverage (commit 3503a151)
        // ================================================================

        /// workflow_wait_aborted formats "{label} cancelled: {err}"
        #[test]
        fn wait_aborted_error_format() {
            let err = workflow_wait_aborted("my-label", "some reason");
            match err {
                crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                    assert_eq!(reason, "my-label cancelled: some reason");
                }
                other => panic!("expected Workflow(Aborted), got: {other:?}"),
            }
        }

        /// A Cx-aware duration wait completes normally while the Cx is live.
        #[test]
        fn wait_duration_with_cx_completes() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let result = wait_duration_with_cx(&cx, Duration::from_millis(1), "test-cx").await;
                assert!(result.is_ok());
            });
        }

        /// A Cx-aware zero duration wait returns immediately.
        #[test]
        fn wait_duration_zero_returns_immediately() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let result = wait_duration_with_cx(&cx, Duration::ZERO, "test-zero").await;
                assert!(result.is_ok());
            });
        }

        #[test]
        fn workflow_deadline_cap_leaves_waits_uncapped_when_disabled() {
            let requested = Duration::from_secs(60);
            let started_at = Instant::now()
                .checked_sub(Duration::from_secs(5))
                .expect("test instant subtracts");

            assert_eq!(
                cap_wait_by_workflow_deadline(requested, started_at, 0),
                requested
            );
        }

        #[test]
        fn workflow_deadline_cap_limits_wait_to_remaining_budget() {
            let requested = Duration::from_secs(60);
            let started_at = Instant::now()
                .checked_sub(Duration::from_millis(900))
                .expect("test instant subtracts");

            let capped = cap_wait_by_workflow_deadline(requested, started_at, 1_000);

            assert!(
                capped <= Duration::from_millis(100),
                "wait cap should not exceed remaining workflow deadline budget: {capped:?}"
            );
        }

        #[test]
        fn workflow_deadline_cap_returns_zero_after_deadline() {
            let requested = Duration::from_secs(60);
            let started_at = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("test instant subtracts");

            assert_eq!(
                cap_wait_by_workflow_deadline(requested, started_at, 1_000),
                Duration::ZERO
            );
        }

        #[test]
        fn workflow_deadline_cap_fails_closed_when_deadline_overflows() {
            let requested = Duration::from_secs(60);
            let started_at = Instant::now();

            assert_eq!(
                cap_wait_by_workflow_deadline(requested, started_at, u64::MAX),
                Duration::ZERO,
                "overflowing workflow_total_deadline_ms must not disable the wait cap"
            );
        }

        /// wait_condition_pause_with_cx dispatches Sleep correctly.
        #[test]
        fn wait_condition_sleep_with_cx() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let cond = WaitCondition::sleep(1);
                let result = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(1),
                    None,
                    "test-sleep",
                )
                .await;
                assert!(result.is_ok());
            });
        }

        #[test]
        fn wait_condition_sleep_aborts_when_timeout_expires() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let cond = WaitCondition::sleep(60);
                let start = std::time::Instant::now();
                let err = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_millis(5),
                    None,
                    "test-sleep",
                )
                .await
                .expect_err("sleep longer than timeout must abort");
                let elapsed = start.elapsed();

                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("sleep wait timed out"),
                            "unexpected abort reason: {reason}"
                        );
                    }
                    other => panic!("expected Workflow(Aborted), got: {other:?}"),
                }
                assert!(
                    elapsed < Duration::from_secs(1),
                    "timeout abort should not wait for full sleep duration: {elapsed:?}"
                );
            });
        }

        /// wait_condition_pause_with_cx dispatches PaneIdle correctly.
        #[test]
        fn wait_condition_pane_idle_with_cx() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let cond = WaitCondition::pane_idle(1);
                let result = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(1),
                    None,
                    "test-idle",
                )
                .await;
                assert!(result.is_ok());
            });
        }

        /// wait_condition_pause_with_cx dispatches StableTail correctly.
        #[test]
        fn wait_condition_stable_tail_with_cx() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let cond = WaitCondition::stable_tail(1);
                let result = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(1),
                    None,
                    "test-tail",
                )
                .await;
                assert!(result.is_ok());
            });
        }

        #[test]
        fn wait_condition_duration_variants_abort_when_timeout_expires() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                for (condition, expected_reason) in [
                    (WaitCondition::pane_idle(60), "pane idle wait timed out"),
                    (WaitCondition::stable_tail(60), "stable tail wait timed out"),
                ] {
                    let start = std::time::Instant::now();
                    let err = wait_condition_pause_with_cx(
                        &cx,
                        &condition,
                        Duration::from_millis(5),
                        None,
                        "test-duration",
                    )
                    .await
                    .expect_err("duration wait longer than timeout must abort");
                    let elapsed = start.elapsed();

                    match err {
                        crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                            assert!(
                                reason.contains(expected_reason),
                                "unexpected abort reason: {reason}"
                            );
                        }
                        other => panic!("expected Workflow(Aborted), got: {other:?}"),
                    }
                    assert!(
                        elapsed < Duration::from_secs(1),
                        "timeout abort should not wait for full duration: {elapsed:?}"
                    );
                }
            });
        }

        #[test]
        fn pattern_and_text_match_waits_require_observable_pane_text() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                for (condition, expected_reason) in [
                    (
                        WaitCondition::pattern("prompt.ready"),
                        "pattern wait 'prompt.ready' requires a pane text source",
                    ),
                    (
                        WaitCondition::text_match(TextMatch::substring("done")),
                        "text-match wait substring(len=4",
                    ),
                ] {
                    let err = wait_condition_pause_with_cx(
                        &cx,
                        &condition,
                        Duration::from_secs(60),
                        None,
                        "workflow wait condition",
                    )
                    .await
                    .expect_err("unobservable wait must abort instead of advancing");

                    match err {
                        crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                            assert!(
                                reason.contains(expected_reason),
                                "unexpected abort reason: {reason}"
                            );
                            assert!(
                                reason.contains("WaitConditionExecutor"),
                                "abort reason should point to the observable wait executor: {reason}"
                            );
                        }
                        other => panic!("expected Workflow(Aborted), got: {other:?}"),
                    }
                }
            });
        }

        /// 7. Wait helpers used by `WaitFor` and `SendText(wait_for=...)`
        ///    must surface a cancelled caller context as an aborted
        ///    workflow result instead of falling back to an ambient
        ///    sleep that ignores cancellation entirely.
        #[test]
        fn wait_condition_pause_observes_pre_cancelled_cx() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("runner wait test pre-cancel"),
                );

                let err = wait_condition_pause_with_cx(
                    &cx,
                    &WaitCondition::sleep(250),
                    Duration::from_secs(1),
                    None,
                    "workflow wait condition",
                )
                .await
                .expect_err("pre-cancelled cx should abort wait helper");

                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("workflow wait condition cancelled"),
                            "unexpected abort reason: {reason}"
                        );
                    }
                    other => panic!("unexpected wait error: {other:?}"),
                }
            });
        }

        /// Retry backoff uses the same helper as wait conditions. A cancel
        /// fired during the backoff must abort promptly instead of sleeping
        /// until the requested retry delay elapses.
        #[test]
        fn retry_backoff_wait_observes_mid_flight_cancelled_cx() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let cancel_cx = cx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    cancel_cx.cancel_with(
                        crate::outcome::CancelKind::User,
                        Some("retry backoff cancel regression"),
                    );
                });

                let start = std::time::Instant::now();
                let err =
                    wait_duration_with_cx(&cx, Duration::from_secs(60), "workflow retry backoff")
                        .await
                        .expect_err("mid-flight cx cancel should abort retry backoff");
                let elapsed = start.elapsed();

                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("workflow retry backoff cancelled"),
                            "unexpected abort reason: {reason}"
                        );
                    }
                    other => panic!("unexpected retry backoff error: {other:?}"),
                }
                assert!(
                    elapsed < Duration::from_secs(1),
                    "retry backoff should not sleep until the long delay after cancellation; took {elapsed:?}"
                );
            });
        }

        // ================================================================
        // ft-ao9k9: External wait registry wiring (was: timeout-sleep mock)
        // ================================================================

        /// Without a registry the runner used to silently sleep until the
        /// configured timeout. Now it must abort with a message naming the
        /// signal key and the wiring API.
        #[test]
        fn external_wait_without_registry_returns_explicit_error() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let cond = WaitCondition::external("deploy-finished");
                let err = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(60),
                    None,
                    "workflow wait condition",
                )
                .await
                .expect_err("missing registry must surface as abort");
                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("deploy-finished"),
                            "abort reason must name signal key: {reason}"
                        );
                        assert!(
                            reason.contains("with_external_signals"),
                            "abort reason must point at the wiring API: {reason}"
                        );
                    }
                    other => panic!("expected Workflow(Aborted), got: {other:?}"),
                }
            });
        }

        #[test]
        fn external_wait_rejects_empty_signal_key() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let registry = ExternalSignalRegistry::new();
                let cond = WaitCondition::external(" \t ");
                let err = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(60),
                    Some(&registry),
                    "workflow wait condition",
                )
                .await
                .expect_err("blank external signal key must abort");
                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("external signal key cannot be empty"),
                            "unexpected abort reason: {reason}"
                        );
                    }
                    other => panic!("expected Workflow(Aborted), got: {other:?}"),
                }
            });
        }

        /// Pre-fired signal must be observed immediately (well under timeout).
        #[test]
        fn external_wait_observes_pre_fired_signal() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let registry = ExternalSignalRegistry::new();
                registry.signal("ready");
                let cond = WaitCondition::external("ready");
                let start = std::time::Instant::now();
                wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(60),
                    Some(&registry),
                    "workflow wait condition",
                )
                .await
                .expect("pre-fired signal must be observed");
                let elapsed = start.elapsed();
                assert!(
                    elapsed < Duration::from_millis(500),
                    "pre-fired signal returned too slowly: {elapsed:?}"
                );
            });
        }

        /// Signal fired by another task during the wait must be observed
        /// well before the configured timeout.
        #[test]
        fn external_wait_unblocks_when_signal_fires_during_wait() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let registry = Arc::new(ExternalSignalRegistry::new());
                let cond = WaitCondition::external("late");
                let signaler = Arc::clone(&registry);
                let handle = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(80));
                    signaler.signal("late");
                });

                let start = std::time::Instant::now();
                wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_secs(30),
                    Some(registry.as_ref()),
                    "workflow wait condition",
                )
                .await
                .expect("late signal must be observed");
                let elapsed = start.elapsed();
                let _ = handle.join();
                assert!(
                    elapsed < Duration::from_secs(2),
                    "signal observed too late: {elapsed:?}"
                );
                assert!(
                    elapsed >= Duration::from_millis(40),
                    "signal observed before it could fire: {elapsed:?}"
                );
            });
        }

        /// Timeout still bounds the wait when no signal fires, and must fail
        /// closed so the workflow cannot advance without the external event.
        #[test]
        fn external_wait_aborts_at_timeout_when_signal_never_fires() {
            run_async_test(async {
                let cx = crate::cx::for_testing();
                let registry = ExternalSignalRegistry::new();
                let cond = WaitCondition::external("never");
                let start = std::time::Instant::now();
                let err = wait_condition_pause_with_cx(
                    &cx,
                    &cond,
                    Duration::from_millis(120),
                    Some(&registry),
                    "workflow wait condition",
                )
                .await
                .expect_err("timeout path must abort instead of advancing");
                let elapsed = start.elapsed();
                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("external signal 'never' timed out"),
                            "abort reason must name the missing signal: {reason}"
                        );
                    }
                    other => panic!("expected Workflow(Aborted), got: {other:?}"),
                }
                assert!(
                    elapsed >= Duration::from_millis(100),
                    "wait returned before timeout: {elapsed:?}"
                );
                assert!(
                    elapsed < Duration::from_secs(2),
                    "wait massively exceeded timeout: {elapsed:?}"
                );
            });
        }

        /// Pre-cancelled cx propagates through the External wait path.
        #[test]
        fn external_wait_observes_pre_cancelled_cx() {
            run_async_test(async {
                let registry = ExternalSignalRegistry::new();
                let cx = crate::cx::for_testing();
                cx.cancel_with(
                    crate::outcome::CancelKind::User,
                    Some("external wait pre-cancel"),
                );
                let err = wait_condition_pause_with_cx(
                    &cx,
                    &WaitCondition::external("anything"),
                    Duration::from_secs(60),
                    Some(&registry),
                    "workflow wait condition",
                )
                .await
                .expect_err("pre-cancelled cx must abort external wait");
                match err {
                    crate::Error::Workflow(crate::error::WorkflowError::Aborted(reason)) => {
                        assert!(
                            reason.contains("workflow wait condition cancelled"),
                            "unexpected abort reason: {reason}"
                        );
                    }
                    other => panic!("unexpected wait error: {other:?}"),
                }
            });
        }
    }

    // ========================================================================
    // ft-o2t7l — RwLock poison-recovery regression
    //
    // The fix replaces three `.unwrap()` call sites at register_workflow /
    // find_matching_workflow / find_workflow_by_name with
    // `unwrap_or_else(|poisoned| poisoned.into_inner())`. The data is
    // append-only `Vec<Arc<dyn Workflow>>` — recovery from poison is safe
    // (the worst-case mid-push panic state is "the panicking registration
    // didn't add the workflow," which is the correct recovery state).
    //
    // These tests pin the pattern. The first test validates that the
    // exact lock-recovery idiom used by the production code recovers
    // cleanly from a poisoned `std::sync::RwLock<Vec<Arc<dyn Workflow>>>`.
    // The second test validates that the same pattern handles the
    // double-poison (poison-after-poisoned-read) case, which can occur
    // if a panic fires during a lookup mid-iteration.
    // ========================================================================

    use crate::workflows::{BoxFuture, StepResult, Workflow, WorkflowContext, WorkflowStep};

    struct PoisonProbeWorkflow;

    impl Workflow for PoisonProbeWorkflow {
        fn name(&self) -> &'static str {
            "poison_probe"
        }

        fn description(&self) -> &'static str {
            "ft-o2t7l RwLock poison-recovery probe"
        }

        fn handles(&self, _detection: &crate::patterns::Detection) -> bool {
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
            Box::pin(async move { StepResult::done_empty() })
        }
    }

    #[test]
    fn rwlock_recovers_from_write_side_poison() {
        // Same shape as `WorkflowRunner.workflows`: an append-only
        // Vec<Arc<dyn Workflow>> behind a std::sync::RwLock.
        let workflows: Arc<std::sync::RwLock<Vec<Arc<dyn Workflow>>>> =
            Arc::new(std::sync::RwLock::new(Vec::new()));

        // Pre-register one workflow before the poison.
        {
            let mut g = workflows
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            g.push(Arc::new(PoisonProbeWorkflow));
        }

        // Poison the lock by panicking inside a write critical
        // section.
        let workflows_clone = Arc::clone(&workflows);
        let join = std::thread::spawn(move || {
            let _g = workflows_clone
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            panic!("intentional poison for ft-o2t7l regression");
        })
        .join();
        assert!(join.is_err(), "thread MUST panic to poison the lock");

        // Sanity — the lock is poisoned now.
        assert!(
            workflows.is_poisoned(),
            "lock must be poisoned after the panic"
        );

        // The pattern used by the fix: read recovers cleanly.
        let g = workflows
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(g.len(), 1, "pre-poison workflow must still be present");
        assert_eq!(g[0].name(), "poison_probe");

        // And subsequent writes also recover.
        drop(g);
        {
            let mut g = workflows
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            g.push(Arc::new(PoisonProbeWorkflow));
        }
        let g = workflows
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(g.len(), 2, "post-poison write must succeed");
    }

    // Note on read-side poison: `std::sync::RwLock` is only
    // poisoned when a writer panics. A read-guard panic does
    // NOT poison the lock (readers don't have exclusive
    // access; std doesn't treat their panics as state-
    // corrupting). The write-side test above is the only
    // reachable poisoning path; it's also the path the bead
    // identified in `register_workflow`.
}
