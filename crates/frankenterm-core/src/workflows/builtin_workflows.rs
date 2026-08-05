//! Built-in workflow implementations: HandleCompaction and HandleUsageLimits.
//!
//! Core workflows for automated context compaction recovery and usage-limit
//! failover with account rotation and session resumption.
//!
//! Extracted from `workflows.rs` as part of strangler fig refactoring (ft-c45am).

#[allow(clippy::wildcard_imports)]
use super::*;
use chrono::{Datelike as _, TimeZone as _, Timelike as _};

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
        let token = format!("{{{{{key}}}}}"); // ubs:ignore - placeholder token syntax, not a secret.
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

fn verified_submit_step_result(workflow_name: &str, submit: WorkflowVerifiedSubmit) -> StepResult {
    match submit.receipt.state {
        crate::robot_types::SubmitReceiptState::Submitted
        | crate::robot_types::SubmitReceiptState::QueuedBehindOperation => StepResult::cont(),
        state => {
            let evidence = if submit.receipt.evidence_rule_ids.is_empty() {
                "none".to_string()
            } else {
                submit.receipt.evidence_rule_ids.join(",")
            };
            StepResult::abort(format!(
                "{workflow_name}: verified submit failed: state={}, idempotency_key={}, evidence={}",
                state.as_str(),
                submit.receipt.idempotency_key,
                evidence
            ))
        }
    }
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
        let deadline = Self::stabilization_deadline(start, timeout_ms)?;
        let mut interval = Duration::from_millis(50);
        let mut polls = 0usize;

        let stable_for_ms_i64 = i64::try_from(stable_for_ms).unwrap_or(i64::MAX);

        loop {
            polls = polls.saturating_add(1);

            let last_activity_ms = storage
                .pane_last_output_at(pane_id)
                .await
                .map_err(|e| format!("Failed to read pane activity: {e}"))?;

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

            // Never let the exponential polling delay itself overshoot the
            // caller's deadline by as much as the one-second maximum interval.
            // Recheck the actual clock after waking: scheduler delay can carry
            // even a shorter requested sleep beyond the deadline. Report the
            // timeout immediately instead of issuing one more storage query
            // after the deadline has already elapsed.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let sleep_for = interval.min(remaining);
            sleep(sleep_for).await;
            if Instant::now() >= deadline {
                return Err(format!(
                    "Stabilization timeout after {}ms (last_activity_ms={:?}, stable_for_ms={})",
                    elapsed_ms(start),
                    last_activity_ms,
                    stable_for_ms
                ));
            }
            interval = interval.saturating_mul(2);
            if interval > Duration::from_secs(1) {
                interval = Duration::from_secs(1);
            }
        }
    }

    fn stabilization_deadline(start: Instant, timeout_ms: u64) -> Result<Instant, String> {
        Self::stabilization_deadline_after(start, Duration::from_millis(timeout_ms))
    }

    fn stabilization_deadline_after(start: Instant, timeout: Duration) -> Result<Instant, String> {
        start
            .checked_add(timeout)
            .ok_or_else(|| format!("Stabilization timeout is too large: {timeout:?}"))
    }
}

impl Workflow for HandleCompaction {
    fn name(&self) -> &'static str {
        "handle_compaction"
    }

    fn description(&self) -> &'static str {
        "Re-inject critical context (AGENTS.md) after conversation compaction"
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["session.compaction"]
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
        let ctx_for_verified_send = if step_idx == 2 {
            Some(ctx.clone())
        } else {
            None
        };

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

                    let mut submit_ctx =
                        ctx_for_verified_send.expect("step 2 captures workflow context");
                    match submit_ctx.send_verified(&prompt).await {
                        Ok(submit) => verified_submit_step_result("handle_compaction", submit),
                        Err(reason) => StepResult::abort(reason),
                    }
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
    crate::runtime_async::Mutex<crate::rate_limit_tracker::RateLimitTracker>,
> = LazyLock::new(|| {
    crate::runtime_async::Mutex::new(crate::rate_limit_tracker::RateLimitTracker::new())
});

const UNKNOWN_LIMIT_ACCOUNT_ID: &str = "unknown";
const CONSERVATIVE_UNKNOWN_LIMIT_WINDOW_TTL_MS: i64 = 5 * 60 * 1000;

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

pub(super) fn trigger_is_limit_window_event(trigger: &serde_json::Value) -> bool {
    if let Some(event) = trigger.get("event_type").and_then(|v| v.as_str()) {
        return matches!(
            event,
            "usage.reached" | "rate_limit.detected" | "usage_limit"
        );
    }

    trigger
        .get("rule_id")
        .and_then(|v| v.as_str())
        .is_some_and(|rule_id| {
            rule_id.contains("usage.reached")
                || rule_id.contains("usage_limit")
                || rule_id.contains("rate_limit")
        })
}

fn trigger_service(trigger: &serde_json::Value) -> String {
    trigger_string_field(trigger, &["service", "provider"])
        .or_else(|| trigger_extracted_string(trigger, &["service", "provider"]))
        .unwrap_or_else(|| match trigger_agent_type(trigger) {
            crate::patterns::AgentType::Codex => "openai".to_string(),
            crate::patterns::AgentType::ClaudeCode => "anthropic".to_string(),
            crate::patterns::AgentType::Gemini => "google".to_string(),
            _ => "unknown".to_string(),
        })
}

fn trigger_account_id(trigger: &serde_json::Value) -> Option<String> {
    trigger_extracted_string(trigger, &["account_id", "account", "account_name"])
        .or_else(|| trigger_string_field(trigger, &["account_id", "account", "account_name"]))
        .filter(|value| !value.trim().is_empty())
}

fn trigger_retry_after(trigger: &serde_json::Value) -> Option<String> {
    trigger_extracted_string(trigger, &["retry_after"])
        .or_else(|| trigger_string_field(trigger, &["retry_after"]))
}

fn trigger_extracted_string(trigger: &serde_json::Value, keys: &[&str]) -> Option<String> {
    trigger
        .get("extracted")
        .and_then(|extracted| trigger_string_field(extracted, keys))
}

fn trigger_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = value.get(*key)?;
        match value {
            serde_json::Value::String(raw) => Some(raw.trim().to_string()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
        .filter(|text| !text.is_empty())
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LimitWindowReset {
    reset_at: Option<i64>,
    reset_source: &'static str,
    reset_text: Option<String>,
    conservative_ttl_ms: i64,
}

fn limit_window_reset_from_trigger(
    trigger: &serde_json::Value,
    detected_at_ms: i64,
) -> LimitWindowReset {
    let absolute_text =
        trigger_extracted_string(trigger, &["reset_at", "reset_time", "reset_until"])
            .or_else(|| trigger_string_field(trigger, &["reset_at", "reset_time", "reset_until"]));
    if let Some(text) = absolute_text {
        if let Some((reset_source, reset_at)) = parse_reset_deadline_ms(&text, detected_at_ms) {
            return LimitWindowReset {
                reset_at: Some(reset_at),
                reset_source,
                reset_text: Some(text),
                conservative_ttl_ms: 0,
            };
        }
        return LimitWindowReset {
            reset_at: None,
            reset_source: "unknown_ttl",
            reset_text: Some(text),
            conservative_ttl_ms: CONSERVATIVE_UNKNOWN_LIMIT_WINDOW_TTL_MS,
        };
    }

    if let Some(text) = trigger_retry_after(trigger) {
        if let Some(duration) = crate::rate_limit_tracker::parse_retry_after_duration(&text) {
            let delta_ms = i64::try_from(duration.as_millis()).unwrap_or(i64::MAX);
            return LimitWindowReset {
                reset_at: detected_at_ms.checked_add(delta_ms),
                reset_source: "retry_after",
                reset_text: Some(text),
                conservative_ttl_ms: 0,
            };
        }
        return LimitWindowReset {
            reset_at: None,
            reset_source: "unknown_ttl",
            reset_text: Some(text),
            conservative_ttl_ms: CONSERVATIVE_UNKNOWN_LIMIT_WINDOW_TTL_MS,
        };
    }

    LimitWindowReset {
        reset_at: None,
        reset_source: "unknown_ttl",
        reset_text: None,
        conservative_ttl_ms: CONSERVATIVE_UNKNOWN_LIMIT_WINDOW_TTL_MS,
    }
}

fn parse_absolute_reset_ms(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(epoch) = trimmed.parse::<i64>() {
        if epoch < 946_684_800 {
            return None;
        }
        return if epoch >= 10_000_000_000 {
            Some(epoch)
        } else {
            epoch.checked_mul(1000)
        };
    }

    if let Some(ms) = chrono::DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|dt| dt.timestamp_millis())
    {
        return Some(ms);
    }

    // Space-separated absolute datetimes carrying an explicit UTC marker, e.g.
    // the canonical usage-limit fixture "2026-01-20 12:34 UTC" (and the "…Z" /
    // " GMT" variants). Only an explicit UTC marker is honored: a zone-less
    // wall clock is deliberately NOT treated as absolute, so it degrades to a
    // conservative unknown TTL rather than silently mis-scheduling against the
    // wrong timezone.
    parse_utc_datetime_ms(trimmed)
}

/// Parse a full `YYYY-MM-DD[ T]HH:MM[:SS]` datetime that carries an explicit
/// UTC marker (`Z`, ` UTC`, or ` GMT`) into epoch milliseconds. Returns `None`
/// when no UTC marker is present or the body is not a recognized datetime.
fn parse_utc_datetime_ms(text: &str) -> Option<i64> {
    let body = strip_utc_zone_marker(text)?.trim();
    const FORMATS: [&str; 4] = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ];
    FORMATS.iter().find_map(|format| {
        chrono::NaiveDateTime::parse_from_str(body, format)
            .ok()
            .map(|naive| {
                chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc)
                    .timestamp_millis()
            })
    })
}

/// Strip a trailing explicit-UTC marker (` UTC`, ` GMT`, or a `Z`/`z` suffix
/// that immediately follows a digit) and return the datetime body. Returns
/// `None` when no explicit UTC marker is present. UTF-8 safe: suffix slicing
/// only happens on verified char boundaries.
fn strip_utc_zone_marker(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    let cut = trimmed.len().checked_sub(4)?;
    if let Some(tail) = trimmed.get(cut..) {
        if tail.eq_ignore_ascii_case(" UTC") || tail.eq_ignore_ascii_case(" GMT") {
            return trimmed.get(..cut).map(str::trim_end);
        }
    }
    if let Some(stripped) = trimmed.strip_suffix(['Z', 'z']) {
        if stripped
            .chars()
            .last()
            .is_some_and(|ch| ch.is_ascii_digit())
        {
            return Some(stripped);
        }
    }
    None
}

fn parse_reset_deadline_ms(text: &str, detected_at_ms: i64) -> Option<(&'static str, i64)> {
    if let Some(reset_at) = parse_absolute_reset_ms(text) {
        return Some(("absolute", reset_at));
    }

    if let Some(duration) = crate::rate_limit_tracker::parse_retry_after_duration(text) {
        let delta_ms = i64::try_from(duration.as_millis()).ok()?;
        return detected_at_ms
            .checked_add(delta_ms)
            .map(|reset_at| ("retry_after", reset_at));
    }

    parse_time_of_day_reset_ms(text, detected_at_ms).map(|reset_at| ("absolute", reset_at))
}

fn parse_time_of_day_reset_ms(text: &str, detected_at_ms: i64) -> Option<i64> {
    let parsed = ParsedResetTimeOfDay::parse(text)?;
    match parsed.zone.as_deref() {
        Some("UTC" | "Etc/UTC" | "Z") => {
            parse_utc_time_of_day_reset_ms(&parsed.time_text, detected_at_ms)
        }
        Some("America/New_York" | "US/Eastern" | "EST" | "EDT") => {
            parse_new_york_time_of_day_reset_ms(&parsed.time_text, detected_at_ms)
        }
        Some(_) => None,
        None => parse_local_time_of_day_reset_ms(&parsed.time_text, detected_at_ms),
    }
}

struct ParsedResetTimeOfDay {
    time_text: String,
    zone: Option<String>,
}

impl ParsedResetTimeOfDay {
    fn parse(text: &str) -> Option<Self> {
        let mut value = text.trim().trim_end_matches('.').trim().to_string();
        if value.is_empty() {
            return None;
        }

        let mut zone = None;
        if let Some(open_idx) = value.rfind('(') {
            if value.ends_with(')') {
                let zone_text = value[open_idx + 1..value.len() - 1].trim();
                if !zone_text.is_empty() {
                    zone = Some(zone_text.to_string());
                    value.truncate(open_idx);
                    value = value.trim().to_string();
                }
            }
        }

        let uppercase = value.to_ascii_uppercase();
        if uppercase.ends_with(" UTC") {
            zone = Some("UTC".to_string());
            value.truncate(value.len().saturating_sub(4));
            value = value.trim().to_string();
        }

        normalize_time_of_day_text(&value).map(|time_text| Self { time_text, zone })
    }
}

fn normalize_time_of_day_text(text: &str) -> Option<String> {
    let compact = text.trim();
    if compact.is_empty() {
        return None;
    }

    let lowercase = compact.to_ascii_lowercase();
    for suffix in ["am", "pm"] {
        if lowercase.ends_with(suffix) {
            let split_at = compact.len().checked_sub(suffix.len())?;
            let time = compact[..split_at].trim();
            if time.is_empty() {
                return None;
            }
            return normalize_meridiem_time_text(time, &suffix.to_ascii_uppercase());
        }
    }

    Some(compact.to_string())
}

fn normalize_meridiem_time_text(time: &str, suffix: &str) -> Option<String> {
    let (hour_text, minute_text) = time.split_once(':').map_or_else(
        || (time.trim(), None),
        |(hour, minute)| (hour.trim(), Some(minute.trim())),
    );
    let hour = hour_text.parse::<u32>().ok()?;
    if !(1..=12).contains(&hour) {
        return None;
    }

    match minute_text {
        Some(minute_text) => {
            let minute = minute_text.parse::<u32>().ok()?;
            if minute > 59 {
                return None;
            }
            Some(format!("{hour:02}:{minute:02} {suffix}"))
        }
        None => Some(format!("{hour:02} {suffix}")),
    }
}

fn parse_naive_time_of_day(text: &str) -> Option<chrono::NaiveTime> {
    parse_meridiem_time_of_day(text).or_else(|| {
        std::iter::once(&"%H:%M")
            .find_map(|format| chrono::NaiveTime::parse_from_str(text, format).ok())
    })
}

fn parse_meridiem_time_of_day(text: &str) -> Option<chrono::NaiveTime> {
    let mut parts = text.split_whitespace();
    let time_text = parts.next()?;
    let suffix = parts.next()?.to_ascii_uppercase();
    if parts.next().is_some() {
        return None;
    }
    let (hour_text, minute_text) = time_text
        .split_once(':')
        .map_or((time_text, "0"), |(hour, minute)| (hour, minute));
    let hour = hour_text.parse::<u32>().ok()?;
    let minute = minute_text.parse::<u32>().ok()?;
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour_24 = match suffix.as_str() {
        "AM" => {
            if hour == 12 {
                0
            } else {
                hour
            }
        }
        "PM" => {
            if hour == 12 {
                12
            } else {
                hour + 12
            }
        }
        _ => return None,
    };
    chrono::NaiveTime::from_hms_opt(hour_24, minute, 0)
}

fn parse_local_time_of_day_reset_ms(text: &str, detected_at_ms: i64) -> Option<i64> {
    let time = parse_naive_time_of_day(text)?;
    let detected_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(detected_at_ms)?;
    let detected_local = detected_utc.with_timezone(&chrono::Local);
    let date = detected_local.date_naive();
    let candidate_naive = date.and_time(time);
    let mut candidate = chrono::Local
        .from_local_datetime(&candidate_naive)
        .earliest()
        .or_else(|| chrono::Local.from_local_datetime(&candidate_naive).latest())?;
    if candidate.timestamp_millis() <= detected_at_ms {
        let next_naive = candidate_naive.checked_add_days(chrono::Days::new(1))?;
        candidate = chrono::Local
            .from_local_datetime(&next_naive)
            .earliest()
            .or_else(|| chrono::Local.from_local_datetime(&next_naive).latest())?;
    }
    Some(candidate.timestamp_millis())
}

fn parse_utc_time_of_day_reset_ms(text: &str, detected_at_ms: i64) -> Option<i64> {
    let time = parse_naive_time_of_day(text)?;
    let detected = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(detected_at_ms)?;
    let date = detected.date_naive();
    let mut candidate = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        date.and_time(time),
        chrono::Utc,
    );
    if candidate.timestamp_millis() <= detected_at_ms {
        let next_date = date.checked_add_days(chrono::Days::new(1))?;
        candidate = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            next_date.and_time(time),
            chrono::Utc,
        );
    }
    Some(candidate.timestamp_millis())
}

fn parse_new_york_time_of_day_reset_ms(text: &str, detected_at_ms: i64) -> Option<i64> {
    let time = parse_naive_time_of_day(text)?;
    let date = new_york_date_for_utc_ms(detected_at_ms)?;
    let candidate = new_york_local_datetime_ms(date, time);
    if candidate > detected_at_ms {
        return Some(candidate);
    }
    let next_date = date.checked_add_days(chrono::Days::new(1))?;
    Some(new_york_local_datetime_ms(next_date, time))
}

fn new_york_date_for_utc_ms(detected_at_ms: i64) -> Option<chrono::NaiveDate> {
    let detected_utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(detected_at_ms)?;
    let provisional = detected_utc - chrono::Duration::hours(5);
    let offset = new_york_utc_offset_seconds_for_local(
        provisional.year(),
        provisional.month(),
        provisional.day(),
        provisional.hour(),
    );
    Some((detected_utc + chrono::Duration::seconds(i64::from(offset))).date_naive())
}

fn new_york_local_datetime_ms(date: chrono::NaiveDate, time: chrono::NaiveTime) -> i64 {
    let offset =
        new_york_utc_offset_seconds_for_local(date.year(), date.month(), date.day(), time.hour());
    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
        date.and_time(time) - chrono::Duration::seconds(i64::from(offset)),
        chrono::Utc,
    )
    .timestamp_millis()
}

fn new_york_utc_offset_seconds_for_local(year: i32, month: u32, day: u32, hour: u32) -> i32 {
    match month {
        4..=10 => -4 * 60 * 60,
        1 | 2 | 12 => -5 * 60 * 60,
        3 => {
            let transition_day = nth_weekday_of_month(year, 3, chrono::Weekday::Sun, 2);
            if day > transition_day || (day == transition_day && hour >= 2) {
                -4 * 60 * 60
            } else {
                -5 * 60 * 60
            }
        }
        11 => {
            let transition_day = nth_weekday_of_month(year, 11, chrono::Weekday::Sun, 1);
            if day < transition_day || (day == transition_day && hour < 2) {
                -4 * 60 * 60
            } else {
                -5 * 60 * 60
            }
        }
        _ => -5 * 60 * 60,
    }
}

fn nth_weekday_of_month(year: i32, month: u32, weekday: chrono::Weekday, nth: u32) -> u32 {
    let mut found = 0;
    for day in 1..=31 {
        let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, day) else {
            break;
        };
        if date.weekday() == weekday {
            found += 1;
            if found == nth {
                return day;
            }
        }
    }
    1
}

pub(super) async fn record_limit_window_for_trigger(
    storage: &crate::storage::StorageHandle,
    pane_id: u64,
    trigger: &serde_json::Value,
    detected_at_ms: i64,
    source: &'static str,
) -> crate::error::Result<crate::storage::LimitWindowRecord> {
    let cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
    record_limit_window_for_trigger_with_cx(storage, &cx, pane_id, trigger, detected_at_ms, source)
        .await
}

pub(super) async fn record_limit_window_for_trigger_with_cx(
    storage: &crate::storage::StorageHandle,
    cx: &crate::cx::Cx,
    pane_id: u64,
    trigger: &serde_json::Value,
    detected_at_ms: i64,
    source: &'static str,
) -> crate::error::Result<crate::storage::LimitWindowRecord> {
    let service = trigger_service(trigger);
    let account_id = trigger_account_id(trigger);
    let account = match account_id.as_deref() {
        Some(account_id) => {
            storage
                .get_account_with_cx(cx, &service, account_id)
                .await?
        }
        None => None,
    };
    let account_key = account_id
        .clone()
        .or_else(|| account.as_ref().map(|record| record.account_id.clone()))
        .unwrap_or_else(|| UNKNOWN_LIMIT_ACCOUNT_ID.to_string());
    let reset = limit_window_reset_from_trigger(trigger, detected_at_ms);
    let rule_id = trigger
        .get("rule_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let event_type = trigger
        .get("event_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let agent_type = trigger
        .get("agent_type")
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let extracted = trigger
        .get("extracted")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let account_known = account.is_some();

    storage
        .upsert_limit_window_with_cx(
            cx,
            crate::storage::LimitWindowRecord {
                id: 0,
                pane_id,
                service,
                account_id: account_key,
                account_db_id: account.as_ref().map(|record| record.id),
                account_known,
                agent_type,
                rule_id,
                event_type,
                limited_at: detected_at_ms,
                reset_at: reset.reset_at,
                reset_source: reset.reset_source.to_string(),
                reset_text: reset.reset_text.clone(),
                conservative_ttl_ms: reset.conservative_ttl_ms,
                last_seen_at: detected_at_ms,
                seen_count: 1,
                metadata: Some(
                    serde_json::json!({
                        "source": "workflow.limit_window_ledger",
                        "workflow": source,
                        "account_known": account_known,
                        "reset_source": reset.reset_source,
                        "reset_text": reset.reset_text,
                        "extracted": extracted,
                    })
                    .to_string(),
                ),
                created_at: detected_at_ms,
                updated_at: detected_at_ms,
            },
        )
        .await
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
        if detection.agent_type != crate::patterns::AgentType::Codex {
            return false;
        }

        matches!(
            detection.event_type.as_str(),
            "usage.reached" | "rate_limit.detected" | "usage_limit"
        ) || matches!(
            detection.rule_id.as_str(),
            "codex.usage_limit" | "codex.usage.reached" | "codex.rate_limit.detected"
        )
    }

    fn trigger_event_types(&self) -> &'static [&'static str] {
        &["usage.reached", "rate_limit.detected", "usage_limit"]
    }

    fn supported_agent_types(&self) -> &'static [&'static str] {
        &["codex"]
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

                    if trigger_is_limit_window_event(&trigger) {
                        if let Err(err) = record_limit_window_for_trigger(
                            &storage,
                            pane_id,
                            &trigger,
                            now,
                            "handle_usage_limits",
                        )
                        .await
                        {
                            tracing::warn!(
                                pane_id,
                                error = %err,
                                "handle_usage_limits: failed to upsert limit window"
                            );
                        }
                    }

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
                                // Account available — proceed with failover.
                                // br-ft-zkthg: bump workflows serde-drop
                                // counter on serialization failures rather
                                // than silently substituting Value::Null
                                // in the audit chain.
                                let mut json = match serde_json::to_value(&selection) {
                                    Ok(v) => v,
                                    Err(err) => {
                                        super::record_workflows_serde_drop();
                                        tracing::warn!(
                                            target: "ft.workflows.serde",
                                            error = %err,
                                            "account selection serialization failed; recording Value::Null"
                                        );
                                        serde_json::Value::Null
                                    }
                                };
                                if let Some(summary) = rate_limit_summary {
                                    if let Some(obj) = json.as_object_mut() {
                                        let summary_value = match serde_json::to_value(summary) {
                                            Ok(v) => v,
                                            Err(err) => {
                                                super::record_workflows_serde_drop();
                                                tracing::warn!(
                                                    target: "ft.workflows.serde",
                                                    error = %err,
                                                    "rate-limit summary serialization failed; recording Value::Null"
                                                );
                                                serde_json::Value::Null
                                            }
                                        };
                                        obj.insert(
                                            "provider_rate_limit_status".to_string(),
                                            summary_value,
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
                                            // br-ft-zkthg: bump workflows
                                            // serde-drop counter rather than
                                            // silently substituting Null.
                                            let summary_value = match serde_json::to_value(summary)
                                            {
                                                Ok(v) => v,
                                                Err(err) => {
                                                    super::record_workflows_serde_drop();
                                                    tracing::warn!(
                                                        target: "ft.workflows.serde",
                                                        error = %err,
                                                        "rate-limit summary serialization failed; recording Value::Null"
                                                    );
                                                    serde_json::Value::Null
                                                }
                                            };
                                            obj.insert(
                                                "provider_rate_limit_status".to_string(),
                                                summary_value,
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

    #[test]
    fn handle_compaction_rejects_unrepresentable_stabilization_timeout() {
        let err = HandleCompaction::stabilization_deadline_after(Instant::now(), Duration::MAX)
            .expect_err("Duration::MAX should not fit in an Instant deadline");
        assert!(err.contains("too large"));
    }

    #[test]
    fn v35_compaction_stabilization_preserves_target_pane_activity_semantics() {
        use crate::runtime_async::CompatRuntime;

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("workflow stabilization test runtime");
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("stabilization-activity.db");
        let db_path_str = db_path.to_string_lossy().to_string();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let storage = std::sync::Arc::new(
                    crate::storage::StorageHandle::new(&db_path_str)
                        .await
                        .expect("storage"),
                );
                let now = crate::storage::now_ms();
                storage
                    .upsert_pane(crate::storage::PaneRecord {
                        pane_id: 42,
                        pane_uuid: Some("pane-42".to_string()),
                        domain: "local".to_string(),
                        window_id: None,
                        tab_id: None,
                        title: Some("codex".to_string()),
                        cwd: None,
                        tty_name: None,
                        first_seen_at: now,
                        last_seen_at: now,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    })
                    .await
                    .expect("seed pane");

                let empty = HandleCompaction::wait_for_stable_output(
                    storage.clone(),
                    42,
                    1,
                    0,
                )
                .await
                .expect("an empty target pane is immediately stable");
                assert_eq!(empty.polls, 1);
                assert_eq!(empty.last_activity_ms, None);

                let segment = storage
                    .append_segment(42, "recent activity", None)
                    .await
                    .expect("append target-pane activity");
                let error = HandleCompaction::wait_for_stable_output(
                    storage.clone(),
                    42,
                    u64::MAX,
                    0,
                )
                .await
                .expect_err("recent target-pane activity must honor the zero timeout");
                assert!(error.contains("Stabilization timeout"));
                assert!(error.contains(&segment.captured_at.to_string()));

                storage.shutdown().await.expect("shutdown storage");
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
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

    fn workflow_submit_with_state(
        state: crate::robot_types::SubmitReceiptState,
    ) -> WorkflowVerifiedSubmit {
        WorkflowVerifiedSubmit {
            injection: crate::policy::InjectionResult::Allowed {
                decision: crate::policy::PolicyDecision::allow_with_rule("policy.allow"),
                summary: "send_text".to_string(),
                pane_id: 42,
                action: crate::policy::ActionKind::SendText,
                audit_action_id: Some(7),
            },
            receipt: crate::robot_types::SubmitReceipt {
                state,
                guarantee_level: crate::robot_types::SubmitGuaranteeLevel::Submitted,
                guarantee_met: matches!(
                    state,
                    crate::robot_types::SubmitReceiptState::Submitted
                        | crate::robot_types::SubmitReceiptState::QueuedBehindOperation
                ),
                agent_type: Some("codex".to_string()),
                profile_id: Some("codex.default".to_string()),
                profile_version: Some("2026-06-08".to_string()),
                attempts: 1,
                evidence_rule_ids: vec![format!("submit_profile:codex.default:{}", state.as_str())],
                elapsed_ms: 12,
                polls: 1,
                cursor_before: Some("pane:42:capture:sha256:before".to_string()),
                cursor_after: Some("pane:42:capture:sha256:after".to_string()),
                idempotency_key: "rk:test-submit".to_string(),
            },
            verification_report: None,
        }
    }

    #[test]
    fn verified_submit_step_result_continues_for_delivered_states() {
        assert!(
            verified_submit_step_result(
                "handle_compaction",
                workflow_submit_with_state(crate::robot_types::SubmitReceiptState::Submitted),
            )
            .is_continue()
        );
        assert!(
            verified_submit_step_result(
                "handle_compaction",
                workflow_submit_with_state(
                    crate::robot_types::SubmitReceiptState::QueuedBehindOperation,
                ),
            )
            .is_continue()
        );
    }

    #[test]
    fn verified_submit_step_result_aborts_stuck_composer_with_typed_state() {
        let result = verified_submit_step_result(
            "handle_compaction",
            workflow_submit_with_state(crate::robot_types::SubmitReceiptState::StuckInComposer),
        );

        assert!(
            matches!(result, StepResult::Abort { .. }),
            "expected typed abort for stuck composer"
        );
        if let StepResult::Abort { reason } = result {
            assert!(reason.contains("handle_compaction"));
            assert!(reason.contains("stuck_in_composer"));
            assert!(reason.contains("rk:test-submit"));
            assert!(reason.contains("submit_profile:codex.default:stuck_in_composer"));
        }
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
    fn handle_compaction_reports_session_compaction_trigger_event() {
        let wf = HandleCompaction::new();
        assert_eq!(wf.trigger_event_types(), ["session.compaction"]);
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
    fn handle_usage_limits_reports_expected_trigger_event_types() {
        let wf = HandleUsageLimits::new();
        assert_eq!(
            wf.trigger_event_types(),
            ["usage.reached", "rate_limit.detected", "usage_limit"]
        );
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

    #[test]
    fn handle_usage_limits_rejects_session_token_usage_summary() {
        let wf = HandleUsageLimits::new();
        let detection = crate::patterns::Detection {
            rule_id: "codex.session.token_usage".to_string(),
            event_type: "session.summary".to_string(),
            severity: crate::patterns::Severity::Info,
            agent_type: crate::patterns::AgentType::Codex,
            matched_text: "Token usage: total=1234".to_string(),
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
        let trigger =
            serde_json::json!({"event_type": "session.compaction", "rule_id": "compaction"});
        assert!(!trigger_is_rate_limit(&trigger));
    }

    #[test]
    fn trigger_is_rate_limit_empty() {
        let trigger = serde_json::json!({});
        assert!(!trigger_is_rate_limit(&trigger));
    }

    #[test]
    fn trigger_is_limit_window_event_treats_event_type_as_authoritative() {
        let warning = serde_json::json!({
            "event_type": "usage.warning",
            "rule_id": "gemini.usage.reached"
        });
        assert!(
            !trigger_is_limit_window_event(&warning),
            "usage.warning must not create a hard limit window even if a rule id is misleading"
        );

        let legacy_rule_only = serde_json::json!({"rule_id": "claude_code.rate_limit.detected"});
        assert!(trigger_is_limit_window_event(&legacy_rule_only));
    }

    #[test]
    fn limit_window_event_rejects_generic_event_even_if_rule_matches() {
        let trigger = serde_json::json!({
            "event_type": "pattern.detected",
            "rule_id": "codex.rate_limit.detected"
        });
        assert!(!trigger_is_limit_window_event(&trigger));
    }

    #[test]
    fn limit_window_event_rejects_unrelated_event_and_rule() {
        let trigger = serde_json::json!({
            "event_type": "session.compaction",
            "rule_id": "codex.session.compaction"
        });
        assert!(!trigger_is_limit_window_event(&trigger));
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
    fn trigger_retry_after_accepts_top_level_value() {
        let trigger = serde_json::json!({"retry_after": "45 seconds"});
        assert_eq!(
            trigger_retry_after(&trigger),
            Some("45 seconds".to_string())
        );
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

    #[test]
    fn trigger_retry_after_accepts_numeric_extracted_value() {
        let trigger = serde_json::json!({
            "extracted": {"retry_after": 45}
        });
        assert_eq!(trigger_retry_after(&trigger), Some("45".to_string()));
    }

    #[test]
    fn limit_window_reset_parses_epoch_and_rfc3339() {
        assert_eq!(
            parse_absolute_reset_ms("1800000000"),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            parse_absolute_reset_ms("1800000000000"),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            parse_absolute_reset_ms("2026-01-01T00:00:00Z"),
            Some(1_767_225_600_000)
        );
        assert_eq!(
            parse_absolute_reset_ms("30"),
            None,
            "small numeric reset text is not an epoch timestamp"
        );
    }

    #[test]
    fn parse_absolute_reset_ms_parses_canonical_space_utc_fixture() {
        // The canonical usage-limit fixture string
        // ("…try again at 2026-01-20 12:34 UTC.") must resolve to an absolute
        // deadline, not degrade to a conservative unknown TTL.
        let expected = chrono::DateTime::parse_from_rfc3339("2026-01-20T12:34:00Z")
            .expect("valid timestamp")
            .timestamp_millis();
        assert_eq!(
            parse_absolute_reset_ms("2026-01-20 12:34 UTC"),
            Some(expected)
        );
        assert_eq!(
            parse_absolute_reset_ms("2026-01-20 12:34:56 UTC"),
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-01-20T12:34:56Z")
                    .expect("valid timestamp")
                    .timestamp_millis()
            )
        );
        // `Z` and ` gmt` (case-insensitive) UTC markers parse identically.
        assert_eq!(parse_absolute_reset_ms("2026-01-20T12:34Z"), Some(expected));
        assert_eq!(
            parse_absolute_reset_ms("2026-01-20 12:34 gmt"),
            Some(expected)
        );
    }

    #[test]
    fn parse_absolute_reset_ms_rejects_zoneless_wall_clock() {
        // Without an explicit UTC marker a full datetime is NOT treated as
        // absolute — it must degrade (caller falls back to unknown TTL) rather
        // than mis-schedule against the wrong timezone.
        assert_eq!(parse_absolute_reset_ms("2026-01-20 12:34"), None);
        assert_eq!(parse_absolute_reset_ms("2026-01-20T12:34"), None);
    }

    #[test]
    fn limit_window_reset_parses_canonical_usage_limit_fixture() {
        // Mirrors the patterns.rs codex.usage.reached fixture
        // ("…try again at 2026-01-20 12:34 UTC.").
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"reset_time": "2026-01-20 12:34 UTC"}
        });
        let reset = limit_window_reset_from_trigger(&trigger, 1_700_000_000_000);
        assert_eq!(reset.reset_source, "absolute");
        assert_eq!(
            reset.reset_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-01-20T12:34:00Z")
                    .expect("valid timestamp")
                    .timestamp_millis()
            )
        );
        assert_eq!(reset.conservative_ttl_ms, 0);
        assert_eq!(reset.reset_text.as_deref(), Some("2026-01-20 12:34 UTC"));
    }

    #[test]
    fn limit_window_reset_prefers_absolute_reset() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {
                "reset_at": "2026-01-01T00:00:00Z",
                "retry_after": "5 minutes"
            }
        });
        let reset = limit_window_reset_from_trigger(&trigger, 1_800_000_000_000);
        assert_eq!(reset.reset_source, "absolute");
        assert_eq!(reset.reset_at, Some(1_767_225_600_000));
        assert_eq!(reset.reset_text.as_deref(), Some("2026-01-01T00:00:00Z"));
        assert_eq!(reset.conservative_ttl_ms, 0);
    }

    #[test]
    fn limit_window_reset_uses_retry_after_duration() {
        let trigger = serde_json::json!({
            "event_type": "rate_limit.detected",
            "extracted": {"retry_after": "2 minutes"}
        });
        let reset = limit_window_reset_from_trigger(&trigger, 1_800_000_000_000);
        assert_eq!(reset.reset_source, "retry_after");
        assert_eq!(reset.reset_at, Some(1_800_000_120_000));
        assert_eq!(reset.conservative_ttl_ms, 0);
    }

    #[test]
    fn limit_window_reset_parses_duration_like_reset_time() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"reset_time": "30 seconds"}
        });
        let reset = limit_window_reset_from_trigger(&trigger, 1_800_000_000_000);
        assert_eq!(reset.reset_source, "retry_after");
        assert_eq!(reset.reset_at, Some(1_800_000_030_000));
        assert_eq!(reset.conservative_ttl_ms, 0);
    }

    #[test]
    fn limit_window_reset_parses_utc_time_of_day_into_future_deadline() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"reset_time": "10:30 AM UTC"}
        });
        let detected_at = 1_800_000_000_000;
        let reset = limit_window_reset_from_trigger(&trigger, detected_at);
        assert_eq!(reset.reset_source, "absolute");
        assert!(
            reset.reset_at.expect("time-of-day reset should parse") > detected_at,
            "time-of-day reset must resolve to a future deadline"
        );
        assert_eq!(reset.conservative_ttl_ms, 0);
    }

    #[test]
    fn limit_window_reset_parses_new_york_wall_clock_fixture() {
        let detected_at = chrono::DateTime::parse_from_rfc3339("2026-06-08T16:00:00Z")
            .expect("valid timestamp")
            .timestamp_millis();
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"reset_time": "2pm (America/New_York)"}
        });
        let reset = limit_window_reset_from_trigger(&trigger, detected_at);
        assert_eq!(reset.reset_source, "absolute");
        assert_eq!(
            reset.reset_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-08T18:00:00Z")
                    .expect("valid timestamp")
                    .timestamp_millis()
            )
        );
    }

    #[test]
    fn limit_window_reset_degrades_unparseable_to_unknown_ttl() {
        let trigger = serde_json::json!({
            "event_type": "usage.reached",
            "extracted": {"reset_time": "after lunch maybe"}
        });
        let reset = limit_window_reset_from_trigger(&trigger, 1_800_000_000_000);
        assert_eq!(reset.reset_source, "unknown_ttl");
        assert!(reset.reset_at.is_none());
        assert_eq!(
            reset.conservative_ttl_ms,
            CONSERVATIVE_UNKNOWN_LIMIT_WINDOW_TTL_MS
        );
        assert_eq!(reset.reset_text.as_deref(), Some("after lunch maybe"));
    }

    #[test]
    fn handle_usage_limits_step0_writes_limit_window_for_fired_rule() {
        use crate::runtime_async::CompatRuntime;

        let runtime = crate::runtime_async::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("workflow test runtime");
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let db_path = temp_dir.path().join("usage-limit-window.db");
        let db_path_str = db_path.to_string_lossy().to_string();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async {
                let storage = std::sync::Arc::new(
                    crate::storage::StorageHandle::new(&db_path_str)
                        .await
                        .expect("storage"),
                );
                let now = crate::storage::now_ms();
                storage
                    .upsert_pane(crate::storage::PaneRecord {
                        pane_id: 88,
                        pane_uuid: Some("pane-88".to_string()),
                        domain: "local".to_string(),
                        window_id: None,
                        tab_id: None,
                        title: Some("codex".to_string()),
                        cwd: None,
                        tty_name: None,
                        first_seen_at: now,
                        last_seen_at: now,
                        observed: true,
                        ignore_reason: None,
                        last_decision_at: None,
                    })
                    .await
                    .expect("seed pane");
                let account_id = "acct-ledger";
                let account_db_id = storage
                    .upsert_account(crate::accounts::AccountRecord {
                        id: 0,
                        account_id: account_id.to_string(),
                        service: "openai".to_string(),
                        name: Some("ledger account".to_string()),
                        percent_remaining: 0.0,
                        reset_at: None,
                        tokens_used: None,
                        tokens_remaining: None,
                        tokens_limit: None,
                        last_refreshed_at: now,
                        last_used_at: None,
                        created_at: now,
                        updated_at: now,
                    })
                    .await
                    .expect("seed account");
                let trigger = serde_json::json!({
                    "agent_type": "codex",
                    "event_type": "rate_limit.detected",
                    "rule_id": "codex.rate_limit.detected",
                    "extracted": {
                        "account_id": account_id,
                        "retry_after": "30 seconds"
                    }
                });
                let mut ctx = WorkflowContext::new(
                    storage.clone(),
                    88,
                    crate::policy::PaneCapabilities::default(),
                    "exec-limit-window",
                )
                .with_trigger(trigger);
                let step = HandleUsageLimits::new().execute_step(&mut ctx, 0).await;
                assert!(step.is_continue(), "step 0 should continue, got {step:?}");

                let row = storage
                    .get_limit_window(88, "openai", account_id)
                    .await
                    .expect("query limit window")
                    .expect("limit window should be written");
                assert_eq!(row.account_db_id, Some(account_db_id));
                assert!(row.account_known);
                assert_eq!(row.account_id, account_id);
                assert_eq!(row.service, "openai");
                assert_eq!(row.rule_id, "codex.rate_limit.detected");
                assert_eq!(row.event_type, "rate_limit.detected");
                assert_eq!(row.reset_source, "retry_after");
                assert_eq!(row.reset_at, row.limited_at.checked_add(30_000));
                assert_eq!(row.seen_count, 1);

                storage.shutdown().await.expect("shutdown storage");
            });
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(runtime)));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_async::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
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
