//! Dry-run preview infrastructure
//!
//! Provides the foundational infrastructure to thread dry_run mode through all
//! command execution paths. When dry_run is enabled, commands perform all
//! validation and resolution but don't execute side effects.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::policy::{PolicyDecision, Redactor};

/// Build the shared Robot/MCP workflow preview without executing its steps.
/// Runtime pane-state and policy authorization are explicitly deferred; the
/// preview reports workflow eligibility and never grants permission to run.
#[must_use]
pub fn workflow_preview(
    command: &CommandContext,
    name: &str,
    pane: u64,
    pane_info: Option<&crate::wezterm::PaneInfo>,
    workflow: Option<&dyn crate::workflows::Workflow>,
    enabled: bool,
) -> DryRunReport {
    let mut ctx = command.dry_run_context();
    if let Some(info) = pane_info {
        let mut target =
            TargetResolution::new(pane, info.inferred_domain()).with_is_active(info.is_active);
        if let Some(title) = &info.title {
            target = target.with_title(title.clone());
        }
        if let Some(cwd) = &info.cwd {
            target = target.with_cwd(cwd.clone());
        }
        ctx.set_target(target);
    } else {
        ctx.set_target(TargetResolution::new(pane, "unknown"));
        ctx.add_warning("Pane metadata unavailable; verify pane ID and daemon state.");
    }
    let mut eval = PolicyEvaluation::new();
    if let Some(wf) = workflow {
        eval.add_check(PolicyCheck::passed(
            "workflow",
            format!("Workflow '{name}' loaded"),
        ));
        eval.add_check(if enabled {
            PolicyCheck::passed("workflow_enabled", "Workflow is enabled")
        } else {
            PolicyCheck::failed("workflow_enabled", "Workflow is disabled")
        });
        eval.add_check(if wf.requires_approval() {
            PolicyCheck::failed("approval", "Workflow requires approval")
        } else {
            PolicyCheck::passed("approval", "No approval required")
        });
        eval.add_check(if wf.is_destructive() {
            PolicyCheck::failed("destructive", "Workflow marked destructive")
        } else {
            PolicyCheck::passed("destructive", "Workflow marked non-destructive")
        });
    } else {
        eval.add_check(PolicyCheck::failed(
            "workflow",
            format!("Workflow '{name}' not found"),
        ));
    }
    eval.add_check(if pane_info.is_some() {
        PolicyCheck::passed("pane", "Pane found")
    } else {
        PolicyCheck::failed(
            "pane",
            "Pane not found (dry-run uses best-effort resolution)",
        )
    });
    eval.add_check(
        PolicyCheck::passed("pane_state", "Pane state not inspected during dry-run")
            .with_details("Verify prompt/alt-screen state before execution."),
    );
    eval.add_check(
        PolicyCheck::passed("policy_surface", "Policy surface: workflow")
            .with_details("action: workflow_run"),
    );
    eval.add_check(
        PolicyCheck::passed("policy", "Policy checks deferred to execution")
            .with_details("Send steps remain policy-gated at runtime."),
    );
    ctx.set_policy_evaluation(eval);
    if let Some(wf) = workflow {
        let mut step = 1;
        ctx.add_action(PlannedAction::new(
            step,
            ActionType::AcquireLock,
            format!("Acquire workflow lock for pane {pane}"),
        ));
        step += 1;
        for plan in wf.steps_to_plans(pane) {
            let action_type = step_action_to_dry_run_type(&plan.action);
            let mut description = plan.description.clone();
            if action_type == ActionType::SendText {
                description.push_str(" [policy-gated]");
            }
            ctx.add_action(
                PlannedAction::new(step, action_type, description)
                    .with_metadata(workflow_step_metadata(&plan)),
            );
            step += 1;
        }
        let event_types = wf.trigger_event_types();
        let rule_ids = wf.trigger_rule_ids();
        if !event_types.is_empty() || !rule_ids.is_empty() {
            let mut details = Vec::new();
            if !event_types.is_empty() {
                details.push(format!("event types: {}", event_types.join(", ")));
            }
            if !rule_ids.is_empty() {
                details.push(format!("rule ids: {}", rule_ids.join(", ")));
            }
            ctx.add_action(PlannedAction::new(
                step,
                ActionType::MarkEventHandled,
                format!("Mark triggering event handled ({})", details.join("; ")),
            ));
            step += 1;
        }
        ctx.add_action(PlannedAction::new(
            step,
            ActionType::ReleaseLock,
            "Release workflow lock".to_string(),
        ));
    } else {
        ctx.add_warning("No workflow steps available; check workflow name.");
    }
    ctx.take_report()
}

/// Map a structured workflow step to its preview action category.
#[must_use]
pub fn step_action_to_dry_run_type(action: &crate::plan::StepAction) -> ActionType {
    use crate::plan::StepAction;
    match action {
        StepAction::SendText { .. } => ActionType::SendText,
        StepAction::WaitFor { .. } => ActionType::WaitFor,
        StepAction::AcquireLock { .. } => ActionType::AcquireLock,
        StepAction::ReleaseLock { .. } => ActionType::ReleaseLock,
        StepAction::StoreData { .. } => ActionType::StoreData,
        StepAction::RunWorkflow { .. } | StepAction::NestedPlan { .. } => ActionType::WorkflowStep,
        StepAction::MarkEventHandled { .. } => ActionType::MarkEventHandled,
        StepAction::ValidateApproval { .. } => ActionType::ValidateApproval,
        StepAction::Custom { action_type, .. } => infer_action_type_from_name(action_type),
    }
}

/// Infer the category of an extension workflow's named action.
#[must_use]
pub fn infer_action_type_from_name(name: &str) -> ActionType {
    let lower = name.to_lowercase();
    if lower.contains("send") {
        ActionType::SendText
    } else if lower.contains("wait")
        || lower.contains("stabilize")
        || lower.contains("verify")
        || lower.contains("check")
    {
        ActionType::WaitFor
    } else if lower.contains("unlock") || (lower.contains("release") && lower.contains("lock")) {
        ActionType::ReleaseLock
    } else if lower.contains("lock") {
        ActionType::AcquireLock
    } else if lower.contains("mark") && lower.contains("handled") {
        ActionType::MarkEventHandled
    } else {
        ActionType::WorkflowStep
    }
}

fn workflow_step_metadata(step: &crate::plan::StepPlan) -> serde_json::Value {
    use crate::plan::StepAction;
    let mut meta = serde_json::Map::new();
    // Custom workflow plans can describe sends too; use the same category as
    // the displayed action so structured and human preview policy agree.
    if step_action_to_dry_run_type(&step.action) == ActionType::SendText {
        meta.insert("policy_gated".to_string(), serde_json::json!(true));
    }
    meta.insert(
        "step_id".to_string(),
        serde_json::json!(step.step_id.to_string()),
    );
    meta.insert("idempotent".to_string(), serde_json::json!(step.idempotent));
    if let Some(timeout) = step.timeout_ms {
        meta.insert("timeout_ms".to_string(), serde_json::json!(timeout));
    }
    if !step.preconditions.is_empty() {
        meta.insert(
            "precondition_count".to_string(),
            serde_json::json!(step.preconditions.len()),
        );
    }
    if step.verification.is_some() {
        meta.insert("has_verification".to_string(), serde_json::json!(true));
    }
    match &step.action {
        StepAction::SendText {
            pane_id,
            text,
            paste_mode,
        } => {
            meta.insert("pane_id".to_string(), serde_json::json!(pane_id));
            meta.insert("text_len".to_string(), serde_json::json!(text.len()));
            let preview = if text.len() > 64 * 1024 {
                crate::output::truncate_bounded(
                    "preview omitted: input exceeds safety limit",
                    60,
                    512,
                )
            } else {
                crate::output::sanitize_redact_truncate_bounded(text, 60, 512, |sanitized| {
                    Redactor::new().redact(sanitized)
                })
            };
            meta.insert("text_preview".to_string(), serde_json::json!(preview));
            if let Some(paste) = paste_mode {
                meta.insert("paste_mode".to_string(), serde_json::json!(paste));
            }
        }
        StepAction::WaitFor {
            pane_id,
            condition,
            timeout_ms,
        } => {
            if let Some(pane) = pane_id {
                meta.insert("pane_id".to_string(), serde_json::json!(pane));
            }
            meta.insert("wait_timeout_ms".to_string(), serde_json::json!(timeout_ms));
            meta.insert(
                "condition".to_string(),
                serde_json::json!(condition.canonical_string()),
            );
        }
        StepAction::AcquireLock {
            lock_name,
            timeout_ms,
        } => {
            meta.insert("lock_name".to_string(), serde_json::json!(lock_name));
            if let Some(timeout) = timeout_ms {
                meta.insert("lock_timeout_ms".to_string(), serde_json::json!(timeout));
            }
        }
        StepAction::ReleaseLock { lock_name } => {
            meta.insert("lock_name".to_string(), serde_json::json!(lock_name));
        }
        StepAction::Custom {
            action_type,
            payload,
        } => {
            meta.insert(
                "custom_action_type".to_string(),
                serde_json::json!(action_type),
            );
            meta.insert("custom_payload".to_string(), payload.clone());
        }
        _ => {}
    }
    serde_json::Value::Object(meta)
}

// ============================================================================
// Core Types
// ============================================================================

/// Context for command execution that carries dry-run intent.
#[derive(Debug, Clone)]
pub struct CommandContext {
    /// Whether dry-run mode is enabled
    pub dry_run: bool,
    /// Command string for reporting
    pub command: String,
}

impl CommandContext {
    /// Create a new command context
    #[must_use]
    pub fn new(command: impl Into<String>, dry_run: bool) -> Self {
        Self {
            dry_run,
            command: command.into(),
        }
    }

    /// Check if this is a dry-run execution
    #[must_use]
    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Build a dry-run context seeded with this command
    #[must_use]
    pub fn dry_run_context(&self) -> DryRunContext {
        let mut ctx = DryRunContext::from_flag(self.dry_run);
        ctx.set_command(self.command.clone());
        ctx
    }
}

/// Context for dry-run mode execution.
///
/// Carries the dry_run flag and collects information for the report.
#[derive(Debug, Clone, Default)]
pub struct DryRunContext {
    /// Whether dry-run mode is enabled
    pub enabled: bool,
    /// Report builder for collecting dry-run information
    pub report: DryRunReport,
}

impl DryRunContext {
    /// Create a new context with dry-run enabled
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            report: DryRunReport::default(),
        }
    }

    /// Create a new context with dry-run disabled (normal execution)
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            report: DryRunReport::default(),
        }
    }

    /// Create a context from a flag value
    #[must_use]
    pub fn from_flag(dry_run: bool) -> Self {
        if dry_run {
            Self::enabled()
        } else {
            Self::disabled()
        }
    }

    /// Check if this is a dry-run execution
    #[must_use]
    pub fn is_dry_run(&self) -> bool {
        self.enabled
    }

    /// Add a warning to the report
    pub fn add_warning(&mut self, warning: impl Into<String>) {
        self.report.warnings.push(warning.into());
    }

    /// Set the command being executed
    pub fn set_command(&mut self, command: impl Into<String>) {
        self.report.command = command.into();
    }

    /// Set target resolution information
    pub fn set_target(&mut self, target: TargetResolution) {
        self.report.target_resolution = Some(target);
    }

    /// Set policy evaluation result
    pub fn set_policy_evaluation(&mut self, evaluation: PolicyEvaluation) {
        self.report.policy_evaluation = Some(evaluation);
    }

    /// Add a planned action
    pub fn add_action(&mut self, action: PlannedAction) {
        self.report.expected_actions.push(action);
    }

    /// Take the report, consuming the context
    #[must_use]
    pub fn take_report(self) -> DryRunReport {
        self.report
    }
}

/// Report generated by a dry-run execution.
///
/// Contains all the information about what would happen if the command
/// were executed for real.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DryRunReport {
    /// The command being executed
    pub command: String,

    /// Target resolution information (pane, domain, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_resolution: Option<TargetResolution>,

    /// Policy evaluation results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_evaluation: Option<PolicyEvaluation>,

    /// Expected actions that would be performed
    pub expected_actions: Vec<PlannedAction>,

    /// Warnings encountered during dry-run
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl DryRunReport {
    /// Create a new empty report
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a report with a command
    #[must_use]
    pub fn with_command(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            ..Default::default()
        }
    }

    /// Check if there are any warnings
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Check if all policy checks passed
    #[must_use]
    pub fn policy_passed(&self) -> bool {
        self.policy_evaluation
            .as_ref()
            .is_none_or(PolicyEvaluation::all_passed)
    }

    /// Get the number of planned actions
    #[must_use]
    pub fn action_count(&self) -> usize {
        self.expected_actions.len()
    }

    /// Return a redacted copy of this report for safe output.
    #[must_use]
    pub fn redacted(&self) -> Self {
        let redactor = Redactor::new();
        let mut report = self.clone();

        report.command = redact_report_text(&report.command, &redactor);

        if let Some(target) = &mut report.target_resolution {
            target.domain = redact_report_text(&target.domain, &redactor);
            if let Some(title) = &mut target.title {
                *title = redact_report_text(title, &redactor);
            }
            if let Some(cwd) = &mut target.cwd {
                *cwd = redact_report_text(cwd, &redactor);
            }
            if let Some(agent) = &mut target.agent_type {
                *agent = redact_report_text(agent, &redactor);
            }
        }

        if let Some(policy) = &mut report.policy_evaluation {
            for check in &mut policy.checks {
                check.name = redact_report_text(&check.name, &redactor);
                check.message = redact_report_text(&check.message, &redactor);
                if let Some(details) = &mut check.details {
                    *details = redact_report_text(details, &redactor);
                }
            }
        }

        for action in &mut report.expected_actions {
            action.description = redact_report_text(&action.description, &redactor);
            if let Some(metadata) = &mut action.metadata {
                redact_json_value(metadata, &redactor);
            }
        }

        for warning in &mut report.warnings {
            *warning = redact_report_text(warning, &redactor);
        }

        report
    }
}

// ============================================================================
// Target Resolution
// ============================================================================

/// Information about the resolved target for a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetResolution {
    /// Target pane ID
    pub pane_id: u64,

    /// Pane title (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Current working directory
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,

    /// Domain (local, ssh:host, etc.)
    pub domain: String,

    /// Whether the pane is currently active
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,

    /// Detected agent type running in pane
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

impl TargetResolution {
    /// Create a new target resolution
    #[must_use]
    pub fn new(pane_id: u64, domain: impl Into<String>) -> Self {
        Self {
            pane_id,
            title: None,
            cwd: None,
            domain: domain.into(),
            is_active: None,
            agent_type: None,
        }
    }

    /// Set the pane title
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the current working directory
    #[must_use]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set whether the pane is active
    #[must_use]
    pub fn with_is_active(mut self, is_active: bool) -> Self {
        self.is_active = Some(is_active);
        self
    }

    /// Set the detected agent type
    #[must_use]
    pub fn with_agent_type(mut self, agent_type: impl Into<String>) -> Self {
        self.agent_type = Some(agent_type.into());
        self
    }
}

// ============================================================================
// Policy Evaluation
// ============================================================================

/// Results of policy checks for a command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEvaluation {
    /// Individual check results
    pub checks: Vec<PolicyCheck>,
}

impl PolicyEvaluation {
    /// Create a new policy evaluation
    #[must_use]
    pub fn new() -> Self {
        Self { checks: Vec::new() }
    }

    /// Add a policy check result
    pub fn add_check(&mut self, check: PolicyCheck) {
        self.checks.push(check);
    }

    /// Check if all policy checks passed
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// Get failed checks
    #[must_use]
    pub fn failed_checks(&self) -> Vec<&PolicyCheck> {
        self.checks.iter().filter(|c| !c.passed).collect()
    }
}

impl Default for PolicyEvaluation {
    fn default() -> Self {
        Self::new()
    }
}

/// A single policy check result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyCheck {
    /// Name of the check
    pub name: String,

    /// Whether the check passed
    pub passed: bool,

    /// Human-readable status message
    pub message: String,

    /// Additional details
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

impl PolicyCheck {
    /// Create a passing check
    #[must_use]
    pub fn passed(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: true,
            message: message.into(),
            details: None,
        }
    }

    /// Create a failing check
    #[must_use]
    pub fn failed(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            passed: false,
            message: message.into(),
            details: None,
        }
    }

    /// Add details to the check
    #[must_use]
    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }
}

/// Convert a PolicyDecision to a PolicyCheck
impl From<&PolicyDecision> for PolicyCheck {
    fn from(decision: &PolicyDecision) -> Self {
        match decision {
            PolicyDecision::Allow { .. } => Self::passed("policy", "Operation allowed"),
            PolicyDecision::Deny { reason, .. } => Self::failed("policy", reason.as_str()),
            PolicyDecision::RequireApproval { reason, .. } => {
                Self::failed("policy", format!("Approval required: {reason}"))
            }
        }
    }
}

// ============================================================================
// Planned Actions
// ============================================================================

/// A planned action that would be performed during execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedAction {
    /// Step number in sequence
    pub step: u32,

    /// Action type
    pub action_type: ActionType,

    /// Human-readable description
    pub description: String,

    /// Additional metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

impl PlannedAction {
    /// Create a new planned action
    #[must_use]
    pub fn new(step: u32, action_type: ActionType, description: impl Into<String>) -> Self {
        Self {
            step,
            action_type,
            description: description.into(),
            metadata: None,
        }
    }

    /// Add metadata to the action
    #[must_use]
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// Types of actions that can be planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    /// Send text to a pane
    SendText,
    /// Wait for a condition
    WaitFor,
    /// Acquire a lock
    AcquireLock,
    /// Release a lock
    ReleaseLock,
    /// Store data in database
    StoreData,
    /// Execute a workflow step
    WorkflowStep,
    /// Mark an event as handled
    MarkEventHandled,
    /// Validate approval token
    ValidateApproval,
    /// Other/custom action
    Other,
}

impl fmt::Display for ActionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SendText => write!(f, "send-text"),
            Self::WaitFor => write!(f, "wait-for"),
            Self::AcquireLock => write!(f, "acquire-lock"),
            Self::ReleaseLock => write!(f, "release-lock"),
            Self::StoreData => write!(f, "store-data"),
            Self::WorkflowStep => write!(f, "workflow-step"),
            Self::MarkEventHandled => write!(f, "mark-event-handled"),
            Self::ValidateApproval => write!(f, "validate-approval"),
            Self::Other => write!(f, "other"),
        }
    }
}

// ============================================================================
// Output Formatting
// ============================================================================

fn redact_report_text(text: &str, redactor: &Redactor) -> String {
    redactor.redact(&crate::output::normalize_terminal_text_for_redaction(text))
}

fn redact_json_value(value: &mut serde_json::Value, redactor: &Redactor) {
    match value {
        serde_json::Value::String(text) => {
            *text = redact_report_text(text, redactor);
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_value(item, redactor);
            }
        }
        serde_json::Value::Object(map) => {
            if map.keys().any(|key| {
                let normalized = crate::output::normalize_terminal_text_for_redaction(key);
                redactor.redact(&normalized) != normalized
            }) {
                // Renaming arbitrary payload keys could collide and silently
                // replace a sibling. Explicitly omit this sensitive object.
                *value = serde_json::json!("[REDACTED: metadata object contains sensitive keys]");
                return;
            }
            for value in map.values_mut() {
                redact_json_value(value, redactor);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

/// Format a dry-run report as JSON
pub fn format_json(report: &DryRunReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&report.redacted())
}

/// Format a dry-run report for human-readable TTY output
#[must_use]
pub fn format_human(report: &DryRunReport) -> String {
    use std::fmt::Write;

    let report = report.redacted();
    let mut output = String::new();

    // Header
    output.push_str("DRY RUN - No changes will be made\n");
    output.push_str(&"─".repeat(40));
    output.push('\n');

    // Command
    if !report.command.is_empty() {
        let _ = writeln!(output, "\nCommand: {}", report.command);
    }

    // Target Resolution
    if let Some(target) = &report.target_resolution {
        output.push_str("\nTarget Resolution:\n");
        let _ = write!(output, "  Pane: {}", target.pane_id);
        if let Some(title) = &target.title {
            let _ = write!(output, " ({title})");
        }
        output.push('\n');
        let _ = writeln!(output, "  Domain: {}", target.domain);
        if let Some(cwd) = &target.cwd {
            let _ = writeln!(output, "  CWD: {cwd}");
        }
        if let Some(agent) = &target.agent_type {
            let _ = writeln!(output, "  Agent: {agent}");
        }
    }

    // Policy Evaluation
    if let Some(policy) = &report.policy_evaluation {
        output.push_str("\nPolicy Evaluation:\n");
        for check in &policy.checks {
            let symbol = if check.passed { "✓" } else { "✗" };
            let _ = writeln!(output, "  {symbol} {}: {}", check.name, check.message);
            if let Some(details) = &check.details {
                let _ = writeln!(output, "    {details}");
            }
        }
    }

    // Expected Actions
    if !report.expected_actions.is_empty() {
        output.push_str("\nExpected Actions:\n");
        for action in &report.expected_actions {
            let _ = writeln!(
                output,
                "  {}. [{}] {}",
                action.step, action.action_type, action.description
            );
        }
    }

    // Warnings
    if !report.warnings.is_empty() {
        output.push_str("\nWarnings:\n");
        for warning in &report.warnings {
            let _ = writeln!(output, "  ⚠ {warning}");
        }
    }

    // Footer with execution hint
    output.push('\n');
    output.push_str(&"─".repeat(40));
    output.push_str("\nTo execute for real, remove --dry-run flag\n");

    output
}

// ============================================================================
// Builder Helpers
// ============================================================================

/// Helper to build a policy evaluation for a send command
#[must_use]
pub fn build_send_policy_evaluation(
    rate_limit_status: (u32, u32), // (current, limit)
    is_prompt_active: bool,
    require_prompt_active: bool,
    has_recent_gaps: bool,
) -> PolicyEvaluation {
    let mut eval = PolicyEvaluation::new();

    // Rate limit check
    let (current, limit) = rate_limit_status;
    if limit == 0 {
        eval.add_check(PolicyCheck::passed(
            "rate_limit",
            "Rate limit disabled".to_string(),
        ));
    } else if current < limit {
        eval.add_check(PolicyCheck::passed(
            "rate_limit",
            format!("{current}/{limit} sends in last minute (within budget)"),
        ));
    } else {
        eval.add_check(PolicyCheck::failed(
            "rate_limit",
            format!("Rate limit exceeded: {current}/{limit}"),
        ));
    }

    // Prompt state check
    if require_prompt_active {
        if is_prompt_active {
            eval.add_check(PolicyCheck::passed(
                "pane_state",
                "PromptActive (safe to send)",
            ));
        } else {
            eval.add_check(PolicyCheck::failed(
                "pane_state",
                "Prompt not active - command may be running",
            ));
        }
    } else {
        eval.add_check(PolicyCheck::passed(
            "pane_state",
            "Prompt check not required",
        ));
    }

    // Continuity check
    if has_recent_gaps {
        eval.add_check(
            PolicyCheck::passed("continuity", "Recent gap detected")
                .with_details("Output continuity may be affected; consider waiting for stability"),
        );
    } else {
        eval.add_check(PolicyCheck::passed("continuity", "No recent gaps (OK)"));
    }

    eval
}

/// Helper to create a send action
#[must_use]
pub fn create_send_action(step: u32, pane_id: u64, text_len: usize) -> PlannedAction {
    PlannedAction::new(
        step,
        ActionType::SendText,
        format!(
            "Inject {text_len} characters through the direct mux transport into pane {pane_id}"
        ),
    )
}

/// Helper to create a wait-for action
#[must_use]
pub fn create_wait_for_action(
    step: u32,
    condition: impl Into<String>,
    timeout_ms: u64,
) -> PlannedAction {
    PlannedAction::new(
        step,
        ActionType::WaitFor,
        format!("Wait for: {} (timeout: {}ms)", condition.into(), timeout_ms),
    )
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_custom_send_metadata_retains_policy_gate_without_gating_waits() {
        for (name, gated) in [("send_prompt", true), ("wait_for_prompt", false)] {
            let step = crate::plan::StepPlan::new(
                1,
                crate::plan::StepAction::Custom {
                    action_type: name.to_string(),
                    payload: serde_json::json!({"pane_id": 42}),
                },
                "preview custom workflow step",
            );
            let metadata = workflow_step_metadata(&step);
            assert_eq!(
                metadata.get("policy_gated"),
                gated.then_some(&serde_json::Value::Bool(true))
            );
            assert_eq!(metadata["custom_action_type"], name);
            assert_eq!(metadata["custom_payload"]["pane_id"], 42);
        }
    }

    #[test]
    fn report_without_warnings_roundtrips_but_malformed_warnings_are_rejected() {
        let report = DryRunReport::with_command("workflow run");
        let mut wire = serde_json::to_value(&report).unwrap();
        assert!(wire.get("warnings").is_none());
        let decoded: DryRunReport = serde_json::from_value(wire.clone()).unwrap();
        assert!(decoded.warnings.is_empty());
        assert_eq!(decoded.command, report.command);
        wire["warnings"] = serde_json::json!(false);
        assert!(serde_json::from_value::<DryRunReport>(wire).is_err());
    }

    #[test]
    fn workflow_custom_lock_preview_distinguishes_acquire_and_release() {
        for name in [
            "release_lock",
            "unlock",
            "unlock_pane",
            "release_workflow_lock",
        ] {
            assert_eq!(
                infer_action_type_from_name(name),
                ActionType::ReleaseLock,
                "{name}"
            );
        }
        for name in ["acquire_lock", "lock_pane"] {
            assert_eq!(
                infer_action_type_from_name(name),
                ActionType::AcquireLock,
                "{name}"
            );
        }
    }

    #[test]
    fn workflow_preview_redacts_nested_values_wait_conditions_and_object_keys() {
        let secret = format!("sk-ant-api03-{}", "a".repeat(48));
        let split = format!("{}\x1b[31m{}\x1b[0m", &secret[..18], &secret[18..]);
        let mut report = DryRunReport::with_command("workflow run");
        report.expected_actions.push(
            PlannedAction::new(1, ActionType::WorkflowStep, "custom step").with_metadata(
                serde_json::json!({
                    "custom_payload": {"nested": [{"value": split}], "plain": "keep me"},
                    "condition": split,
                "sensitive_keys": {(split.clone()): "payload"},
                }),
            ),
        );
        let output = report.redacted();
        let metadata = output.expected_actions[0].metadata.as_ref().unwrap();
        assert_eq!(metadata["custom_payload"]["plain"], "keep me");
        assert_eq!(
            metadata["sensitive_keys"],
            "[REDACTED: metadata object contains sensitive keys]"
        );
        let encoded = serde_json::to_string(&output).unwrap();
        assert!(
            !encoded.contains(&secret[18..]),
            "split secret suffix escaped final preview boundary"
        );
        assert!(
            metadata["condition"]
                .as_str()
                .unwrap()
                .contains("[REDACTED]")
        );
        assert!(
            metadata["custom_payload"]["nested"][0]["value"]
                .as_str()
                .unwrap()
                .contains("[REDACTED]")
        );
    }

    #[test]
    fn dry_run_context_creation() {
        let enabled = DryRunContext::enabled();
        assert!(enabled.is_dry_run());

        let disabled = DryRunContext::disabled();
        assert!(!disabled.is_dry_run());

        let from_flag_true = DryRunContext::from_flag(true);
        assert!(from_flag_true.is_dry_run());

        let from_flag_false = DryRunContext::from_flag(false);
        assert!(!from_flag_false.is_dry_run());
    }

    #[test]
    fn command_context_builds_dry_run_context() {
        let ctx = CommandContext::new("ft send --pane 1 \"hi\" --dry-run", true);
        let dry_ctx = ctx.dry_run_context();
        assert!(dry_ctx.is_dry_run());
        assert_eq!(dry_ctx.report.command, "ft send --pane 1 \"hi\" --dry-run");
    }

    #[test]
    fn dry_run_context_building() {
        let mut ctx = DryRunContext::enabled();
        ctx.set_command("ft send --pane 1 \"hello\"");
        ctx.set_target(TargetResolution::new(1, "local").with_title("test"));
        ctx.add_warning("Test warning");
        ctx.add_action(PlannedAction::new(1, ActionType::SendText, "Send hello"));

        let report = ctx.take_report();
        assert_eq!(report.command, "ft send --pane 1 \"hello\"");
        assert!(report.target_resolution.is_some());
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.expected_actions.len(), 1);
    }

    #[test]
    fn dry_run_report_serialization() {
        let mut report = DryRunReport::with_command("test command");
        report.target_resolution = Some(TargetResolution::new(42, "local"));
        report.warnings.push("warning 1".to_string());

        let json = format_json(&report).expect("serialization should succeed");
        assert!(json.contains("test command"));
        assert!(json.contains("42"));
        assert!(json.contains("warning 1"));
    }

    #[test]
    fn format_json_redacts_secrets() {
        let secret = "sk-abc123456789012345678901234567890123456789012345678901";
        let mut report = DryRunReport::with_command(format!("ft send {secret}"));
        report
            .warnings
            .push(format!("token: {secret} should be hidden"));

        let json = format_json(&report).expect("serialization should succeed");
        assert!(json.contains("[REDACTED]"));
        assert!(!json.contains("sk-abc"));
    }

    #[test]
    fn policy_evaluation_all_passed() {
        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("check1", "ok"));
        eval.add_check(PolicyCheck::passed("check2", "ok"));
        assert!(eval.all_passed());

        eval.add_check(PolicyCheck::failed("check3", "failed"));
        assert!(!eval.all_passed());
    }

    #[test]
    fn policy_check_from_policy_decision() {
        use crate::policy::PolicyDecision;

        let allowed = PolicyDecision::allow();
        let check: PolicyCheck = (&allowed).into();
        assert!(check.passed);

        let denied = PolicyDecision::deny("test reason");
        let check: PolicyCheck = (&denied).into();
        assert!(!check.passed);
        assert_eq!(check.message, "test reason");

        let approval = PolicyDecision::require_approval("approval needed");
        let check: PolicyCheck = (&approval).into();
        assert!(!check.passed);
        assert!(check.message.contains("approval needed"));
    }

    #[test]
    fn human_format_includes_all_sections() {
        let mut report = DryRunReport::with_command("test");
        report.target_resolution = Some(TargetResolution::new(1, "local"));

        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("test", "passed"));
        report.policy_evaluation = Some(eval);

        report
            .expected_actions
            .push(PlannedAction::new(1, ActionType::SendText, "action"));
        report.warnings.push("warning".to_string());

        let output = format_human(&report);
        assert!(output.contains("DRY RUN"));
        assert!(output.contains("Target Resolution"));
        assert!(output.contains("Policy Evaluation"));
        assert!(output.contains("Expected Actions"));
        assert!(output.contains("Warnings"));
    }

    #[test]
    fn build_send_policy_evaluation_all_ok() {
        let eval = build_send_policy_evaluation((5, 30), true, true, false);
        assert!(eval.all_passed());
        assert_eq!(eval.checks.len(), 3);
    }

    #[test]
    fn build_send_policy_evaluation_rate_limited() {
        let eval = build_send_policy_evaluation((30, 30), true, true, false);
        assert!(!eval.all_passed());
        assert_eq!(eval.failed_checks().len(), 1);
    }

    #[test]
    fn create_send_action_format() {
        let action = create_send_action(1, 42, 100);
        assert_eq!(action.step, 1);
        assert_eq!(action.action_type, ActionType::SendText);
        assert!(action.description.contains("100 characters"));
        assert!(action.description.contains("42"));
        assert!(action.description.contains("direct mux transport"));
        assert!(!action.description.contains("cli send-text"));
    }

    #[test]
    fn action_type_display() {
        assert_eq!(format!("{}", ActionType::SendText), "send-text");
        assert_eq!(format!("{}", ActionType::WaitFor), "wait-for");
        assert_eq!(format!("{}", ActionType::AcquireLock), "acquire-lock");
    }

    #[test]
    fn target_resolution_builder() {
        let target = TargetResolution::new(1, "local")
            .with_title("test title")
            .with_cwd("/home/user")
            .with_is_active(true)
            .with_agent_type("claude_code");

        assert_eq!(target.pane_id, 1);
        assert_eq!(target.domain, "local");
        assert_eq!(target.title, Some("test title".to_string()));
        assert_eq!(target.cwd, Some("/home/user".to_string()));
        assert_eq!(target.is_active, Some(true));
        assert_eq!(target.agent_type, Some("claude_code".to_string()));
    }

    // ========================================================================
    // wa-1pe.5: Additional dry-run tests for error paths and edge cases
    // ========================================================================

    #[test]
    fn report_policy_passed_helper() {
        // Empty report (no policy evaluation) should pass
        let empty_report = DryRunReport::new();
        assert!(empty_report.policy_passed());

        // Report with all passing checks should pass
        let mut report = DryRunReport::new();
        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("check1", "ok"));
        eval.add_check(PolicyCheck::passed("check2", "ok"));
        report.policy_evaluation = Some(eval);
        assert!(report.policy_passed());

        // Report with failing check should not pass
        let mut report_fail = DryRunReport::new();
        let mut eval_fail = PolicyEvaluation::new();
        eval_fail.add_check(PolicyCheck::passed("check1", "ok"));
        eval_fail.add_check(PolicyCheck::failed("check2", "denied"));
        report_fail.policy_evaluation = Some(eval_fail);
        assert!(!report_fail.policy_passed());
    }

    #[test]
    fn report_action_count_helper() {
        let mut report = DryRunReport::new();
        assert_eq!(report.action_count(), 0);

        report
            .expected_actions
            .push(PlannedAction::new(1, ActionType::SendText, "send"));
        assert_eq!(report.action_count(), 1);

        report
            .expected_actions
            .push(PlannedAction::new(2, ActionType::WaitFor, "wait"));
        assert_eq!(report.action_count(), 2);
    }

    #[test]
    fn report_has_warnings_helper() {
        let mut report = DryRunReport::new();
        assert!(!report.has_warnings());

        report.warnings.push("warning 1".to_string());
        assert!(report.has_warnings());
    }

    #[test]
    fn policy_denial_shows_clear_reason() {
        use crate::policy::PolicyDecision;

        // Deny with detailed reason
        let denied = PolicyDecision::deny("Pane 42 is in alt-screen mode");
        let check: PolicyCheck = (&denied).into();

        assert!(!check.passed);
        assert!(check.message.contains("alt-screen"));

        // Require approval with reason
        let approval = PolicyDecision::require_approval("Command requires human review");
        let check: PolicyCheck = (&approval).into();

        assert!(!check.passed);
        assert!(check.message.contains("human review"));
    }

    #[test]
    fn human_format_shows_policy_failure_clearly() {
        let mut report = DryRunReport::with_command("ft send 42 \"rm -rf /\"");

        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("rate_limit", "within budget"));
        eval.add_check(PolicyCheck::failed(
            "command_safety",
            "Command appears destructive",
        ));
        report.policy_evaluation = Some(eval);

        let output = format_human(&report);

        // Should contain failure indicator
        assert!(output.contains("✗"));
        assert!(output.contains("destructive"));
        assert!(output.contains("command_safety"));
    }

    #[test]
    fn prompt_inactive_policy_shows_warning() {
        let eval = build_send_policy_evaluation((0, 30), false, true, false);

        assert!(!eval.all_passed());
        let failed = eval.failed_checks();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].message.contains("Prompt not active"));
    }

    #[test]
    fn rate_limit_disabled_shows_disabled() {
        let eval = build_send_policy_evaluation((0, 0), true, true, false);

        assert!(eval.all_passed());
        assert!(eval.checks.iter().any(|c| c.message.contains("disabled")));
    }

    #[test]
    fn recent_gaps_shows_details() {
        let eval = build_send_policy_evaluation((0, 30), true, false, true);

        assert!(eval.all_passed());
        let gap_check = eval.checks.iter().find(|c| c.name == "continuity").unwrap();
        assert!(gap_check.details.is_some());
        assert!(gap_check.details.as_ref().unwrap().contains("stability"));
    }

    #[test]
    fn wait_for_action_includes_timeout() {
        let action = create_wait_for_action(2, "prompt boundary", 30000);

        assert_eq!(action.step, 2);
        assert_eq!(action.action_type, ActionType::WaitFor);
        assert!(action.description.contains("prompt boundary"));
        assert!(action.description.contains("30000"));
    }

    #[test]
    fn planned_action_with_metadata() {
        let metadata = serde_json::json!({
            "target_pane": 42,
            "text_length": 100
        });
        let action =
            PlannedAction::new(1, ActionType::SendText, "send text").with_metadata(metadata);

        assert!(action.metadata.is_some());
        let meta = action.metadata.unwrap();
        assert_eq!(meta["target_pane"], 42);
    }

    #[test]
    fn redaction_covers_all_report_fields() {
        let secret = "sk-secret123456789012345678901234567890123456789012345678";

        let mut report = DryRunReport::with_command(format!("ft send {secret}"));
        report.target_resolution = Some(
            TargetResolution::new(1, format!("ssh:{secret}@host"))
                .with_title(format!("session-{secret}"))
                .with_cwd(format!("/home/{secret}"))
                .with_agent_type(format!("agent-{secret}")),
        );

        let mut eval = PolicyEvaluation::new();
        eval.add_check(
            PolicyCheck::passed("check", format!("token: {secret}"))
                .with_details(format!("details: {secret}")),
        );
        report.policy_evaluation = Some(eval);

        report.expected_actions.push(
            PlannedAction::new(1, ActionType::SendText, format!("send {secret}"))
                .with_metadata(serde_json::json!({"secret": secret})),
        );

        report.warnings.push(format!("warning: {secret}"));

        let redacted = report.redacted();

        // Verify all fields are redacted
        assert!(!redacted.command.contains("sk-secret"));
        let target = redacted.target_resolution.as_ref().unwrap();
        assert!(!target.domain.contains("sk-secret"));
        assert!(!target.title.as_ref().unwrap().contains("sk-secret"));
        assert!(!target.cwd.as_ref().unwrap().contains("sk-secret"));
        assert!(!target.agent_type.as_ref().unwrap().contains("sk-secret"));

        let policy = redacted.policy_evaluation.as_ref().unwrap();
        assert!(!policy.checks[0].message.contains("sk-secret"));
        assert!(
            !policy.checks[0]
                .details
                .as_ref()
                .unwrap()
                .contains("sk-secret")
        );

        assert!(
            !redacted.expected_actions[0]
                .description
                .contains("sk-secret")
        );
        assert!(!redacted.warnings[0].contains("sk-secret"));
    }

    #[test]
    fn json_format_produces_valid_json() {
        let mut report = DryRunReport::with_command("ft send 1 \"test\"");
        report.target_resolution = Some(TargetResolution::new(1, "local"));

        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("test", "ok"));
        report.policy_evaluation = Some(eval);

        report
            .expected_actions
            .push(PlannedAction::new(1, ActionType::SendText, "send"));

        let json = format_json(&report).expect("should serialize");

        // Verify it's valid JSON by parsing
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("should parse");

        assert!(parsed.get("command").is_some());
        assert!(parsed.get("target_resolution").is_some());
        assert!(parsed.get("policy_evaluation").is_some());
        assert!(parsed.get("expected_actions").is_some());
    }

    #[test]
    fn action_type_all_variants_display() {
        // Ensure all action types have display implementations
        assert_eq!(format!("{}", ActionType::SendText), "send-text");
        assert_eq!(format!("{}", ActionType::WaitFor), "wait-for");
        assert_eq!(format!("{}", ActionType::AcquireLock), "acquire-lock");
        assert_eq!(format!("{}", ActionType::ReleaseLock), "release-lock");
        assert_eq!(format!("{}", ActionType::StoreData), "store-data");
        assert_eq!(format!("{}", ActionType::WorkflowStep), "workflow-step");
        assert_eq!(
            format!("{}", ActionType::MarkEventHandled),
            "mark-event-handled"
        );
        assert_eq!(
            format!("{}", ActionType::ValidateApproval),
            "validate-approval"
        );
        assert_eq!(format!("{}", ActionType::Other), "other");
    }

    #[test]
    fn policy_check_with_details() {
        let check = PolicyCheck::passed("rate_limit", "5/30 sends")
            .with_details("Last send was 30 seconds ago");

        assert!(check.passed);
        assert_eq!(check.name, "rate_limit");
        assert!(check.details.is_some());
        assert!(check.details.unwrap().contains("30 seconds"));
    }

    // ---------------------------------------------------------------
    // Expanded pure unit tests (wa-1u90p.7.1)
    // ---------------------------------------------------------------

    #[test]
    fn command_context_clone() {
        let ctx = CommandContext::new("ft send 1 hello", true);
        let c = ctx.clone();
        assert_eq!(c.command, "ft send 1 hello");
        assert!(c.dry_run);
    }

    #[test]
    fn command_context_debug() {
        let ctx = CommandContext::new("cmd", false);
        let dbg = format!("{:?}", ctx);
        assert!(dbg.contains("CommandContext"));
        assert!(dbg.contains("cmd"));
    }

    #[test]
    fn command_context_not_dry_run() {
        let ctx = CommandContext::new("ft send 1 test", false);
        assert!(!ctx.is_dry_run());
        let dry_ctx = ctx.dry_run_context();
        assert!(!dry_ctx.is_dry_run());
    }

    #[test]
    fn dry_run_context_default() {
        let ctx = DryRunContext::default();
        assert!(!ctx.enabled);
        assert_eq!(ctx.report.command, "");
        assert!(ctx.report.expected_actions.is_empty());
    }

    #[test]
    fn dry_run_context_clone() {
        let mut ctx = DryRunContext::enabled();
        ctx.set_command("test");
        ctx.add_warning("w1");
        let c = ctx.clone();
        assert!(c.enabled);
        assert_eq!(c.report.command, "test");
        assert_eq!(c.report.warnings.len(), 1);
    }

    #[test]
    fn dry_run_context_multiple_warnings() {
        let mut ctx = DryRunContext::enabled();
        ctx.add_warning("w1");
        ctx.add_warning("w2");
        ctx.add_warning("w3");
        let report = ctx.take_report();
        assert_eq!(report.warnings.len(), 3);
        assert_eq!(report.warnings[0], "w1");
        assert_eq!(report.warnings[2], "w3");
    }

    #[test]
    fn dry_run_context_multiple_actions() {
        let mut ctx = DryRunContext::enabled();
        ctx.add_action(PlannedAction::new(1, ActionType::AcquireLock, "lock"));
        ctx.add_action(PlannedAction::new(2, ActionType::SendText, "send"));
        ctx.add_action(PlannedAction::new(3, ActionType::ReleaseLock, "unlock"));
        let report = ctx.take_report();
        assert_eq!(report.action_count(), 3);
        assert_eq!(report.expected_actions[0].step, 1);
        assert_eq!(report.expected_actions[2].step, 3);
    }

    #[test]
    fn dry_run_report_new_is_default() {
        let r = DryRunReport::new();
        assert_eq!(r.command, "");
        assert!(r.target_resolution.is_none());
        assert!(r.policy_evaluation.is_none());
        assert!(r.expected_actions.is_empty());
        assert!(!r.has_warnings());
    }

    #[test]
    fn dry_run_report_with_command_sets_command() {
        let r = DryRunReport::with_command("my-cmd");
        assert_eq!(r.command, "my-cmd");
    }

    #[test]
    fn dry_run_report_has_warnings() {
        let mut r = DryRunReport::new();
        assert!(!r.has_warnings());
        r.warnings.push("warning".to_string());
        assert!(r.has_warnings());
    }

    #[test]
    fn dry_run_report_policy_passed_none() {
        let r = DryRunReport::new();
        assert!(r.policy_passed(), "no policy = passed");
    }

    #[test]
    fn dry_run_report_policy_passed_all_pass() {
        let mut r = DryRunReport::new();
        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("a", "ok"));
        eval.add_check(PolicyCheck::passed("b", "ok"));
        r.policy_evaluation = Some(eval);
        assert!(r.policy_passed());
    }

    #[test]
    fn dry_run_report_policy_passed_with_failure() {
        let mut r = DryRunReport::new();
        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("a", "ok"));
        eval.add_check(PolicyCheck::failed("b", "denied"));
        r.policy_evaluation = Some(eval);
        assert!(!r.policy_passed());
    }

    #[test]
    fn dry_run_report_action_count() {
        let mut r = DryRunReport::new();
        assert_eq!(r.action_count(), 0);
        r.expected_actions
            .push(PlannedAction::new(1, ActionType::Other, "x"));
        assert_eq!(r.action_count(), 1);
    }

    #[test]
    fn dry_run_report_serde_roundtrip() {
        let mut r = DryRunReport::with_command("test");
        r.target_resolution = Some(TargetResolution::new(5, "local"));
        r.warnings.push("w".to_string());
        r.expected_actions
            .push(PlannedAction::new(1, ActionType::StoreData, "store"));

        let json = serde_json::to_string(&r).unwrap();
        let parsed: DryRunReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.command, "test");
        assert!(parsed.target_resolution.is_some());
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.action_count(), 1);
    }

    #[test]
    fn target_resolution_builder_chain() {
        let target = TargetResolution::new(42, "local")
            .with_title("my-pane")
            .with_cwd("/home/user")
            .with_is_active(true)
            .with_agent_type("claude-code");

        assert_eq!(target.pane_id, 42);
        assert_eq!(target.domain, "local");
        assert_eq!(target.title.as_deref(), Some("my-pane"));
        assert_eq!(target.cwd.as_deref(), Some("/home/user"));
        assert_eq!(target.is_active, Some(true));
        assert_eq!(target.agent_type.as_deref(), Some("claude-code"));
    }

    #[test]
    fn target_resolution_serde_roundtrip() {
        let target = TargetResolution::new(1, "ssh:host1")
            .with_title("remote")
            .with_cwd("/tmp");

        let json = serde_json::to_string(&target).unwrap();
        let parsed: TargetResolution = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.pane_id, 1);
        assert_eq!(parsed.domain, "ssh:host1");
        assert_eq!(parsed.title.as_deref(), Some("remote"));
    }

    #[test]
    fn target_resolution_minimal() {
        let target = TargetResolution::new(0, "local");
        assert_eq!(target.pane_id, 0);
        assert!(target.title.is_none());
        assert!(target.cwd.is_none());
        assert!(target.is_active.is_none());
        assert!(target.agent_type.is_none());
    }

    #[test]
    fn policy_evaluation_empty_all_passed() {
        let eval = PolicyEvaluation::new();
        assert!(eval.all_passed(), "empty checks = all passed");
        assert!(eval.failed_checks().is_empty());
    }

    #[test]
    fn policy_evaluation_failed_checks() {
        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("a", "ok"));
        eval.add_check(PolicyCheck::failed("b", "nope"));
        eval.add_check(PolicyCheck::failed("c", "denied"));
        let failed = eval.failed_checks();
        assert_eq!(failed.len(), 2);
        assert_eq!(failed[0].name, "b");
        assert_eq!(failed[1].name, "c");
    }

    #[test]
    fn policy_evaluation_serde_roundtrip() {
        let mut eval = PolicyEvaluation::new();
        eval.add_check(PolicyCheck::passed("rate_limit", "ok"));
        let json = serde_json::to_string(&eval).unwrap();
        let parsed: PolicyEvaluation = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.checks.len(), 1);
        assert!(parsed.all_passed());
    }

    #[test]
    fn policy_check_passed_and_failed() {
        let p = PolicyCheck::passed("a", "good");
        assert!(p.passed);
        assert_eq!(p.name, "a");
        assert!(p.details.is_none());

        let f = PolicyCheck::failed("b", "bad");
        assert!(!f.passed);
        assert_eq!(f.name, "b");
    }

    #[test]
    fn policy_check_serde_roundtrip() {
        let check = PolicyCheck::failed("rate_limit", "exceeded").with_details("5/5 sends in 30s");
        let json = serde_json::to_string(&check).unwrap();
        let parsed: PolicyCheck = serde_json::from_str(&json).unwrap();
        assert!(!parsed.passed);
        assert_eq!(parsed.details.as_deref(), Some("5/5 sends in 30s"));
    }

    #[test]
    fn planned_action_builder_with_metadata() {
        let meta = serde_json::json!({"target_pane": 42, "text": "hello"});
        let action =
            PlannedAction::new(1, ActionType::SendText, "send text").with_metadata(meta.clone());
        assert_eq!(action.step, 1);
        assert_eq!(action.action_type, ActionType::SendText);
        assert_eq!(action.metadata.unwrap(), meta);
    }

    #[test]
    fn planned_action_serde_roundtrip() {
        let action = PlannedAction::new(3, ActionType::WorkflowStep, "execute step");
        let json = serde_json::to_string(&action).unwrap();
        let parsed: PlannedAction = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.step, 3);
        assert_eq!(parsed.action_type, ActionType::WorkflowStep);
        assert_eq!(parsed.description, "execute step");
    }

    #[test]
    fn action_type_eq_and_copy() {
        let a = ActionType::SendText;
        let b = a; // Copy
        assert_eq!(a, b);
        assert_ne!(ActionType::SendText, ActionType::WaitFor);
    }

    #[test]
    fn action_type_serde_roundtrip() {
        let variants = [
            ActionType::SendText,
            ActionType::WaitFor,
            ActionType::AcquireLock,
            ActionType::ReleaseLock,
            ActionType::StoreData,
            ActionType::WorkflowStep,
            ActionType::MarkEventHandled,
            ActionType::ValidateApproval,
            ActionType::Other,
        ];
        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let parsed: ActionType = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, parsed);
        }
    }
}
