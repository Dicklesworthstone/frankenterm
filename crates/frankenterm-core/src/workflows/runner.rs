//! Event-driven workflow runner.
//!
//! Provides WorkflowRunner, WorkflowRunnerConfig, WorkflowStartResult,
//! and the event-driven execution loop that dispatches detections to
//! registered workflows.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

fn spawn_runner_child_with_cx<F, Fut>(cx: &crate::cx::Cx, task: F)
where
    F: FnOnce(crate::cx::Cx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    std::mem::drop(crate::runtime_compat::task::spawn_with_cx(cx, task));
}

fn workflow_wait_aborted(label: &str, err: impl std::fmt::Display) -> crate::Error {
    crate::Error::Workflow(crate::error::WorkflowError::Aborted(format!(
        "{label} cancelled: {err}"
    )))
}

async fn wait_duration_maybe_cx(
    cx: Option<&crate::cx::Cx>,
    duration: Duration,
    label: &str,
) -> Result<(), crate::Error> {
    if let Some(cx) = cx {
        let deadline = std::time::Instant::now() + duration;
        loop {
            cx.checkpoint()
                .map_err(|err| workflow_wait_aborted(label, err))?;

            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }

            let chunk = remaining.min(Duration::from_millis(50));
            crate::runtime_compat::sleep_with_cx(cx, chunk)
                .await
                .map_err(|err| workflow_wait_aborted(label, err))?;
        }
    }

    sleep(duration).await;
    Ok(())
}

async fn wait_condition_pause_maybe_cx(
    cx: Option<&crate::cx::Cx>,
    condition: &WaitCondition,
    timeout: Duration,
    label: &str,
) -> Result<(), crate::Error> {
    match condition {
        WaitCondition::PaneIdle {
            idle_threshold_ms, ..
        } => wait_duration_maybe_cx(cx, Duration::from_millis(*idle_threshold_ms), label).await,
        WaitCondition::Pattern { .. }
        | WaitCondition::TextMatch { .. }
        | WaitCondition::External { .. } => wait_duration_maybe_cx(cx, timeout, label).await,
        WaitCondition::StableTail { stable_for_ms, .. } => {
            wait_duration_maybe_cx(cx, Duration::from_millis(*stable_for_ms), label).await
        }
        WaitCondition::Sleep { duration_ms } => {
            wait_duration_maybe_cx(cx, Duration::from_millis(*duration_ms), label).await
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
    /// An error occurred
    Error {
        /// Error message
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
    /// Retry delay multiplier for exponential backoff
    pub retry_backoff_multiplier: f64,
    /// Maximum retries per step
    pub max_retries_per_step: usize,
}

impl Default for WorkflowRunnerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            step_timeout_ms: 30_000,
            retry_backoff_multiplier: 2.0,
            max_retries_per_step: 3,
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
        config: WorkflowRunnerConfig,
    ) -> Self {
        Self {
            workflows: std::sync::RwLock::new(Vec::new()),
            engine,
            lock_manager,
            storage,
            injector,
            replay_capture: None,
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

    /// Get the lock manager.
    pub fn lock_manager(&self) -> &Arc<PaneWorkflowLockManager> {
        &self.lock_manager
    }

    /// Register a workflow.
    pub fn register_workflow(&self, workflow: Arc<dyn Workflow>) {
        let mut workflows = self.workflows.write().unwrap();
        workflows.push(workflow);
    }

    /// Find a workflow that handles the given detection.
    pub fn find_matching_workflow(
        &self,
        detection: &crate::patterns::Detection,
    ) -> Option<Arc<dyn Workflow>> {
        let workflows = self.workflows.read().unwrap();
        workflows.iter().find(|w| w.handles(detection)).cloned()
    }

    /// Find a workflow by name.
    pub fn find_workflow_by_name(&self, name: &str) -> Option<Arc<dyn Workflow>> {
        let workflows = self.workflows.read().unwrap();
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
        // Find matching workflow
        let Some(workflow) = self.find_matching_workflow(detection) else {
            return WorkflowStartResult::NoMatchingWorkflow {
                rule_id: detection.rule_id.clone(),
            };
        };

        let workflow_name = workflow.name().to_string();

        // Try to acquire pane lock
        let execution_id = generate_workflow_id(&workflow_name);
        let lock_result = match self.lock_manager.try_acquire_with_limit(
            pane_id,
            &workflow_name,
            &execution_id,
            self.config.max_concurrent,
        ) {
            Ok(lock_result) => lock_result,
            Err(limit_info) => {
                return WorkflowStartResult::ConcurrencyLimitReached {
                    active: limit_info.active,
                    limit: limit_info.limit,
                };
            }
        };

        match lock_result {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                ..
            } => {
                return WorkflowStartResult::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                };
            }
            LockAcquisitionResult::Acquired => {
                // Lock acquired, start execution
            }
        }

        // Start workflow execution via engine
        let agent_type_str = match detection.agent_type {
            crate::patterns::AgentType::Codex => "codex",
            crate::patterns::AgentType::ClaudeCode => "claude_code",
            crate::patterns::AgentType::Gemini => "gemini",
            crate::patterns::AgentType::Wezterm => "wezterm",
            crate::patterns::AgentType::Unknown => "unknown",
        };
        let severity_str = format!("{:?}", detection.severity).to_lowercase();

        // IMPORTANT: workflows expect ctx.trigger() to include at least:
        // - agent_type
        // - event_type
        // - extracted
        //
        // Keep the legacy nested "detection" object for backward compatibility.
        let context = serde_json::json!({
            "rule_id": detection.rule_id,
            "agent_type": agent_type_str,
            "event_type": detection.event_type,
            "severity": severity_str,
            "confidence": detection.confidence,
            "extracted": detection.extracted,
            "matched_text": detection.matched_text,
            "span": { "start": detection.span.0, "end": detection.span.1 },
            "detection": {
                "rule_id": detection.rule_id,
                "matched_text": detection.matched_text,
                "severity": format!("{:?}", detection.severity),
            }
        });

        match self
            .engine
            .start_with_id(
                &self.storage,
                execution_id.clone(),
                &workflow_name,
                pane_id,
                event_id,
                Some(context),
            )
            .await
        {
            Ok(_execution) => WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            },
            Err(e) => {
                // Release lock on error
                self.lock_manager.release(pane_id, &execution_id);
                WorkflowStartResult::Error {
                    error: e.to_string(),
                }
            }
        }
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

        let execution_id = generate_workflow_id(&workflow_name);
        let lock_result = match self.lock_manager.try_acquire_with_limit(
            pane_id,
            &workflow_name,
            &execution_id,
            self.config.max_concurrent,
        ) {
            Ok(lock_result) => lock_result,
            Err(limit_info) => {
                return WorkflowStartResult::ConcurrencyLimitReached {
                    active: limit_info.active,
                    limit: limit_info.limit,
                };
            }
        };

        match lock_result {
            LockAcquisitionResult::AlreadyLocked {
                held_by_workflow,
                held_by_execution,
                ..
            } => {
                return WorkflowStartResult::PaneLocked {
                    pane_id,
                    held_by_workflow,
                    held_by_execution,
                };
            }
            LockAcquisitionResult::Acquired => {}
        }

        let agent_type_str = match detection.agent_type {
            crate::patterns::AgentType::Codex => "codex",
            crate::patterns::AgentType::ClaudeCode => "claude_code",
            crate::patterns::AgentType::Gemini => "gemini",
            crate::patterns::AgentType::Wezterm => "wezterm",
            crate::patterns::AgentType::Unknown => "unknown",
        };
        let severity_str = format!("{:?}", detection.severity).to_lowercase();

        let context = serde_json::json!({
            "rule_id": detection.rule_id,
            "agent_type": agent_type_str,
            "event_type": detection.event_type,
            "severity": severity_str,
            "confidence": detection.confidence,
            "extracted": detection.extracted,
            "matched_text": detection.matched_text,
            "span": { "start": detection.span.0, "end": detection.span.1 },
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
                execution_id.clone(),
                &workflow_name,
                pane_id,
                event_id,
                Some(context),
            )
            .await
        {
            Ok(_execution) => WorkflowStartResult::Started {
                execution_id,
                workflow_name,
            },
            Err(e) => {
                self.lock_manager.release(pane_id, &execution_id);
                WorkflowStartResult::Error {
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
        self.run_workflow_inner(None, pane_id, workflow, execution_id, start_step)
            .await
    }

    /// Tick 180: shared implementation for [`run_workflow`] and
    /// [`run_workflow_with_cx`]. When `cx` is `Some`, the 4 startup
    /// storage call sites (record_workflow_start_action,
    /// fetch_workflow_start_action_id, get_workflow, get_pane) route
    /// through their cx-first siblings so caller cancellation
    /// propagates through the full workflow-start handshake. When
    /// `cx` is `None` (legacy caller), the behavior is byte-for-byte
    /// identical to the pre-tick-180 body.
    ///
    /// The step-loop portion (after ctx setup) does not yet thread
    /// cx — future ticks (181+) will migrate per-step checkpoints
    /// and per-step-branch storage calls progressively.
    #[allow(clippy::too_many_arguments)]
    async fn run_workflow_inner(
        &self,
        cx: Option<&crate::cx::Cx>,
        pane_id: u64,
        workflow: Arc<dyn Workflow>,
        execution_id: &str,
        start_step: usize,
    ) -> WorkflowExecutionResult {
        let start_time = Instant::now();
        let workflow_name = workflow.name().to_string();
        let step_count = workflow.step_count();
        let mut current_step = start_step;
        let mut retries = 0;
        let mut jump_count: usize = 0;
        // Prevent infinite loops from backward JumpTo cycles.
        // A workflow with N steps should never need more than N*10 jumps.
        let max_total_jumps = step_count.saturating_mul(10).max(100);
        let start_action_id = if start_step == 0 {
            let rec = if let Some(cx) = cx {
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
            } else {
                record_workflow_start_action(
                    &self.storage,
                    &workflow_name,
                    execution_id,
                    pane_id,
                    step_count,
                    start_step,
                )
                .await
            };
            rec
        } else {
            let fetched = if let Some(cx) = cx {
                fetch_workflow_start_action_id_with_cx(cx, &self.storage, execution_id).await
            } else {
                fetch_workflow_start_action_id(&self.storage, execution_id).await
            };
            fetched
        };

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
        let maybe_wf = if let Some(cx) = cx {
            self.storage.get_workflow_with_cx(cx, execution_id).await
        } else {
            self.storage.get_workflow(execution_id).await
        };
        if let Ok(Some(record)) = maybe_wf {
            if let Some(trigger) = record.context {
                ctx = ctx.with_trigger(trigger);
            }
        }

        let maybe_pane = if let Some(cx) = cx {
            self.storage.get_pane_with_cx(cx, pane_id).await
        } else {
            self.storage.get_pane(pane_id).await
        };
        if let Ok(Some(record)) = maybe_pane {
            ctx.set_pane_meta(PaneMetadata::from_record(&record));
        }

        if let Some(adapter) = self.replay_capture.as_ref() {
            let request_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
            let injector_cx = cx.unwrap_or(&request_cx);
            self.injector
                .set_decision_capture(injector_cx, adapter.clone())
                .await;
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
                if let Err(e) = self
                    .fail_execution_maybe_cx(cx, execution_id, &reason)
                    .await
                {
                    tracing::warn!(
                        execution_id,
                        error = %e,
                        "Failed to fail execution after plan validation error"
                    );
                }
                if let Err(e) = self
                    .mark_trigger_event_handled_maybe_cx(cx, execution_id, "error")
                    .await
                {
                    tracing::warn!(
                        execution_id,
                        error = %e,
                        "Failed to mark trigger event as handled after plan validation error"
                    );
                }
                self.lock_manager.release(pane_id, execution_id);
                record_workflow_terminal_action_maybe_cx(
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
                .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: reason,
                };
            }

            // ft-xbnl0.2.3 tick 259: cx-first action-plan persist with maybe-cx fallback.
            let persist_result = if let Some(cx) = cx {
                self.storage
                    .upsert_action_plan_with_cx(cx, execution_id, &plan)
                    .await
            } else {
                self.storage.upsert_action_plan(execution_id, &plan).await
            };
            if let Err(e) = persist_result {
                tracing::warn!(
                    execution_id,
                    error = %e,
                    "Failed to persist action plan"
                );
            }

            ctx.set_action_plan(plan);
        }

        while current_step < step_count {
            // Tick 181: per-step cancellation seam. On cx-cancel
            // we do the same cleanup sequence as a plan-validation
            // error (fail_execution + mark_trigger_event_handled +
            // lock release + terminal-action audit) so the pane
            // lock and trigger state don't leak under a cancelled
            // parent. No-op when cx is None (legacy caller) or
            // healthy.
            if let Some(cx) = cx {
                if let Err(err) = cx.checkpoint() {
                    let reason = format!("run_workflow cancelled at step {current_step}: {err}");
                    if let Err(e) = self.fail_execution_with_cx(cx, execution_id, &reason).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to fail execution on cx cancel"
                        );
                    }
                    if let Err(e) = self
                        .mark_trigger_event_handled_with_cx(cx, execution_id, "error")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event handled on cx cancel"
                        );
                    }
                    self.lock_manager.release(pane_id, execution_id);
                    record_workflow_terminal_action_with_cx(
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
                    .await;
                    return WorkflowExecutionResult::Error {
                        execution_id: Some(execution_id.to_string()),
                        error: reason,
                    };
                }
            }

            let step_plan = ctx.get_step_plan(current_step).cloned();
            let mut idempotency_skip: Option<(i64, Option<String>)> = None;
            let mut idempotency_abort: Option<String> = None;
            let step_started_at = now_ms();

            if let Some(step_plan) = step_plan.as_ref() {
                if step_plan.idempotent {
                    match check_step_idempotency_maybe_cx(
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
                // Tick 187: route step execution through execute_step_cx
                // when cx is Some so workflow implementors that override
                // the cx-first trait method (ft-xbnl0.2.2) can observe
                // caller cancellation mid-step. Implementors that use
                // the default trait impl still receive ambient semantics
                // — the default delegates to execute_step unchanged.
                let step_outcome = if let Some(cx) = cx {
                    workflow.execute_step_cx(cx, &mut ctx, current_step).await
                } else {
                    workflow.execute_step(&mut ctx, current_step).await
                };
                step_outcome
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
            let verification_refs = build_verification_refs(&step_result, step_plan_ref);
            let step_error_code = step_error_code_from_result(&step_result);
            let log_step_result = redacted_step_result_for_logging(&step_result);

            if let Some(adapter) = self.replay_capture.as_ref() {
                let step_definition_text = steps
                    .get(current_step)
                    .and_then(|step| serde_json::to_string(step).ok())
                    .unwrap_or_else(|| step_name.to_string());
                let step_input = serde_json::json!({
                    "workflow_name": workflow_name.as_str(),
                    "execution_id": execution_id,
                    "pane_id": pane_id,
                    "step_index": current_step,
                    "step_name": step_name,
                    "trigger": ctx.trigger().cloned().unwrap_or(serde_json::Value::Null),
                });
                let step_input_text = serde_json::to_string(&step_input)
                    .unwrap_or_else(|_| format!("workflow={workflow_name};step={current_step}"));
                let step_output = serde_json::to_value(&log_step_result).unwrap_or_else(|_| {
                    serde_json::json!({
                        "result_type": result_type,
                    })
                });
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
                adapter.capture_decision(
                    crate::recording::RecorderEventSource::WorkflowEngine,
                    Some(execution_id.to_string()),
                    decision_event,
                );
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

                serde_json::to_string(&data)
                    .inspect_err(
                        |e| tracing::warn!(error = %e, "workflow step data serialization failed"),
                    )
                    .ok()
            };
            let step_completed_at = now_ms();

            // Persist step log for non-SendText steps
            // SendText steps are logged after injection to capture the audit_action_id (wa-nu4.1.1.11)
            if !matches!(&step_result, StepResult::SendText { .. }) {
                let step_audit_action_id = record_workflow_step_action(
                    &self.storage,
                    &workflow_name,
                    execution_id,
                    pane_id,
                    current_step,
                    step_name,
                    step_id.clone(),
                    step_kind.clone(),
                    result_type,
                    start_action_id,
                )
                .await;

                if let Err(e) = self
                    .storage
                    .insert_step_log(
                        execution_id,
                        step_audit_action_id,
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
                    tracing::warn!(
                        workflow = %workflow_name,
                        execution_id,
                        step = current_step,
                        error = %e,
                        "Failed to log step"
                    );
                }
            }

            // Handle step result
            match step_result {
                StepResult::Continue => {
                    current_step += 1;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self
                        .update_execution_step_maybe_cx(cx, execution_id, current_step)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action_maybe_cx(
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
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::JumpTo { step } => {
                    jump_count += 1;
                    if jump_count > max_total_jumps {
                        tracing::error!(
                            execution_id,
                            jump_count,
                            "Workflow exceeded maximum jump count ({max_total_jumps}); aborting to prevent infinite loop"
                        );
                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason: format!("exceeded maximum jump count ({max_total_jumps})"),
                            step_index: current_step,
                            elapsed_ms: elapsed_ms(start_time),
                        };
                    }
                    current_step = step;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self
                        .update_execution_step_maybe_cx(cx, execution_id, current_step)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after jump"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action_maybe_cx(
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
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }
                }
                StepResult::Done { result } => {
                    // Workflow completed
                    let elapsed_ms = elapsed_ms(start_time);

                    // Update execution to completed
                    if let Err(e) = self
                        .complete_execution_maybe_cx(cx, execution_id, Some(result.clone()))
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to complete execution"
                        );
                    }

                    // Mark trigger event as handled
                    if let Err(e) = self
                        .mark_trigger_event_handled_maybe_cx(cx, execution_id, "completed")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event as handled"
                        );
                    }

                    // Release lock
                    self.lock_manager.release(pane_id, execution_id);

                    record_workflow_terminal_action_maybe_cx(
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
                    .await;

                    return WorkflowExecutionResult::Completed {
                        execution_id: execution_id.to_string(),
                        result,
                        elapsed_ms,
                        steps_executed: current_step + 1,
                    };
                }
                StepResult::Retry { delay_ms } => {
                    retries += 1;
                    if retries > self.config.max_retries_per_step {
                        let elapsed_ms = elapsed_ms(start_time);
                        let reason = format!(
                            "Max retries ({}) exceeded at step {}",
                            self.config.max_retries_per_step, current_step
                        );

                        // Update execution to failed
                        if let Err(e) = self
                            .fail_execution_maybe_cx(cx, execution_id, &reason)
                            .await
                        {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to fail execution"
                            );
                        }

                        // Mark trigger event as handled (with failed status)
                        if let Err(e) = self
                            .mark_trigger_event_handled_maybe_cx(cx, execution_id, "failed")
                            .await
                        {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to mark trigger event as handled"
                            );
                        }

                        // Cleanup and release lock
                        workflow.cleanup(&mut ctx).await;
                        self.lock_manager.release(pane_id, execution_id);

                        record_workflow_terminal_action_maybe_cx(
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
                        .await;

                        return WorkflowExecutionResult::Aborted {
                            execution_id: execution_id.to_string(),
                            reason,
                            step_index: current_step,
                            elapsed_ms,
                        };
                    }

                    // Wait before retry
                    sleep(Duration::from_millis(delay_ms)).await;
                }
                StepResult::Abort { reason } => {
                    let elapsed_ms = elapsed_ms(start_time);

                    // Update execution to failed
                    if let Err(e) = self
                        .fail_execution_maybe_cx(cx, execution_id, &reason)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to fail execution"
                        );
                    }

                    // Mark trigger event as handled (with aborted status)
                    if let Err(e) = self
                        .mark_trigger_event_handled_maybe_cx(cx, execution_id, "aborted")
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to mark trigger event as handled"
                        );
                    }

                    // Cleanup and release lock
                    workflow.cleanup(&mut ctx).await;
                    self.lock_manager.release(pane_id, execution_id);

                    record_workflow_terminal_action_maybe_cx(
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
                    .await;

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
                        .set_execution_waiting_maybe_cx(cx, execution_id, current_step, &condition)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to set waiting state"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action_maybe_cx(
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
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
                        }
                    }

                    // Execute wait condition
                    let timeout = timeout_ms.map_or_else(
                        || Duration::from_millis(self.config.step_timeout_ms),
                        Duration::from_millis,
                    );

                    if let Err(e) = wait_condition_pause_maybe_cx(
                        cx,
                        &condition,
                        timeout,
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
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action_maybe_cx(
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
                            .await;
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
                        .update_execution_step_maybe_cx(cx, execution_id, current_step)
                        .await
                    {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to update execution step after wait"
                        );
                        if let crate::Error::Workflow(crate::error::WorkflowError::Aborted(
                            reason,
                        )) = e
                        {
                            self.lock_manager.release(pane_id, execution_id);
                            record_workflow_terminal_action_maybe_cx(
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
                            .await;
                            return WorkflowExecutionResult::Aborted {
                                execution_id: execution_id.to_string(),
                                reason,
                                step_index: current_step,
                                elapsed_ms: elapsed_ms(start_time),
                            };
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

                    let request_cx =
                        crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
                    let injector_cx = cx.unwrap_or(&request_cx);
                    let send_result = self
                        .injector
                        .send_text(
                            injector_cx,
                            pane_id,
                            &text,
                            crate::policy::ActorKind::Workflow,
                            ctx.capabilities(),
                            Some(execution_id),
                        )
                        .await;

                    // Log the SendText step with audit_action_id (wa-nu4.1.1.11)
                    let audit_action_id = send_result.audit_action_id();
                    let policy_summary = policy_summary_from_injection(&send_result);
                    let policy_error_code = policy_error_code_from_injection(&send_result);
                    if let Err(e) = self
                        .storage
                        .insert_step_log(
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
                        tracing::warn!(
                            workflow = %workflow_name,
                            execution_id,
                            step = current_step,
                            ?audit_action_id,
                            error = %e,
                            "Failed to log SendText step"
                        );
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

                                if let Err(e) = wait_condition_pause_maybe_cx(
                                    cx,
                                    &condition,
                                    timeout,
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
                                        self.lock_manager.release(pane_id, execution_id);
                                        record_workflow_terminal_action_maybe_cx(
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
                                        .await;
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
                                .update_execution_step_maybe_cx(cx, execution_id, current_step)
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to update execution step after send"
                                );
                                if let crate::Error::Workflow(
                                    crate::error::WorkflowError::Aborted(reason),
                                ) = e
                                {
                                    self.lock_manager.release(pane_id, execution_id);
                                    record_workflow_terminal_action_maybe_cx(
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
                                    .await;
                                    return WorkflowExecutionResult::Aborted {
                                        execution_id: execution_id.to_string(),
                                        reason,
                                        step_index: current_step,
                                        elapsed_ms: elapsed_ms(start_time),
                                    };
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

                            // Update execution to failed
                            if let Err(e) = self
                                .fail_execution_maybe_cx(cx, execution_id, &abort_reason)
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with denied status)
                            if let Err(e) = self
                                .mark_trigger_event_handled_maybe_cx(cx, execution_id, "denied")
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action_maybe_cx(
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
                            .await;

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

                            // Update execution to failed (approval not auto-granted for workflows)
                            if let Err(e) = self
                                .fail_execution_maybe_cx(cx, execution_id, &abort_reason)
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with requires_approval status)
                            if let Err(e) = self
                                .mark_trigger_event_handled_maybe_cx(
                                    cx,
                                    execution_id,
                                    "requires_approval",
                                )
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action_maybe_cx(
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
                            .await;

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

                            // Update execution to failed
                            if let Err(e) = self
                                .fail_execution_maybe_cx(cx, execution_id, &abort_reason)
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with error status)
                            if let Err(e) = self
                                .mark_trigger_event_handled_maybe_cx(cx, execution_id, "error")
                                .await
                            {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to mark trigger event as handled"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action_maybe_cx(
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
                            .await;

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
            .complete_execution_maybe_cx(cx, execution_id, Some(result.clone()))
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to complete execution"
            );
        }

        // Mark trigger event as handled
        if let Err(e) = self
            .mark_trigger_event_handled_maybe_cx(cx, execution_id, "completed")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled"
            );
        }

        self.lock_manager.release(pane_id, execution_id);

        record_workflow_terminal_action_maybe_cx(
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
        .await;

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
    /// Tick 180: upgraded to route through the shared
    /// `run_workflow_inner(Some(cx), ...)` path so the 4 startup
    /// storage call sites (record_workflow_start_action,
    /// fetch_workflow_start_action_id, get_workflow, get_pane)
    /// all thread the caller's cx. The step-loop portion after
    /// ctx setup is still on the ambient path; future ticks
    /// (181+) will migrate per-step cancellation.
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
        if let Err(err) = cx.checkpoint() {
            let reason = format!("run_workflow cancelled pre-start: {err}");
            self.lock_manager.release(pane_id, execution_id);
            if let Err(cleanup_err) = self.fail_execution(execution_id, &reason).await {
                tracing::warn!(
                    execution_id,
                    error = %cleanup_err,
                    "Failed to mark pre-start cancelled workflow as failed"
                );
            }
            if let Err(cleanup_err) = self.mark_trigger_event_handled(execution_id, "error").await {
                tracing::warn!(
                    execution_id,
                    error = %cleanup_err,
                    "Failed to mark trigger event handled after pre-start cancellation"
                );
            }
            return WorkflowExecutionResult::Error {
                execution_id: Some(execution_id.to_string()),
                error: reason,
            };
        }
        self.run_workflow_inner(Some(cx), pane_id, workflow, execution_id, start_step)
            .await
    }

    /// Run the event loop, subscribing to detection events.
    ///
    /// This spawns workflow executions for matching detections. The loop
    /// runs until the event bus channel is closed.
    ///
    /// On startup, resumes any incomplete workflows that were interrupted
    /// (e.g., by a previous watcher crash or restart).
    pub async fn run(&self, event_bus: &crate::events::EventBus) {
        // Resume any incomplete workflows from a previous run
        let resumed = self.resume_incomplete().await;
        if !resumed.is_empty() {
            tracing::info!(
                count = resumed.len(),
                "Resumed incomplete workflows from previous run"
            );
            for result in &resumed {
                match result {
                    WorkflowExecutionResult::Completed { execution_id, .. } => {
                        tracing::info!(execution_id, "Resumed workflow completed");
                    }
                    WorkflowExecutionResult::Error {
                        execution_id,
                        error,
                    } => {
                        tracing::warn!(?execution_id, error, "Resumed workflow errored");
                    }
                    _ => {}
                }
            }
        }

        let mut subscriber = event_bus.subscribe_detections();

        loop {
            match subscriber.recv().await {
                Ok(event) => {
                    if let crate::events::Event::PatternDetected {
                        pane_id,
                        pane_uuid: _,
                        detection,
                        event_id,
                    } = event
                    {
                        // Handle detection with event_id for proper event lifecycle
                        let result = self.handle_detection(pane_id, &detection, event_id).await;

                        match result {
                            WorkflowStartResult::Started {
                                execution_id,
                                workflow_name,
                            } => {
                                // Find workflow and spawn execution
                                if let Some(workflow) = self.find_workflow_by_name(&workflow_name) {
                                    let execution_id_clone = execution_id.clone();
                                    let workflow_clone = Arc::clone(&workflow);
                                    let storage = Arc::clone(&self.storage);
                                    let lock_manager = Arc::clone(&self.lock_manager);
                                    let config = self.config.clone();
                                    let engine = WorkflowEngine::new(config.max_concurrent);

                                    // Create a mini-runner for the spawned task
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
                                    };

                                    let request_cx = crate::cx::Cx::current()
                                        .unwrap_or_else(crate::cx::for_request);
                                    crate::runtime_compat::task::spawn_with_cx(
                                        &request_cx,
                                        move |_child_cx| async move {
                                            let result = runner
                                                .run_workflow(
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
                                                        "Workflow completed"
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
                                                        "Workflow aborted"
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
                                                        "Workflow denied by policy"
                                                    );
                                                }
                                                WorkflowExecutionResult::Error {
                                                    execution_id,
                                                    error,
                                                } => {
                                                    tracing::error!(
                                                        execution_id = execution_id.as_deref(),
                                                        error,
                                                        "Workflow error"
                                                    );
                                                }
                                            }
                                        },
                                    );
                                }
                            }
                            WorkflowStartResult::NoMatchingWorkflow { rule_id } => {
                                tracing::debug!(rule_id, "No workflow handles detection");
                            }
                            WorkflowStartResult::PaneLocked {
                                pane_id,
                                held_by_workflow,
                                ..
                            } => {
                                tracing::debug!(
                                    pane_id,
                                    held_by = %held_by_workflow,
                                    "Pane locked, skipping detection"
                                );
                            }
                            WorkflowStartResult::ConcurrencyLimitReached { active, limit } => {
                                tracing::debug!(
                                    active,
                                    limit,
                                    "Workflow concurrency limit reached, skipping detection"
                                );
                            }
                            WorkflowStartResult::Error { error } => {
                                tracing::error!(error, "Failed to start workflow");
                            }
                        }
                    }
                }
                Err(crate::events::RecvError::Lagged { missed_count }) => {
                    tracing::warn!(
                        skipped = missed_count,
                        "Workflow runner lagged, skipped events"
                    );
                }
                Err(crate::events::RecvError::Closed) => {
                    tracing::info!("Event bus closed, workflow runner stopping");
                    break;
                }
            }
        }
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

        loop {
            if cx.checkpoint().is_err() {
                tracing::info!(
                    explicit_cx = true,
                    "Workflow runner loop cancelled via Cx, stopping"
                );
                break;
            }
            match subscriber.recv_cx(cx).await {
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
                                    };

                                    spawn_runner_child_with_cx(cx, move |child_cx| async move {
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
                Err(crate::events::RecvError::Closed) => {
                    tracing::info!(
                        explicit_cx = true,
                        "Event bus closed, workflow runner stopping (cx)"
                    );
                    break;
                }
            }
        }
    }

    /// Resume incomplete workflows after restart.
    ///
    /// Queries storage for workflows with status 'running' or 'waiting'
    /// and attempts to resume them.
    pub async fn resume_incomplete(&self) -> Vec<WorkflowExecutionResult> {
        let incomplete = match self.storage.find_incomplete_workflows().await {
            Ok(workflows) => workflows,
            Err(e) => {
                tracing::error!(error = %e, "Failed to query incomplete workflows");
                return vec![];
            }
        };

        let mut results = Vec::new();

        for record in incomplete {
            // Find the workflow definition
            let Some(workflow) = self.find_workflow_by_name(&record.workflow_name) else {
                tracing::warn!(
                    workflow_name = %record.workflow_name,
                    execution_id = %record.id,
                    "Cannot resume: workflow not registered"
                );
                continue;
            };

            let (execution, next_step) = match self.engine.resume(&self.storage, &record.id).await {
                Ok(Some(resume)) => resume,
                Ok(None) => {
                    tracing::debug!(
                        execution_id = %record.id,
                        "Skipping resume for workflow already in a terminal state"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        execution_id = %record.id,
                        error = %e,
                        "Failed to load workflow state for resume"
                    );
                    continue;
                }
            };

            // Try to re-acquire lock
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
                        "Cannot resume: pane locked"
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
                "Resuming workflow"
            );

            let result = self
                .run_workflow(execution.pane_id, workflow, &execution.id, next_step)
                .await;

            results.push(result);
        }

        results
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

    async fn update_execution_step(&self, execution_id: &str, step: usize) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if workflow was externally aborted/completed
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

        self.storage.upsert_workflow(record).await
    }

    /// Tick 182: dispatcher used by `run_workflow_inner` step-loop
    /// call sites. When `cx` is `Some`, routes through
    /// `update_execution_step_with_cx`; otherwise falls through to
    /// the legacy `update_execution_step`. Keeps the 4 call sites
    /// inside the step loop to a single `.await` line rather than
    /// a match-on-cx boilerplate at each site.
    async fn update_execution_step_maybe_cx(
        &self,
        cx: Option<&crate::cx::Cx>,
        execution_id: &str,
        step: usize,
    ) -> crate::Result<()> {
        if let Some(cx) = cx {
            return self
                .update_execution_step_with_cx(cx, execution_id, step)
                .await;
        }
        self.update_execution_step(execution_id, step).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`update_execution_step`].
    ///
    /// Tick 182: routes both the read (get_workflow_with_cx) and
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

    async fn set_execution_waiting(
        &self,
        execution_id: &str,
        step: usize,
        condition: &WaitCondition,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
            .await?
            .ok_or_else(|| {
                crate::Error::Workflow(crate::error::WorkflowError::NotFound(
                    execution_id.to_string(),
                ))
            })?;

        // Check if workflow was externally aborted/completed
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

        self.storage.upsert_workflow(record).await
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`set_execution_waiting`].
    /// Tick 184: routes get_workflow + upsert_workflow through _with_cx.
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

    /// Tick 184: dispatcher for set_execution_waiting call sites.
    async fn set_execution_waiting_maybe_cx(
        &self,
        cx: Option<&crate::cx::Cx>,
        execution_id: &str,
        step: usize,
        condition: &WaitCondition,
    ) -> crate::Result<()> {
        if let Some(cx) = cx {
            return self
                .set_execution_waiting_with_cx(cx, execution_id, step, condition)
                .await;
        }
        self.set_execution_waiting(execution_id, step, condition)
            .await
    }

    async fn complete_execution(
        &self,
        execution_id: &str,
        result: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
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

        self.storage.upsert_workflow(record).await?;
        if let Err(err) = self.storage.record_usage_metric(metric).await {
            tracing::warn!(pane_id, error = %err, "Failed to record workflow completion metric");
        }
        Ok(())
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`complete_execution`].
    /// Tick 184: routes get_workflow + upsert_workflow + record_usage_metric
    /// through _with_cx. Metric-write failure stays warn-only (matches
    /// tick 183's fail_execution_with_cx contract).
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
        if let Err(err) = self.storage.record_usage_metric_with_cx(cx, metric).await {
            tracing::warn!(pane_id, error = %err, "Failed to record workflow completion metric (cx path)");
        }
        Ok(())
    }

    /// Tick 184: dispatcher for complete_execution call sites.
    async fn complete_execution_maybe_cx(
        &self,
        cx: Option<&crate::cx::Cx>,
        execution_id: &str,
        result: Option<serde_json::Value>,
    ) -> crate::Result<()> {
        if let Some(cx) = cx {
            return self
                .complete_execution_with_cx(cx, execution_id, result)
                .await;
        }
        self.complete_execution(execution_id, result).await
    }

    async fn fail_execution(&self, execution_id: &str, error: &str) -> crate::Result<()> {
        let mut record = self
            .storage
            .get_workflow(execution_id)
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

        self.storage.upsert_workflow(record).await?;
        if let Err(err) = self.storage.record_usage_metric(metric).await {
            tracing::warn!(pane_id, error = %err, "Failed to record workflow failure metric");
        }
        Ok(())
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`fail_execution`].
    ///
    /// Tick 183: all 3 storage calls (get_workflow + upsert_workflow
    /// + record_usage_metric) route through their cx-first siblings.
    ///   Keeps the "metric-write failure is warn-only" contract — the
    ///   metric write uses `if let Err` on the cx-first path same as
    ///   legacy, so a cx-cancelled metric write logs-and-continues.
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
        if let Err(err) = self.storage.record_usage_metric_with_cx(cx, metric).await {
            tracing::warn!(pane_id, error = %err, "Failed to record workflow failure metric (cx path)");
        }
        Ok(())
    }

    /// Tick 183: dispatcher for fail_execution call sites.
    async fn fail_execution_maybe_cx(
        &self,
        cx: Option<&crate::cx::Cx>,
        execution_id: &str,
        error: &str,
    ) -> crate::Result<()> {
        if let Some(cx) = cx {
            return self.fail_execution_with_cx(cx, execution_id, error).await;
        }
        self.fail_execution(execution_id, error).await
    }

    /// Mark the triggering event as handled after workflow completion.
    ///
    /// This ensures proper event lifecycle management - events that triggered
    /// workflows are marked with the outcome so they won't be re-processed.
    ///
    /// # Arguments
    /// * `execution_id` - The workflow execution ID
    /// * `status` - The handling status ("completed", "failed", "aborted", "denied")
    async fn mark_trigger_event_handled(
        &self,
        execution_id: &str,
        status: &str,
    ) -> crate::Result<()> {
        // Get the workflow record to find trigger_event_id
        let record = self.storage.get_workflow(execution_id).await?;

        if let Some(record) = record {
            if let Some(event_id) = record.trigger_event_id {
                self.storage
                    .mark_event_handled(event_id, Some(execution_id.to_string()), status)
                    .await?;

                tracing::debug!(
                    execution_id,
                    event_id,
                    status,
                    "Marked trigger event as handled"
                );
            }
        }

        Ok(())
    }

    /// ft-xbnl0.2.3 Cx-first sibling of [`mark_trigger_event_handled`].
    /// Tick 184: routes get_workflow + mark_event_handled through _with_cx.
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

    /// Tick 184: dispatcher for mark_trigger_event_handled call sites.
    async fn mark_trigger_event_handled_maybe_cx(
        &self,
        cx: Option<&crate::cx::Cx>,
        execution_id: &str,
        status: &str,
    ) -> crate::Result<()> {
        if let Some(cx) = cx {
            return self
                .mark_trigger_event_handled_with_cx(cx, execution_id, status)
                .await;
        }
        self.mark_trigger_event_handled(execution_id, status).await
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
        _force: bool, // Reserved for future cleanup skipping
    ) -> crate::Result<AbortResult> {
        // Load the workflow record
        let record = self
            .storage
            .get_workflow(execution_id)
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
            _ => {} // running, waiting - proceed with abort
        }

        let previous_status = record.status.clone();
        let workflow_name = record.workflow_name.clone();
        let pane_id = record.pane_id;
        let aborted_at_step = record.current_step;
        let now = now_ms();

        // Update the record to aborted status
        let mut updated_record = record;
        updated_record.status = "aborted".to_string();
        updated_record.error = reason.map(|r| format!("Aborted: {r}"));
        updated_record.updated_at = now;
        updated_record.completed_at = Some(now);

        self.storage.upsert_workflow(updated_record).await?;

        // Release the pane lock if held
        self.lock_manager.release(pane_id, execution_id);

        // Mark trigger event as handled with aborted status
        if let Err(e) = self
            .mark_trigger_event_handled(execution_id, "aborted")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled during abort"
            );
        }

        tracing::info!(
            execution_id,
            workflow_name,
            pane_id,
            reason = reason.unwrap_or("no reason provided"),
            "Workflow aborted"
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
            return Err(crate::Error::Runtime(
                "capability context already cancelled; abort_execution refused".to_owned(),
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
            crate::Error::Runtime(format!(
                "abort_execution cancelled between get_workflow and upsert_workflow (exec_id={execution_id}): {err}"
            ))
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

        self.storage
            .upsert_workflow_with_cx(cx, updated_record)
            .await?;

        self.lock_manager.release(pane_id, execution_id);

        if let Err(e) = self
            .mark_trigger_event_handled_with_cx(cx, execution_id, "aborted")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled during abort (cx-first)"
            );
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
    use crate::runtime_compat::CompatRuntime;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .build()
            .expect("failed to build runtime for async test");
        let test_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        crate::runtime_compat::clear_runtime_handle();
        if let Err(payload) = test_result {
            std::panic::resume_unwind(payload);
        }
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
        };
        assert_eq!(config.max_concurrent, 10);
        assert_eq!(config.step_timeout_ms, 60_000);
        assert!((config.retry_backoff_multiplier - 1.5).abs() < f64::EPSILON);
        assert_eq!(config.max_retries_per_step, 5);
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
            });
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

        /// wait_duration_maybe_cx with cx=None completes after sleep.
        #[test]
        fn wait_duration_no_cx_completes() {
            run_async_test(async {
                let result =
                    wait_duration_maybe_cx(None, Duration::from_millis(1), "test-no-cx").await;
                assert!(result.is_ok());
            });
        }

        /// wait_duration_maybe_cx with zero duration returns immediately.
        #[test]
        fn wait_duration_zero_returns_immediately() {
            run_async_test(async {
                let result = wait_duration_maybe_cx(None, Duration::ZERO, "test-zero").await;
                assert!(result.is_ok());
            });
        }

        /// wait_condition_pause_maybe_cx dispatches Sleep variant correctly.
        #[test]
        fn wait_condition_sleep_no_cx() {
            run_async_test(async {
                let cond = WaitCondition::sleep(1);
                let result = wait_condition_pause_maybe_cx(
                    None,
                    &cond,
                    Duration::from_secs(1),
                    "test-sleep",
                )
                .await;
                assert!(result.is_ok());
            });
        }

        /// wait_condition_pause_maybe_cx dispatches PaneIdle variant correctly.
        #[test]
        fn wait_condition_pane_idle_no_cx() {
            run_async_test(async {
                let cond = WaitCondition::pane_idle(1);
                let result =
                    wait_condition_pause_maybe_cx(None, &cond, Duration::from_secs(1), "test-idle")
                        .await;
                assert!(result.is_ok());
            });
        }

        /// wait_condition_pause_maybe_cx dispatches StableTail variant correctly.
        #[test]
        fn wait_condition_stable_tail_no_cx() {
            run_async_test(async {
                let cond = WaitCondition::stable_tail(1);
                let result =
                    wait_condition_pause_maybe_cx(None, &cond, Duration::from_secs(1), "test-tail")
                        .await;
                assert!(result.is_ok());
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

                let err = wait_condition_pause_maybe_cx(
                    Some(&cx),
                    &WaitCondition::sleep(250),
                    Duration::from_secs(1),
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
    }
}
