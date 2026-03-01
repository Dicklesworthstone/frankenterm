//! Event-driven workflow runner.
//!
//! Provides WorkflowRunner, WorkflowRunnerConfig, WorkflowStartResult,
//! and the event-driven execution loop that dispatches detections to
//! registered workflows.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

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
    injector: PolicyInjectorHandle,
    /// Optional replay capture adapter for decision provenance.
    replay_capture: Option<crate::replay_capture::SharedCaptureAdapter>,
    /// Configuration
    config: WorkflowRunnerConfig,
}

impl WorkflowRunner {
    /// Create a new workflow runner.
    pub fn new(
        engine: WorkflowEngine,
        lock_manager: Arc<PaneWorkflowLockManager>,
        storage: Arc<crate::storage::StorageHandle>,
        injector: PolicyInjectorHandle,
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
        let lock_result = self
            .lock_manager
            .try_acquire(pane_id, &workflow_name, &execution_id);

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
        let start_time = Instant::now();
        let workflow_name = workflow.name().to_string();
        let step_count = workflow.step_count();
        let mut current_step = start_step;
        let mut retries = 0;
        let start_action_id = if start_step == 0 {
            record_workflow_start_action(
                &self.storage,
                &workflow_name,
                execution_id,
                pane_id,
                step_count,
                start_step,
            )
            .await
        } else {
            fetch_workflow_start_action_id(&self.storage, execution_id).await
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
        .with_injector(Arc::clone(&self.injector));

        // Attach persisted trigger context (if any) so workflows can interpret extracted fields.
        if let Ok(Some(record)) = self.storage.get_workflow(execution_id).await {
            if let Some(trigger) = record.context {
                ctx = ctx.with_trigger(trigger);
            }
        }

        if let Ok(Some(record)) = self.storage.get_pane(pane_id).await {
            ctx.set_pane_meta(PaneMetadata::from_record(&record));
        }

        if let Some(adapter) = self.replay_capture.as_ref() {
            let mut injector = self.injector.lock().await;
            injector.set_decision_capture(adapter.clone());
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
                record_workflow_terminal_action(
                    &self.storage,
                    &workflow_name,
                    execution_id,
                    pane_id,
                    "workflow_error",
                    "error",
                    Some(&reason),
                    Some(start_step),
                    None,
                    start_action_id,
                )
                .await;
                return WorkflowExecutionResult::Error {
                    execution_id: Some(execution_id.to_string()),
                    error: reason,
                };
            }

            if let Err(e) = self.storage.upsert_action_plan(execution_id, &plan).await {
                tracing::warn!(
                    execution_id,
                    error = %e,
                    "Failed to persist action plan"
                );
            }

            ctx.set_action_plan(plan);
        }

        while current_step < step_count {
            let step_plan = ctx.get_step_plan(current_step).cloned();
            let mut idempotency_skip: Option<(i64, Option<String>)> = None;
            let mut idempotency_abort: Option<String> = None;

            if let Some(step_plan) = step_plan.as_ref() {
                if step_plan.idempotent {
                    match check_step_idempotency(
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
                workflow.execute_step(&mut ctx, current_step).await
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
            let step_started_at = now_ms();
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
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
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
                            record_workflow_terminal_action(
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
                    current_step = step;
                    retries = 0;

                    // Update execution state
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
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
                            record_workflow_terminal_action(
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
                        .complete_execution(execution_id, Some(result.clone()))
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
                        .mark_trigger_event_handled(execution_id, "completed")
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

                    record_workflow_terminal_action(
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
                        if let Err(e) = self.fail_execution(execution_id, &reason).await {
                            tracing::warn!(
                                execution_id,
                                error = %e,
                                "Failed to fail execution"
                            );
                        }

                        // Mark trigger event as handled (with failed status)
                        if let Err(e) = self
                            .mark_trigger_event_handled(execution_id, "failed")
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

                        record_workflow_terminal_action(
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
                    if let Err(e) = self.fail_execution(execution_id, &reason).await {
                        tracing::warn!(
                            execution_id,
                            error = %e,
                            "Failed to fail execution"
                        );
                    }

                    // Mark trigger event as handled (with aborted status)
                    if let Err(e) = self
                        .mark_trigger_event_handled(execution_id, "aborted")
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

                    record_workflow_terminal_action(
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
                        .set_execution_waiting(execution_id, current_step, &condition)
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
                            record_workflow_terminal_action(
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

                    // Simple wait implementation - in practice would use WaitConditionExecutor
                    match &condition {
                        WaitCondition::PaneIdle {
                            idle_threshold_ms, ..
                        } => {
                            sleep(Duration::from_millis(*idle_threshold_ms)).await;
                        }
                        WaitCondition::Pattern { .. } => {
                            // Would use WaitConditionExecutor here
                            sleep(timeout).await;
                        }
                        WaitCondition::StableTail { stable_for_ms, .. } => {
                            // Would use WaitConditionExecutor here
                            sleep(Duration::from_millis(*stable_for_ms)).await;
                        }
                        WaitCondition::TextMatch { .. } => {
                            // Would use WaitConditionExecutor here
                            sleep(timeout).await;
                        }
                        WaitCondition::Sleep { duration_ms } => {
                            sleep(Duration::from_millis(*duration_ms)).await;
                        }
                        WaitCondition::External { .. } => {
                            // Would wait for external signal
                            sleep(timeout).await;
                        }
                    }

                    // Continue to next step after wait
                    current_step += 1;
                    retries = 0;

                    // Update execution back to running
                    if let Err(e) = self.update_execution_step(execution_id, current_step).await {
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
                            record_workflow_terminal_action(
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

                    let send_result = {
                        let mut guard = self.injector.lock().await;
                        guard
                            .send_text(
                                pane_id,
                                &text,
                                crate::policy::ActorKind::Workflow,
                                ctx.capabilities(),
                                Some(execution_id),
                            )
                            .await
                    };

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

                                // Simple wait implementation
                                match &condition {
                                    WaitCondition::PaneIdle {
                                        idle_threshold_ms, ..
                                    } => {
                                        sleep(Duration::from_millis(*idle_threshold_ms)).await;
                                    }
                                    WaitCondition::Pattern { .. } => {
                                        sleep(timeout).await;
                                    }
                                    WaitCondition::StableTail { stable_for_ms, .. } => {
                                        sleep(Duration::from_millis(*stable_for_ms)).await;
                                    }
                                    WaitCondition::TextMatch { .. } => {
                                        sleep(timeout).await;
                                    }
                                    WaitCondition::Sleep { duration_ms } => {
                                        sleep(Duration::from_millis(*duration_ms)).await;
                                    }
                                    WaitCondition::External { .. } => {
                                        sleep(timeout).await;
                                    }
                                }
                            }

                            // Continue to next step
                            current_step += 1;
                            retries = 0;

                            if let Err(e) =
                                self.update_execution_step(execution_id, current_step).await
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
                                    record_workflow_terminal_action(
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
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with denied status)
                            if let Err(e) = self
                                .mark_trigger_event_handled(execution_id, "denied")
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

                            record_workflow_terminal_action(
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
                            let abort_reason =
                                format!("Text injection requires approval (code: {code})");

                            tracing::warn!(
                                pane_id,
                                execution_id,
                                code = %code,
                                "Text injection requires approval"
                            );

                            // Update execution to failed (approval not auto-granted for workflows)
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Cleanup and release lock
                            workflow.cleanup(&mut ctx).await;
                            self.lock_manager.release(pane_id, execution_id);

                            record_workflow_terminal_action(
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
                            if let Err(e) = self.fail_execution(execution_id, &abort_reason).await {
                                tracing::warn!(
                                    execution_id,
                                    error = %e,
                                    "Failed to fail execution"
                                );
                            }

                            // Mark trigger event as handled (with error status)
                            if let Err(e) =
                                self.mark_trigger_event_handled(execution_id, "error").await
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

                            record_workflow_terminal_action(
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
            .complete_execution(execution_id, Some(result.clone()))
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
            .mark_trigger_event_handled(execution_id, "completed")
            .await
        {
            tracing::warn!(
                execution_id,
                error = %e,
                "Failed to mark trigger event as handled"
            );
        }

        self.lock_manager.release(pane_id, execution_id);

        record_workflow_terminal_action(
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
                                        injector: Arc::clone(&self.injector),
                                        config,
                                        replay_capture: self.replay_capture.clone(),
                                    };

                                    crate::runtime_compat::task::spawn(async move {
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
                                    });
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

            // Compute next step from logs
            let step_logs = match self.storage.get_step_logs(&record.id).await {
                Ok(logs) => logs,
                Err(e) => {
                    tracing::warn!(
                        execution_id = %record.id,
                        error = %e,
                        "Failed to get step logs for resume"
                    );
                    continue;
                }
            };

            let next_step = compute_next_step(&step_logs);

            // Try to re-acquire lock
            let lock_result =
                self.lock_manager
                    .try_acquire(record.pane_id, &record.workflow_name, &record.id);

            match lock_result {
                LockAcquisitionResult::AlreadyLocked { .. } => {
                    tracing::warn!(
                        execution_id = %record.id,
                        pane_id = record.pane_id,
                        "Cannot resume: pane locked"
                    );
                    continue;
                }
                LockAcquisitionResult::Acquired => {}
            }

            tracing::info!(
                execution_id = %record.id,
                workflow = %record.workflow_name,
                pane_id = record.pane_id,
                resume_step = next_step,
                "Resuming workflow"
            );

            let result = self
                .run_workflow(record.pane_id, workflow, &record.id, next_step)
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
}
