//! Safety and policy engine
//!
//! Provides capability gates, rate limiting, and secret redaction.
//!
//! # Architecture
//!
//! The policy engine provides a unified authorization layer for all actions:
//!
//! - [`ActionKind`] - Enumerates all actions that require authorization
//! - [`PolicyDecision`] - The result of policy evaluation (Allow/Deny/RequireApproval)
//! - [`PolicyInput`] - Context for policy evaluation (actor, target, capabilities)
//! - [`PolicyEngine::authorize`] - The main entry point for authorization
//!
//! # Actor Types
//!
//! - `Human` - Direct user interaction via CLI
//! - `Robot` - Programmatic access via robot mode
//! - `Mcp` - External tool via MCP protocol
//! - `Workflow` - Automated workflow execution

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

// ============================================================================
// Action Kinds
// ============================================================================

/// All action kinds that require policy authorization
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Send text to a pane
    SendText,
    /// Send Ctrl-C to a pane
    SendCtrlC,
    /// Send Ctrl-D to a pane
    SendCtrlD,
    /// Send Ctrl-Z to a pane
    SendCtrlZ,
    /// Send any control character
    SendControl,
    /// Spawn a new pane
    Spawn,
    /// Split a pane
    Split,
    /// Activate/focus a pane
    Activate,
    /// Close a pane
    Close,
    /// Browser-based authentication
    BrowserAuth,
    /// Start a workflow
    WorkflowRun,
    /// Reserve a pane for exclusive use
    ReservePane,
    /// Release a pane reservation
    ReleasePane,
    /// Read pane output
    ReadOutput,
    /// Search pane output
    SearchOutput,
    /// Write a file (future)
    WriteFile,
    /// Delete a file (future)
    DeleteFile,
    /// Execute external command (future)
    ExecCommand,
}

impl ActionKind {
    /// Returns true if this action modifies pane state
    #[must_use]
    pub const fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::SendText
                | Self::SendCtrlC
                | Self::SendCtrlD
                | Self::SendCtrlZ
                | Self::SendControl
                | Self::Spawn
                | Self::Split
                | Self::Close
        )
    }

    /// Returns true if this action is potentially destructive
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        matches!(
            self,
            Self::Close | Self::DeleteFile | Self::SendCtrlC | Self::SendCtrlD
        )
    }

    /// Returns a stable string identifier for this action kind
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SendText => "send_text",
            Self::SendCtrlC => "send_ctrl_c",
            Self::SendCtrlD => "send_ctrl_d",
            Self::SendCtrlZ => "send_ctrl_z",
            Self::SendControl => "send_control",
            Self::Spawn => "spawn",
            Self::Split => "split",
            Self::Activate => "activate",
            Self::Close => "close",
            Self::BrowserAuth => "browser_auth",
            Self::WorkflowRun => "workflow_run",
            Self::ReservePane => "reserve_pane",
            Self::ReleasePane => "release_pane",
            Self::ReadOutput => "read_output",
            Self::SearchOutput => "search_output",
            Self::WriteFile => "write_file",
            Self::DeleteFile => "delete_file",
            Self::ExecCommand => "exec_command",
        }
    }
}

// ============================================================================
// Actor Types
// ============================================================================

/// Who is requesting the action
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    /// Direct user interaction via CLI
    Human,
    /// Programmatic access via robot mode
    Robot,
    /// External tool via MCP protocol
    Mcp,
    /// Automated workflow execution
    Workflow,
}

impl ActorKind {
    /// Returns true if this actor has elevated trust
    #[must_use]
    pub const fn is_trusted(&self) -> bool {
        matches!(self, Self::Human)
    }

    /// Returns a stable string identifier for this actor kind
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Human => "human",
            Self::Robot => "robot",
            Self::Mcp => "mcp",
            Self::Workflow => "workflow",
        }
    }
}

// ============================================================================
// Pane Capabilities (stub - full impl in wa-4vx.8.8)
// ============================================================================

/// Pane capability snapshot for policy evaluation
///
/// This is a minimal stub. Full implementation in wa-4vx.8.8 will derive
/// these from OSC 133 markers, alt-screen detection, and heuristics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneCapabilities {
    /// Whether a shell prompt is currently active
    pub prompt_active: bool,
    /// Whether a command is currently running
    pub command_running: bool,
    /// Whether the pane is in alternate screen mode (vim, less, etc.)
    pub alt_screen: bool,
    /// Whether there's a recent capture gap
    pub has_recent_gap: bool,
    /// Whether the pane is reserved by another workflow
    pub is_reserved: bool,
    /// The workflow ID that has reserved this pane, if any
    pub reserved_by: Option<String>,
}

impl PaneCapabilities {
    /// Create capabilities for a pane with an active prompt
    #[must_use]
    pub fn prompt() -> Self {
        Self {
            prompt_active: true,
            ..Default::default()
        }
    }

    /// Create capabilities for a pane running a command
    #[must_use]
    pub fn running() -> Self {
        Self {
            command_running: true,
            ..Default::default()
        }
    }

    /// Create capabilities for an unknown/default state
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }
}

// ============================================================================
// Policy Decision
// ============================================================================

/// Result of policy evaluation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    /// Action is allowed
    Allow,
    /// Action is denied
    Deny {
        /// Human-readable reason for denial
        reason: String,
        /// Optional stable rule ID that triggered denial
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
    },
    /// Action requires explicit user approval
    RequireApproval {
        /// Human-readable reason why approval is needed
        reason: String,
        /// Optional stable rule ID that triggered approval requirement
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
    },
}

impl PolicyDecision {
    /// Create an Allow decision
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow
    }

    /// Create a Deny decision with a reason
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
            rule_id: None,
        }
    }

    /// Create a Deny decision with a reason and rule ID
    #[must_use]
    pub fn deny_with_rule(reason: impl Into<String>, rule_id: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
            rule_id: Some(rule_id.into()),
        }
    }

    /// Create a RequireApproval decision with a reason
    #[must_use]
    pub fn require_approval(reason: impl Into<String>) -> Self {
        Self::RequireApproval {
            reason: reason.into(),
            rule_id: None,
        }
    }

    /// Create a RequireApproval decision with a reason and rule ID
    #[must_use]
    pub fn require_approval_with_rule(
        reason: impl Into<String>,
        rule_id: impl Into<String>,
    ) -> Self {
        Self::RequireApproval {
            reason: reason.into(),
            rule_id: Some(rule_id.into()),
        }
    }

    /// Returns true if the action is allowed
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Returns true if the action is denied
    #[must_use]
    pub const fn is_denied(&self) -> bool {
        matches!(self, Self::Deny { .. })
    }

    /// Returns true if the action requires approval
    #[must_use]
    pub const fn requires_approval(&self) -> bool {
        matches!(self, Self::RequireApproval { .. })
    }

    /// Get the denial reason, if any
    #[must_use]
    pub fn denial_reason(&self) -> Option<&str> {
        match self {
            Self::Deny { reason, .. } => Some(reason),
            _ => None,
        }
    }

    /// Get the rule ID that triggered this decision, if any
    #[must_use]
    pub fn rule_id(&self) -> Option<&str> {
        match self {
            Self::Deny { rule_id, .. } | Self::RequireApproval { rule_id, .. } => {
                rule_id.as_deref()
            }
            Self::Allow => None,
        }
    }
}

// ============================================================================
// Policy Input
// ============================================================================

/// Input for policy evaluation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyInput {
    /// The action being requested
    pub action: ActionKind,
    /// Who is requesting the action
    pub actor: ActorKind,
    /// Target pane ID (if applicable)
    pub pane_id: Option<u64>,
    /// Target pane domain (if applicable)
    pub domain: Option<String>,
    /// Pane capabilities snapshot
    pub capabilities: PaneCapabilities,
    /// Optional redacted text summary for audit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_summary: Option<String>,
    /// Optional workflow ID (if action is from a workflow)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<String>,
}

impl PolicyInput {
    /// Create a new policy input
    #[must_use]
    pub fn new(action: ActionKind, actor: ActorKind) -> Self {
        Self {
            action,
            actor,
            pane_id: None,
            domain: None,
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
        }
    }

    /// Set the target pane
    #[must_use]
    pub fn with_pane(mut self, pane_id: u64) -> Self {
        self.pane_id = Some(pane_id);
        self
    }

    /// Set the target domain
    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set pane capabilities
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: PaneCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Set text summary for audit
    #[must_use]
    pub fn with_text_summary(mut self, summary: impl Into<String>) -> Self {
        self.text_summary = Some(summary.into());
        self
    }

    /// Set workflow ID
    #[must_use]
    pub fn with_workflow(mut self, workflow_id: impl Into<String>) -> Self {
        self.workflow_id = Some(workflow_id.into());
        self
    }
}

/// Rate limiter per pane
pub struct RateLimiter {
    /// Maximum operations per minute
    limit: u32,
    /// Tracking per pane
    pane_counts: HashMap<u64, Vec<Instant>>,
}

impl RateLimiter {
    /// Create a new rate limiter
    #[must_use]
    pub fn new(limit_per_minute: u32) -> Self {
        Self {
            limit: limit_per_minute,
            pane_counts: HashMap::new(),
        }
    }

    /// Check if operation is allowed for pane
    #[must_use]
    pub fn check(&mut self, pane_id: u64) -> bool {
        let now = Instant::now();
        let minute_ago = now
            .checked_sub(std::time::Duration::from_secs(60))
            .unwrap_or(now);

        let timestamps = self.pane_counts.entry(pane_id).or_default();

        // Remove old timestamps
        timestamps.retain(|t| *t > minute_ago);

        // Check if under limit
        if timestamps.len() < self.limit as usize {
            timestamps.push(now);
            true
        } else {
            false
        }
    }
}

// ============================================================================
// Policy Engine
// ============================================================================

/// Policy engine for authorizing actions
///
/// This is the central authorization point for all actions in wa.
/// Every action (send, workflow, MCP call) should go through `authorize()`.
pub struct PolicyEngine {
    /// Rate limiter
    rate_limiter: RateLimiter,
    /// Whether to require prompt active before mutating sends
    require_prompt_active: bool,
}

impl PolicyEngine {
    /// Create a new policy engine with default settings
    #[must_use]
    pub fn new(rate_limit: u32, require_prompt_active: bool) -> Self {
        Self {
            rate_limiter: RateLimiter::new(rate_limit),
            require_prompt_active,
        }
    }

    /// Create a policy engine with permissive defaults (for testing)
    #[must_use]
    pub fn permissive() -> Self {
        Self::new(1000, false)
    }

    /// Create a policy engine with strict defaults
    #[must_use]
    pub fn strict() -> Self {
        Self::new(30, true)
    }

    /// Authorize an action
    ///
    /// This is the main entry point for policy evaluation. All actions
    /// should be authorized through this method before execution.
    ///
    /// # Example
    ///
    /// ```
    /// use wa_core::policy::{PolicyEngine, PolicyInput, ActionKind, ActorKind, PaneCapabilities};
    ///
    /// let mut engine = PolicyEngine::permissive();
    /// let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
    ///     .with_pane(1)
    ///     .with_capabilities(PaneCapabilities::prompt());
    ///
    /// let decision = engine.authorize(&input);
    /// assert!(decision.is_allowed());
    /// ```
    pub fn authorize(&mut self, input: &PolicyInput) -> PolicyDecision {
        // Check rate limit for mutating actions
        if input.action.is_mutating() {
            if let Some(pane_id) = input.pane_id {
                if !self.rate_limiter.check(pane_id) {
                    return PolicyDecision::deny_with_rule(
                        "Rate limit exceeded",
                        "policy.rate_limit",
                    );
                }
            }
        }

        // Check prompt state for send actions
        if matches!(
            input.action,
            ActionKind::SendText | ActionKind::SendControl
        ) {
            if self.require_prompt_active && !input.capabilities.prompt_active {
                // If command is running, deny
                if input.capabilities.command_running {
                    return PolicyDecision::deny_with_rule(
                        "Refusing to send to running command - wait for prompt",
                        "policy.prompt_required",
                    );
                }
                // If state is unknown, require approval for non-trusted actors
                if !input.actor.is_trusted() {
                    return PolicyDecision::require_approval_with_rule(
                        "Pane state unknown - approval required before sending",
                        "policy.prompt_unknown",
                    );
                }
            }
        }

        // Check reservation conflicts
        if input.action.is_mutating() && input.capabilities.is_reserved {
            // Allow if this is the workflow that has the reservation
            if let (Some(reserved_by), Some(workflow_id)) =
                (&input.capabilities.reserved_by, &input.workflow_id)
            {
                if reserved_by == workflow_id {
                    return PolicyDecision::allow();
                }
            }
            // Otherwise deny
            return PolicyDecision::deny_with_rule(
                format!(
                    "Pane is reserved by workflow {}",
                    input
                        .capabilities
                        .reserved_by
                        .as_deref()
                        .unwrap_or("unknown")
                ),
                "policy.pane_reserved",
            );
        }

        // Destructive actions require approval for non-trusted actors
        if input.action.is_destructive() && !input.actor.is_trusted() {
            return PolicyDecision::require_approval_with_rule(
                format!("Destructive action '{}' requires approval", input.action.as_str()),
                "policy.destructive_action",
            );
        }

        PolicyDecision::allow()
    }

    /// Legacy: Check if send operation is allowed
    ///
    /// This is a compatibility shim. New code should use `authorize()`.
    #[must_use]
    #[deprecated(since = "0.2.0", note = "Use authorize() with PolicyInput instead")]
    pub fn check_send(&mut self, pane_id: u64, is_prompt_active: bool) -> PolicyDecision {
        let capabilities = if is_prompt_active {
            PaneCapabilities::prompt()
        } else {
            PaneCapabilities::running()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(pane_id)
            .with_capabilities(capabilities);

        self.authorize(&input)
    }

    /// Redact secrets from text
    #[must_use]
    pub fn redact_secrets(&self, text: &str) -> String {
        // TODO: Implement secret redaction patterns (wa-4vx.8.3)
        // For now, just pass through
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Rate Limiter Tests
    // ========================================================================

    #[test]
    fn rate_limiter_allows_under_limit() {
        let mut limiter = RateLimiter::new(10);
        assert!(limiter.check(1));
        assert!(limiter.check(1));
    }

    #[test]
    fn rate_limiter_denies_over_limit() {
        let mut limiter = RateLimiter::new(2);
        assert!(limiter.check(1));
        assert!(limiter.check(1));
        assert!(!limiter.check(1)); // Third request denied
    }

    #[test]
    fn rate_limiter_is_per_pane() {
        let mut limiter = RateLimiter::new(1);
        assert!(limiter.check(1));
        assert!(limiter.check(2)); // Different pane, allowed
        assert!(!limiter.check(1)); // Same pane, denied
    }

    // ========================================================================
    // ActionKind Tests
    // ========================================================================

    #[test]
    fn action_kind_mutating() {
        assert!(ActionKind::SendText.is_mutating());
        assert!(ActionKind::SendCtrlC.is_mutating());
        assert!(ActionKind::Close.is_mutating());
        assert!(!ActionKind::ReadOutput.is_mutating());
        assert!(!ActionKind::SearchOutput.is_mutating());
    }

    #[test]
    fn action_kind_destructive() {
        assert!(ActionKind::Close.is_destructive());
        assert!(ActionKind::DeleteFile.is_destructive());
        assert!(ActionKind::SendCtrlC.is_destructive());
        assert!(!ActionKind::SendText.is_destructive());
        assert!(!ActionKind::ReadOutput.is_destructive());
    }

    #[test]
    fn action_kind_stable_strings() {
        assert_eq!(ActionKind::SendText.as_str(), "send_text");
        assert_eq!(ActionKind::SendCtrlC.as_str(), "send_ctrl_c");
        assert_eq!(ActionKind::WorkflowRun.as_str(), "workflow_run");
    }

    // ========================================================================
    // PolicyDecision Tests
    // ========================================================================

    #[test]
    fn policy_decision_allow() {
        let decision = PolicyDecision::allow();
        assert!(decision.is_allowed());
        assert!(!decision.is_denied());
        assert!(!decision.requires_approval());
    }

    #[test]
    fn policy_decision_deny() {
        let decision = PolicyDecision::deny("test reason");
        assert!(!decision.is_allowed());
        assert!(decision.is_denied());
        assert_eq!(decision.denial_reason(), Some("test reason"));
        assert!(decision.rule_id().is_none());
    }

    #[test]
    fn policy_decision_deny_with_rule() {
        let decision = PolicyDecision::deny_with_rule("test reason", "test.rule");
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("test.rule"));
    }

    #[test]
    fn policy_decision_require_approval() {
        let decision = PolicyDecision::require_approval("needs approval");
        assert!(!decision.is_allowed());
        assert!(!decision.is_denied());
        assert!(decision.requires_approval());
    }

    // ========================================================================
    // PolicyEngine Authorization Tests
    // ========================================================================

    #[test]
    fn authorize_allows_read_operations() {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot);
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_allows_send_with_active_prompt() {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_denies_send_to_running_command() {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::running());
        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.prompt_required"));
    }

    #[test]
    fn authorize_requires_approval_for_unknown_state() {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::unknown());
        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("policy.prompt_unknown"));
    }

    #[test]
    fn authorize_allows_human_with_unknown_state() {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::unknown());
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_denies_reserved_pane() {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.is_reserved = true;
        caps.reserved_by = Some("other-workflow".to_string());

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(1)
            .with_capabilities(caps)
            .with_workflow("my-workflow");

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.pane_reserved"));
    }

    #[test]
    fn authorize_allows_owning_workflow_on_reserved_pane() {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.is_reserved = true;
        caps.reserved_by = Some("my-workflow".to_string());

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(1)
            .with_capabilities(caps)
            .with_workflow("my-workflow");

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_requires_approval_for_destructive_robot_actions() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot).with_pane(1);
        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("policy.destructive_action"));
    }

    #[test]
    fn authorize_allows_destructive_human_actions() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Human).with_pane(1);
        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_enforces_rate_limit() {
        let mut engine = PolicyEngine::new(1, false);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        assert!(engine.authorize(&input).is_allowed());
        assert!(engine.authorize(&input).is_denied()); // Rate limited
    }

    // ========================================================================
    // Serialization Tests
    // ========================================================================

    #[test]
    fn policy_decision_serializes_correctly() {
        let decision = PolicyDecision::deny_with_rule("test", "test.rule");
        let json = serde_json::to_string(&decision).unwrap();
        assert!(json.contains("\"decision\":\"deny\""));
        assert!(json.contains("\"rule_id\":\"test.rule\""));
    }

    #[test]
    fn policy_input_serializes_correctly() {
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(42)
            .with_domain("local");
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"action\":\"send_text\""));
        assert!(json.contains("\"actor\":\"robot\""));
        assert!(json.contains("\"pane_id\":42"));
    }
}
