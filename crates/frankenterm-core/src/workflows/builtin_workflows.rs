//! Built-in workflow implementations: HandleCompaction and HandleUsageLimits.
//!
//! Core workflows for automated context compaction recovery and usage-limit
//! failover with account rotation and session resumption.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;

// ============================================================================
// Built-in Workflows
// ============================================================================

/// Agent-specific prompts for context refresh after compaction.
///
/// These prompts are carefully crafted to be:
/// - Minimal in length (to avoid adding too much to already-compacted context)
/// - Clear in intent (agent should re-read key project files)
/// - Agent-specific (matching each agent's communication style)
pub mod compaction_prompts {
    /// Prompt for Claude Code agents.
    pub const CLAUDE_CODE: &str = crate::config::DEFAULT_COMPACTION_PROMPT_CLAUDE_CODE;

    /// Prompt for Codex CLI agents.
    pub const CODEX: &str = crate::config::DEFAULT_COMPACTION_PROMPT_CODEX;

    /// Prompt for Gemini CLI agents.
    pub const GEMINI: &str = crate::config::DEFAULT_COMPACTION_PROMPT_GEMINI;

    /// Default prompt for unknown agents.
    pub const UNKNOWN: &str = crate::config::DEFAULT_COMPACTION_PROMPT_UNKNOWN;
}

#[derive(Debug, Clone)]
struct PromptRenderContext {
    pane_id: u64,
    agent_type: crate::patterns::AgentType,
    pane_domain: Option<String>,
    pane_title: Option<String>,
    pane_cwd: Option<String>,
}

impl PromptRenderContext {
    fn from_context(ctx: &WorkflowContext) -> Self {
        let agent_type = HandleCompaction::agent_type_from_trigger(ctx);
        let meta = ctx.pane_meta();
        Self {
            pane_id: ctx.pane_id(),
            agent_type,
            pane_domain: meta.domain.clone(),
            pane_title: meta.title.clone(),
            pane_cwd: meta.cwd.clone(),
        }
    }
}

fn render_compaction_prompt(
    template: &str,
    ctx: &PromptRenderContext,
    config: &crate::config::CompactionPromptConfig,
) -> String {
    let redactor = Redactor::new();
    let max_prompt_len = config.max_prompt_len as usize;
    let max_snippet_len = config.max_snippet_len as usize;

    let mut rendered = template.to_string();
    let replacements = [
        ("agent_type", ctx.agent_type.to_string()),
        ("pane_id", ctx.pane_id.to_string()),
        ("pane_domain", ctx.pane_domain.clone().unwrap_or_default()),
        ("pane_title", ctx.pane_title.clone().unwrap_or_default()),
        ("pane_cwd", ctx.pane_cwd.clone().unwrap_or_default()),
    ];

    for (key, value) in replacements {
        let token = format!("{{{{{key}}}}}");
        if rendered.contains(&token) {
            let redacted = redactor.redact(&value);
            let clipped = truncate_to_len(&redacted, max_snippet_len);
            rendered = rendered.replace(&token, &clipped);
        }
    }

    let redacted = redactor.redact(&rendered);
    truncate_to_len(&redacted, max_prompt_len)
}

fn truncate_to_len(value: &str, max_len: usize) -> String {
    if value.chars().count() <= max_len {
        return value.to_string();
    }

    value.chars().take(max_len).collect()
}

#[derive(Debug)]
struct StabilizationOutcome {
    waited_ms: u64,
    polls: usize,
    last_activity_ms: Option<i64>,
}

/// Handle compaction workflow: re-inject critical context after conversation compaction.
///
/// This workflow is triggered when an AI agent compacts or summarizes its context window.
/// After compaction, the agent may have lost important project context, so we prompt
/// the agent to re-read key files like AGENTS.md.
///
/// # Steps
///
/// 1. **Acquire lock**: Get per-pane workflow lock to prevent concurrent workflows.
/// 2. **Validate state**: Check that pane is not in alt-screen mode and has no recent gap.
/// 3. **Confirm anchor**: Re-read pane tail to verify compaction anchor is still present.
/// 4. **Stabilize**: Wait for pane to be idle (2s default) before sending.
/// 5. **Send prompt**: Inject agent-specific context refresh prompt.
/// 6. **Verify**: Wait for response pattern or timeout.
///
/// # Safety
///
/// - All sends are policy-gated (may be denied by PolicyEngine).
/// - Workflow is idempotent: dedupe/cooldown prevents spam on repeated detections.
/// - Guards abort workflow if pane state is unsuitable for injection.
///
/// # Example Detection
///
/// ```text
/// rule_id: "claude_code.compaction"
/// event_type: "session.compaction"
/// matched_text: "Auto-compact: compacted 150,000 tokens to 25,000 tokens"
/// ```
pub struct HandleCompaction {
    /// Default stabilization wait time in milliseconds.
    pub stabilization_ms: u64,
    /// Timeout for the idle wait condition.
    pub idle_timeout_ms: u64,
    /// Prompt templates and bounds for compaction prompts.
    pub prompt_config: crate::config::CompactionPromptConfig,
}

impl Default for HandleCompaction {
    fn default() -> Self {
        Self {
            stabilization_ms: 2000,
            idle_timeout_ms: 10_000,
            prompt_config: crate::config::CompactionPromptConfig::default(),
        }
    }
}

impl HandleCompaction {
    /// Create a new HandleCompaction workflow with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create with custom stabilization time.
    #[must_use]
    pub fn with_stabilization_ms(mut self, ms: u64) -> Self {
        self.stabilization_ms = ms;
        self
    }

    /// Create with custom idle timeout.
    #[must_use]
    pub fn with_idle_timeout_ms(mut self, ms: u64) -> Self {
        self.idle_timeout_ms = ms;
        self
    }

    /// Create with custom compaction prompt configuration.
    #[must_use]
    pub fn with_prompt_config(
        mut self,
        prompt_config: crate::config::CompactionPromptConfig,
    ) -> Self {
        self.prompt_config = prompt_config;
        self
    }

    /// Get the agent-specific prompt based on agent type from trigger detection.
    pub fn resolve_prompt(&self, ctx: &WorkflowContext) -> String {
        let render_ctx = PromptRenderContext::from_context(ctx);
        let template = self.select_prompt_template(&render_ctx);
        render_compaction_prompt(template, &render_ctx, &self.prompt_config)
    }

    fn select_prompt_template<'a>(&'a self, ctx: &PromptRenderContext) -> &'a str {
        if let Some(prompt) = self.prompt_config.by_pane.get(&ctx.pane_id) {
            return prompt;
        }

        let domain = ctx.pane_domain.as_deref().unwrap_or_default();
        let title = ctx.pane_title.as_deref().unwrap_or_default();
        let cwd = ctx.pane_cwd.as_deref().unwrap_or_default();
        for override_item in &self.prompt_config.by_project {
            if override_item.rule.matches(domain, title, cwd) {
                return &override_item.prompt;
            }
        }

        let agent_key = ctx.agent_type.to_string();
        if let Some(prompt) = self.prompt_config.by_agent.get(&agent_key) {
            return prompt;
        }

        &self.prompt_config.default
    }

    /// Extract agent type from trigger context, if available.
    fn agent_type_from_trigger(ctx: &WorkflowContext) -> crate::patterns::AgentType {
        ctx.trigger()
            .and_then(|t| t.get("agent_type"))
            .and_then(|v| v.as_str())
            .map_or(crate::patterns::AgentType::Unknown, |s| match s {
                "claude_code" => crate::patterns::AgentType::ClaudeCode,
                "codex" => crate::patterns::AgentType::Codex,
                "gemini" => crate::patterns::AgentType::Gemini,
                _ => crate::patterns::AgentType::Unknown,
            })
    }

    /// Check if pane state allows workflow execution.
    ///
    /// Guards against:
    /// - Alt-screen mode (vim, less, etc.)
    /// - Recent output gap (unknown pane state)
    /// - Command currently running
    pub fn check_pane_guards(ctx: &WorkflowContext) -> Result<(), String> {
        let caps = ctx.capabilities();

        // Guard: alt-screen blocks sends (Some(true) = definitely in alt-screen)
        if caps.alt_screen == Some(true) {
            return Err("Pane is in alt-screen mode (vim, less, etc.) - aborting".to_string());
        }

        // Guard: command running could cause issues
        if caps.command_running {
            return Err("Command is currently running in pane - aborting".to_string());
        }

        // Guard: recent gap suggests unknown state
        if caps.has_recent_gap {
            return Err("Recent output gap detected - pane state uncertain".to_string());
        }

        Ok(())
    }

    /// Wait until output has been stable for the requested window.
    ///
    /// Uses captured output activity timestamps from storage to avoid
    /// reading from the pane directly. This is a best-effort stabilization
    /// strategy until deterministic compaction-complete markers are wired in.
    async fn wait_for_stable_output(
        storage: Arc<StorageHandle>,
        pane_id: u64,
        stable_for_ms: u64,
        timeout_ms: u64,
    ) -> Result<StabilizationOutcome, String> {
        if stable_for_ms == 0 {
            return Ok(StabilizationOutcome {
                waited_ms: 0,
                polls: 0,
                last_activity_ms: None,
            });
        }

        let start = Instant::now();
        let deadline = start + Duration::from_millis(timeout_ms);
        let mut interval = Duration::from_millis(50);
        let mut polls = 0usize;

        let stable_for_ms_i64 = i64::try_from(stable_for_ms).unwrap_or(i64::MAX);

        loop {
            polls += 1;

            let activity_map = storage
                .get_last_activity_by_pane()
                .await
                .map_err(|e| format!("Failed to read pane activity: {e}"))?;

            let last_activity_ms = activity_map.get(&pane_id).copied();

            // If we have no activity recorded, treat as stable enough to proceed.
            if last_activity_ms.is_none() {
                return Ok(StabilizationOutcome {
                    waited_ms: elapsed_ms(start),
                    polls,
                    last_activity_ms,
                });
            }

            let now = now_ms();
            let since_ms = now.saturating_sub(last_activity_ms.unwrap_or(now));
            if since_ms >= stable_for_ms_i64 {
                return Ok(StabilizationOutcome {
                    waited_ms: elapsed_ms(start),
                    polls,
                    last_activity_ms,
                });
            }

            if Instant::now() >= deadline {
                return Err(format!(
                    "Stabilization timeout after {}ms (last_activity_ms={:?}, stable_for_ms={})",
                    elapsed_ms(start),
                    last_activity_ms,
                    stable_for_ms
                ));
            }

            sleep(interval).await;
            interval = interval.saturating_mul(2);
            if interval > Duration::from_secs(1) {
                interval = Duration::from_secs(1);
            }
        }
    }
}

impl Workflow for HandleCompaction {
    fn name(&self) -> &'static str {
        "handle_compaction"
    }

    fn description(&self) -> &'static str {
        "Re-inject critical context (AGENTS.md) after conversation compaction"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        // Handle any compaction-related detection
        detection.event_type == "session.compaction" || detection.rule_id.contains("compaction")
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows injection"),
            WorkflowStep::new("stabilize", "Wait for compaction output to stabilize"),
            WorkflowStep::new("send_prompt", "Send agent-specific context refresh prompt"),
            WorkflowStep::new("verify_send", "Verify the prompt was processed"),
        ]
    }

    fn to_action_plan(
        &self,
        ctx: &WorkflowContext,
        execution_id: &str,
    ) -> Option<crate::plan::ActionPlan> {
        let pane_id = ctx.pane_id();
        let workspace_id = ctx.workspace_id().unwrap_or("default");
        let prompt = self.resolve_prompt(ctx);

        let check_guards = crate::plan::StepPlan::new(
            1,
            crate::plan::StepAction::Custom {
                action_type: "check_guards".to_string(),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                }),
            },
            "Validate pane state allows injection",
        );

        let stabilize = crate::plan::StepPlan::new(
            2,
            crate::plan::StepAction::Custom {
                action_type: "stabilize_output".to_string(),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                    "stable_for_ms": self.stabilization_ms,
                    "timeout_ms": self.idle_timeout_ms,
                }),
            },
            "Wait for compaction output to stabilize",
        );

        let send_prompt = crate::plan::StepPlan::new(
            3,
            crate::plan::StepAction::SendText {
                pane_id,
                text: prompt,
                paste_mode: None,
            },
            "Send agent-specific context refresh prompt",
        )
        .idempotent();

        let verify_send = crate::plan::StepPlan::new(
            4,
            crate::plan::StepAction::Custom {
                action_type: "verify_send".to_string(),
                payload: serde_json::json!({
                    "pane_id": pane_id,
                }),
            },
            "Verify the prompt was processed",
        );

        Some(
            crate::plan::ActionPlan::builder(self.description(), workspace_id)
                .add_steps([check_guards, stabilize, send_prompt, verify_send])
                .metadata(serde_json::json!({
                    "workflow_name": self.name(),
                    "execution_id": execution_id,
                    "pane_id": pane_id,
                }))
                .created_at(now_ms())
                .build(),
        )
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        // Capture all values needed in the async block BEFORE entering it.
        // This avoids lifetime issues since we own the captured values.
        let stabilization_ms = self.stabilization_ms;
        let idle_timeout_ms = self.idle_timeout_ms;
        let pane_id = ctx.pane_id();
        let execution_id = ctx.execution_id().to_string();
        let storage = Arc::clone(ctx.storage());

        // For step 0: capture guard check result
        let guard_check_result = if step_idx == 0 {
            Some(Self::check_pane_guards(ctx))
        } else {
            None
        };

        // For step 2: capture prompt and injector availability
        let prompt = if step_idx == 2 {
            Some(self.resolve_prompt(ctx))
        } else {
            None
        };
        let has_injector = ctx.has_injector();

        // For step 3: capture trigger info
        let (tokens_before, tokens_after) = if step_idx == 3 {
            let before = ctx
                .trigger()
                .and_then(|t| t.get("extracted"))
                .and_then(|e| e.get("tokens_before"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let after = ctx
                .trigger()
                .and_then(|t| t.get("extracted"))
                .and_then(|e| e.get("tokens_after"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            (before, after)
        } else {
            (String::new(), String::new())
        };

        Box::pin(async move {
            match step_idx {
                // Step 0: Check guards - validate pane state
                0 => {
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_compaction: checking pane guards"
                    );

                    if let Some(Err(reason)) = guard_check_result {
                        tracing::warn!(
                            pane_id,
                            reason = %reason,
                            "handle_compaction: guard check failed"
                        );
                        return StepResult::abort(reason);
                    }

                    tracing::debug!(
                        pane_id,
                        "handle_compaction: guards passed, proceeding to stabilization"
                    );
                    StepResult::cont()
                }

                // Step 1: Stabilize - wait for pane to be idle
                1 => {
                    tracing::info!(
                        pane_id,
                        stabilization_ms,
                        idle_timeout_ms,
                        "handle_compaction: waiting for output to stabilize"
                    );

                    match Self::wait_for_stable_output(
                        storage.clone(),
                        pane_id,
                        stabilization_ms,
                        idle_timeout_ms,
                    )
                    .await
                    {
                        Ok(outcome) => {
                            tracing::info!(
                                pane_id,
                                waited_ms = outcome.waited_ms,
                                polls = outcome.polls,
                                last_activity_ms = ?outcome.last_activity_ms,
                                "handle_compaction: output stabilized"
                            );
                            StepResult::cont()
                        }
                        Err(reason) => {
                            tracing::warn!(pane_id, reason = %reason, "handle_compaction: stabilization failed");
                            StepResult::abort(reason)
                        }
                    }
                }

                // Step 2: Send agent-specific prompt
                // The runner will handle the actual text injection via policy-gated injector.
                2 => {
                    let prompt = prompt.unwrap_or_else(|| compaction_prompts::UNKNOWN.to_string());

                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        prompt_len = prompt.len(),
                        "handle_compaction: sending context refresh prompt"
                    );

                    // Check if injector is available
                    if !has_injector {
                        tracing::error!(pane_id, "handle_compaction: no injector configured");
                        return StepResult::abort("No injector configured for text injection");
                    }

                    // Use SendText to request the runner inject the prompt.
                    // The runner will call the policy-gated injector and abort if denied.
                    StepResult::send_text(prompt)
                }

                // Step 3: Verify the send (best-effort)
                3 => {
                    // For now, we consider the workflow done after the send step.
                    // Future: wait for OSC 133 prompt boundary or agent response pattern.
                    tracing::info!(
                        pane_id,
                        execution_id = %execution_id,
                        "handle_compaction: workflow completed successfully"
                    );

                    StepResult::done(serde_json::json!({
                        "status": "completed",
                        "pane_id": pane_id,
                        "tokens_before": tokens_before,
                        "tokens_after": tokens_after,
                        "action": "sent_context_refresh_prompt"
                    }))
                }

                _ => {
                    tracing::error!(
                        pane_id,
                        step_idx,
                        "handle_compaction: unexpected step index"
                    );
                    StepResult::abort(format!("Unexpected step index: {step_idx}"))
                }
            }
        })
    }

    fn cleanup(&self, _ctx: &mut WorkflowContext) -> BoxFuture<'_, ()> {
        // Note: We don't use ctx here because the async block would need to capture
        // values from ctx, which has a different lifetime. For a simple cleanup,
        // we just log that cleanup was called.
        Box::pin(async move {
            tracing::debug!("handle_compaction: cleanup completed");
        })
    }
}

/// Handle usage limits workflow: exit agent, persist session, and select new account.
static RATE_LIMIT_TRACKER: LazyLock<
    crate::runtime_compat::Mutex<crate::rate_limit_tracker::RateLimitTracker>,
> = LazyLock::new(|| {
    crate::runtime_compat::Mutex::new(crate::rate_limit_tracker::RateLimitTracker::new())
});

fn trigger_agent_type(trigger: &serde_json::Value) -> crate::patterns::AgentType {
    match trigger
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
    {
        "codex" => crate::patterns::AgentType::Codex,
        "claude_code" => crate::patterns::AgentType::ClaudeCode,
        "gemini" => crate::patterns::AgentType::Gemini,
        "wezterm" => crate::patterns::AgentType::Wezterm,
        _ => crate::patterns::AgentType::Unknown,
    }
}

fn trigger_is_rate_limit(trigger: &serde_json::Value) -> bool {
    trigger
        .get("event_type")
        .and_then(|v| v.as_str())
        .is_some_and(|event| event == "rate_limit.detected")
        || trigger
            .get("rule_id")
            .and_then(|v| v.as_str())
            .is_some_and(|rule_id| rule_id.contains("rate_limit"))
}

fn trigger_retry_after(trigger: &serde_json::Value) -> Option<String> {
    trigger
        .get("extracted")
        .and_then(|v| v.get("retry_after"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

#[derive(Default)]
pub struct HandleUsageLimits;

impl HandleUsageLimits {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Workflow for HandleUsageLimits {
    fn name(&self) -> &'static str {
        "handle_usage_limits"
    }

    fn description(&self) -> &'static str {
        "Exit agent, persist session summary, and select new account for failover"
    }

    fn handles(&self, detection: &crate::patterns::Detection) -> bool {
        detection.agent_type == crate::patterns::AgentType::Codex
            && (detection.rule_id.contains("usage")
                || detection.event_type == "rate_limit.detected"
                || detection.rule_id.contains("rate_limit"))
    }

    fn steps(&self) -> Vec<WorkflowStep> {
        vec![
            WorkflowStep::new("check_guards", "Validate pane state allows interaction"),
            WorkflowStep::new("exit_and_persist", "Exit Codex and persist session summary"),
            WorkflowStep::new("select_account", "Select best available account"),
        ]
    }

    fn execute_step(
        &self,
        ctx: &mut WorkflowContext,
        step_idx: usize,
    ) -> BoxFuture<'_, StepResult> {
        let pane_id = ctx.pane_id();
        let storage = ctx.storage().clone();
        let ctx_clone = ctx.clone();

        Box::pin(async move {
            match step_idx {
                0 => {
                    // Best-effort usage-limit metric (do not fail the workflow on storage errors).
                    let trigger = ctx_clone
                        .trigger()
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let now = now_ms();
                    let agent_type = trigger
                        .get("agent_type")
                        .and_then(|v| v.as_str())
                        .map(ToString::to_string);
                    let rule_id = trigger.get("rule_id").and_then(|v| v.as_str());
                    let extracted = trigger
                        .get("extracted")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let is_rate_limit = trigger_is_rate_limit(&trigger);

                    if let Err(err) = storage
                        .record_usage_metric(crate::storage::UsageMetricRecord {
                            id: 0,
                            timestamp: now,
                            metric_type: crate::storage::MetricType::RateLimitHit,
                            pane_id: Some(pane_id),
                            agent_type,
                            account_id: None,
                            workflow_id: None,
                            count: Some(1),
                            amount: None,
                            tokens: None,
                            metadata: Some(
                                serde_json::json!({
                                    "source": "workflow.handle_usage_limits",
                                    "rule_id": rule_id,
                                    "extracted": extracted,
                                })
                                .to_string(),
                            ),
                            created_at: now,
                        })
                        .await
                    {
                        tracing::warn!(
                            pane_id,
                            error = %err,
                            "handle_usage_limits: failed to record rate limit metric"
                        );
                    }

                    if is_rate_limit {
                        let tracker_agent_type = trigger_agent_type(&trigger);
                        if tracker_agent_type != crate::patterns::AgentType::Unknown {
                            let retry_after = trigger_retry_after(&trigger);
                            let mut tracker = RATE_LIMIT_TRACKER.lock().await;
                            tracker.record(
                                pane_id,
                                tracker_agent_type,
                                rule_id.unwrap_or("unknown").to_string(),
                                retry_after,
                            );
                            tracker.gc();
                            let summary = tracker.provider_status(tracker_agent_type);
                            tracing::info!(
                                pane_id,
                                agent_type = %summary.agent_type,
                                status = ?summary.status,
                                limited_panes = summary.limited_pane_count,
                                total_panes = summary.total_pane_count,
                                earliest_clear_secs = summary.earliest_clear_secs,
                                "handle_usage_limits: updated provider rate-limit tracker"
                            );
                        }
                    }

                    let caps = ctx_clone.capabilities();
                    if caps.alt_screen == Some(true) {
                        return StepResult::abort("Pane is in alt-screen mode");
                    }
                    if caps.command_running {
                        return StepResult::abort("Command is running");
                    }
                    StepResult::cont()
                }
                1 => {
                    let wezterm = default_wezterm_handle();
                    let source = WeztermHandleSource::new(Arc::clone(&wezterm));
                    let options = CodexExitOptions::default();

                    let outcome = codex_exit_and_wait_for_summary(
                        pane_id,
                        &source,
                        || {
                            let mut c = ctx_clone.clone();
                            async move { c.send_ctrl_c().await.map_err(ToString::to_string) }
                        },
                        &options,
                    )
                    .await;

                    match outcome {
                        Ok(_) => {
                            let text = match wezterm.get_text(pane_id, false).await {
                                Ok(t) => t,
                                Err(e) => {
                                    return StepResult::abort(format!("Failed to get text: {e}"));
                                }
                            };
                            let tail = crate::wezterm::tail_text(&text, 200);

                            match parse_codex_session_summary(&tail) {
                                Ok(parsed) => {
                                    if let Err(e) =
                                        persist_codex_session_summary(&storage, pane_id, &parsed)
                                            .await
                                    {
                                        tracing::warn!("Failed to persist session summary: {e}");
                                    }
                                    StepResult::cont()
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse session summary: {e}");
                                    StepResult::cont()
                                }
                            }
                        }
                        Err(e) => StepResult::abort(format!("Failed to exit Codex: {e}")),
                    }
                }
                2 => {
                    let trigger = ctx_clone
                        .trigger()
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let rate_limit_summary = if trigger_is_rate_limit(&trigger) {
                        let tracker_agent_type = trigger_agent_type(&trigger);
                        if tracker_agent_type == crate::patterns::AgentType::Unknown {
                            None
                        } else {
                            let mut tracker = RATE_LIMIT_TRACKER.lock().await;
                            tracker.gc();
                            Some(tracker.provider_status(tracker_agent_type))
                        }
                    } else {
                        None
                    };

                    let caut_client = crate::caut::CautClient::new();
                    let config = crate::accounts::AccountSelectionConfig::default();
                    let result = refresh_and_select_account(&caut_client, &storage, &config).await;

                    match result {
                        Ok(selection) => {
                            if selection.selected.is_some() {
                                if matches!(
                                    selection.quota_advisory.availability,
                                    crate::accounts::QuotaAvailability::Low
                                ) {
                                    tracing::warn!(
                                        pane_id,
                                        selected_percent = ?selection.quota_advisory.selected_percent_remaining,
                                        threshold_percent = selection.quota_advisory.low_quota_threshold_percent,
                                        warning = ?selection.quota_advisory.warning,
                                        "handle_usage_limits: selected account has low remaining quota"
                                    );
                                }
                                if let Some(summary) = rate_limit_summary.as_ref() {
                                    if matches!(
                                        summary.status,
                                        crate::rate_limit_tracker::ProviderRateLimitStatus::FullyLimited
                                    ) {
                                        tracing::warn!(
                                            pane_id,
                                            limited_panes = summary.limited_pane_count,
                                            total_panes = summary.total_pane_count,
                                            earliest_clear_secs = summary.earliest_clear_secs,
                                            "handle_usage_limits: selected account while provider remains fully rate-limited"
                                        );
                                    }
                                }
                                // Account available — proceed with failover
                                let mut json = serde_json::to_value(&selection).unwrap_or_default();
                                if let Some(summary) = rate_limit_summary {
                                    if let Some(obj) = json.as_object_mut() {
                                        obj.insert(
                                            "provider_rate_limit_status".to_string(),
                                            serde_json::to_value(summary).unwrap_or_default(),
                                        );
                                    }
                                }
                                StepResult::done(json)
                            } else {
                                // All accounts exhausted — enter safe fallback path (wa-4r7)
                                tracing::warn!(
                                    pane_id,
                                    total = selection.explanation.total_considered,
                                    "handle_usage_limits: all accounts exhausted, entering fallback"
                                );

                                // Fetch accounts for reset time calculation
                                let accounts = storage
                                    .get_accounts_by_service("openai")
                                    .await
                                    .unwrap_or_default();
                                let exhaustion = crate::accounts::build_exhaustion_info(
                                    &accounts,
                                    selection.explanation,
                                );
                                let tracker_retry_after_ms = rate_limit_summary
                                    .as_ref()
                                    .and_then(|summary| {
                                        i64::try_from(summary.earliest_clear_secs)
                                            .ok()
                                            .and_then(|secs| secs.checked_mul(1000))
                                    })
                                    .and_then(|delta_ms| now_ms().checked_add(delta_ms));

                                let plan = build_all_accounts_exhausted_plan(
                                    pane_id,
                                    exhaustion.accounts_checked,
                                    None, // resume_session_id not available at this step
                                    exhaustion.earliest_reset_ms.or(tracker_retry_after_ms),
                                    now_ms(),
                                );

                                tracing::info!(
                                    pane_id,
                                    accounts_checked = exhaustion.accounts_checked,
                                    earliest_reset_ms = ?exhaustion.earliest_reset_ms,
                                    earliest_reset_account = ?exhaustion.earliest_reset_account,
                                    "handle_usage_limits: built fallback plan"
                                );
                                let mut result = fallback_plan_to_step_result(&plan);
                                if let Some(summary) = rate_limit_summary {
                                    if let StepResult::Done { result: payload } = &mut result {
                                        if let Some(obj) = payload.as_object_mut() {
                                            obj.insert(
                                                "provider_rate_limit_status".to_string(),
                                                serde_json::to_value(summary).unwrap_or_default(),
                                            );
                                        }
                                    }
                                }
                                result
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                pane_id,
                                error = %e,
                                "handle_usage_limits: account selection failed"
                            );
                            StepResult::abort(e.to_string())
                        }
                    }
                }
                _ => StepResult::abort("Unexpected step"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // truncate_to_len
    // ========================================================================

    #[test]
    fn truncate_to_len_short_string() {
        assert_eq!(truncate_to_len("hello", 10), "hello");
    }

    #[test]
    fn truncate_to_len_exact_boundary() {
        assert_eq!(truncate_to_len("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_len_truncates() {
        assert_eq!(truncate_to_len("hello world", 5), "hello");
    }

    #[test]
    fn truncate_to_len_empty() {
        assert_eq!(truncate_to_len("", 10), "");
    }

    #[test]
    fn truncate_to_len_zero_max() {
        assert_eq!(truncate_to_len("hello", 0), "");
    }

    #[test]
    fn truncate_to_len_unicode_aware() {
        // Multi-byte chars — truncation should be char-count-based, not byte-based
        let s = "héllo wörld";
        let truncated = truncate_to_len(s, 5);
        assert_eq!(truncated.chars().count(), 5);
    }

    // ========================================================================
    // HandleCompaction construction
    // ========================================================================

    #[test]
    fn handle_compaction_default() {
        let wf = HandleCompaction::default();
        assert_eq!(wf.stabilization_ms, 2000);
        assert_eq!(wf.idle_timeout_ms, 10_000);
    }

    #[test]
    fn handle_compaction_new_equals_default() {
        let wf1 = HandleCompaction::new();
        let wf2 = HandleCompaction::default();
        assert_eq!(wf1.stabilization_ms, wf2.stabilization_ms);
        assert_eq!(wf1.idle_timeout_ms, wf2.idle_timeout_ms);
    }

    #[test]
    fn handle_compaction_builder_methods() {
        let wf = HandleCompaction::new()
            .with_stabilization_ms(5000)
            .with_idle_timeout_ms(30_000);
        assert_eq!(wf.stabilization_ms, 5000);
        assert_eq!(wf.idle_timeout_ms, 30_000);
    }

    // ========================================================================
    // HandleCompaction: Workflow trait metadata
    // ========================================================================

    #[test]
    fn handle_compaction_name() {
        let wf = HandleCompaction::new();
        assert_eq!(wf.name(), "handle_compaction");
    }

    #[test]
    fn handle_compaction_description_nonempty() {
        let wf = HandleCompaction::new();
        assert!(!wf.description().is_empty());
    }

    #[test]
    fn handle_compaction_has_four_steps() {
        let wf = HandleCompaction::new();
        let steps = wf.steps();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "stabilize");
        assert_eq!(steps[2].name, "send_prompt");
        assert_eq!(steps[3].name, "verify_send");
    }

    #[test]
    fn handle_compaction_handles_compaction_events() {
        let wf = HandleCompaction::new();
        let detection = crate::patterns::Detection {
            rule_id: "claude_code.compaction".to_string(),
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            agent_type: crate::patterns::AgentType::ClaudeCode,
            matched_text: "Auto-compact".to_string(),
            confidence: 1.0,
            extracted: serde_json::Value::Object(Default::default()),
            span: (0, 0),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_compaction_does_not_handle_unrelated() {
        let wf = HandleCompaction::new();
        let detection = crate::patterns::Detection {
            rule_id: "codex.usage_limit".to_string(),
            event_type: "rate_limit.detected".to_string(),
            severity: crate::patterns::Severity::Warning,
            agent_type: crate::patterns::AgentType::Codex,
            matched_text: "rate limit".to_string(),
            confidence: 1.0,
            extracted: serde_json::Value::Object(Default::default()),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    // ========================================================================
    // HandleUsageLimits: Workflow trait metadata
    // ========================================================================

    #[test]
    fn handle_usage_limits_name() {
        let wf = HandleUsageLimits::new();
        assert_eq!(wf.name(), "handle_usage_limits");
    }

    #[test]
    fn handle_usage_limits_description_nonempty() {
        let wf = HandleUsageLimits::new();
        assert!(!wf.description().is_empty());
    }

    #[test]
    fn handle_usage_limits_has_three_steps() {
        let wf = HandleUsageLimits::new();
        let steps = wf.steps();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].name, "check_guards");
        assert_eq!(steps[1].name, "exit_and_persist");
        assert_eq!(steps[2].name, "select_account");
    }

    #[test]
    fn handle_usage_limits_handles_codex_usage() {
        let wf = HandleUsageLimits::new();
        let detection = crate::patterns::Detection {
            rule_id: "codex.usage_limit".to_string(),
            event_type: "rate_limit.detected".to_string(),
            severity: crate::patterns::Severity::Warning,
            agent_type: crate::patterns::AgentType::Codex,
            matched_text: "usage limit".to_string(),
            confidence: 1.0,
            extracted: serde_json::Value::Object(Default::default()),
            span: (0, 0),
        };
        assert!(wf.handles(&detection));
    }

    #[test]
    fn handle_usage_limits_rejects_non_codex() {
        let wf = HandleUsageLimits::new();
        let detection = crate::patterns::Detection {
            rule_id: "claude_code.usage_limit".to_string(),
            event_type: "rate_limit.detected".to_string(),
            severity: crate::patterns::Severity::Warning,
            agent_type: crate::patterns::AgentType::ClaudeCode,
            matched_text: "usage limit".to_string(),
            confidence: 1.0,
            extracted: serde_json::Value::Object(Default::default()),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    #[test]
    fn handle_usage_limits_rejects_unrelated_rule() {
        let wf = HandleUsageLimits::new();
        let detection = crate::patterns::Detection {
            rule_id: "codex.compaction".to_string(),
            event_type: "session.compaction".to_string(),
            severity: crate::patterns::Severity::Info,
            agent_type: crate::patterns::AgentType::Codex,
            matched_text: "compaction".to_string(),
            confidence: 1.0,
            extracted: serde_json::Value::Object(Default::default()),
            span: (0, 0),
        };
        assert!(!wf.handles(&detection));
    }

    // ========================================================================
    // trigger_agent_type
    // ========================================================================

    #[test]
    fn trigger_agent_type_codex() {
        let trigger = serde_json::json!({"agent_type": "codex"});
        assert_eq!(
            trigger_agent_type(&trigger),
            crate::patterns::AgentType::Codex
        );
    }

    #[test]
    fn trigger_agent_type_claude_code() {
        let trigger = serde_json::json!({"agent_type": "claude_code"});
        assert_eq!(
            trigger_agent_type(&trigger),
            crate::patterns::AgentType::ClaudeCode
        );
    }

    #[test]
    fn trigger_agent_type_gemini() {
        let trigger = serde_json::json!({"agent_type": "gemini"});
        assert_eq!(
            trigger_agent_type(&trigger),
            crate::patterns::AgentType::Gemini
        );
    }

    #[test]
    fn trigger_agent_type_wezterm() {
        let trigger = serde_json::json!({"agent_type": "wezterm"});
        assert_eq!(
            trigger_agent_type(&trigger),
            crate::patterns::AgentType::Wezterm
        );
    }

    #[test]
    fn trigger_agent_type_unknown() {
        let trigger = serde_json::json!({"agent_type": "something_else"});
        assert_eq!(
            trigger_agent_type(&trigger),
            crate::patterns::AgentType::Unknown
        );
    }

    #[test]
    fn trigger_agent_type_missing() {
        let trigger = serde_json::json!({});
        assert_eq!(
            trigger_agent_type(&trigger),
            crate::patterns::AgentType::Unknown
        );
    }

    // ========================================================================
    // trigger_is_rate_limit
    // ========================================================================

    #[test]
    fn trigger_is_rate_limit_by_event_type() {
        let trigger = serde_json::json!({"event_type": "rate_limit.detected"});
        assert!(trigger_is_rate_limit(&trigger));
    }

    #[test]
    fn trigger_is_rate_limit_by_rule_id() {
        let trigger = serde_json::json!({"rule_id": "codex.rate_limit"});
        assert!(trigger_is_rate_limit(&trigger));
    }

    #[test]
    fn trigger_is_rate_limit_false() {
        let trigger = serde_json::json!({"event_type": "session.compaction", "rule_id": "compaction"});
        assert!(!trigger_is_rate_limit(&trigger));
    }

    #[test]
    fn trigger_is_rate_limit_empty() {
        let trigger = serde_json::json!({});
        assert!(!trigger_is_rate_limit(&trigger));
    }

    // ========================================================================
    // trigger_retry_after
    // ========================================================================

    #[test]
    fn trigger_retry_after_present() {
        let trigger = serde_json::json!({
            "extracted": {"retry_after": "30s"}
        });
        assert_eq!(trigger_retry_after(&trigger), Some("30s".to_string()));
    }

    #[test]
    fn trigger_retry_after_missing() {
        let trigger = serde_json::json!({});
        assert!(trigger_retry_after(&trigger).is_none());
    }

    #[test]
    fn trigger_retry_after_no_extracted() {
        let trigger = serde_json::json!({"event_type": "rate_limit"});
        assert!(trigger_retry_after(&trigger).is_none());
    }

    // ========================================================================
    // compaction_prompts constants
    // ========================================================================

    #[test]
    fn compaction_prompts_are_nonempty() {
        assert!(!compaction_prompts::CLAUDE_CODE.is_empty());
        assert!(!compaction_prompts::CODEX.is_empty());
        assert!(!compaction_prompts::GEMINI.is_empty());
        assert!(!compaction_prompts::UNKNOWN.is_empty());
    }
}
