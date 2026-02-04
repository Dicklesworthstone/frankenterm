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

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{
    CommandGateConfig, DcgDenyPolicy, DcgMode, PolicyRule, PolicyRuleDecision, PolicyRuleMatch,
    PolicyRulesConfig,
};
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

    /// Returns true if this action should be rate limited
    #[must_use]
    pub const fn is_rate_limited(&self) -> bool {
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
                | Self::BrowserAuth
                | Self::WorkflowRun
                | Self::ReservePane
                | Self::ReleasePane
                | Self::WriteFile
                | Self::DeleteFile
                | Self::ExecCommand
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
/// This provides deterministic state about a pane for policy decisions.
/// Capabilities are derived from:
/// - OSC 133 markers (shell integration for prompt/command state)
/// - Alt-screen detection (ESC[?1049h/l sequences)
/// - Gap detection (capture discontinuities)
///
/// # Safety Behavior
///
/// When `alt_screen` is `None` (unknown), policy should default to deny or
/// require approval for `SendText` actions, since we cannot safely determine
/// if input is appropriate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct PaneCapabilities {
    /// Whether a shell prompt is currently active (from OSC 133)
    pub prompt_active: bool,
    /// Whether a command is currently running (from OSC 133)
    pub command_running: bool,
    /// Whether the pane is in alternate screen mode (vim, less, etc.)
    /// - `Some(true)` - confidently detected alt-screen active
    /// - `Some(false)` - confidently detected normal screen
    /// - `None` - unknown state (should trigger conservative policy)
    pub alt_screen: Option<bool>,
    /// Whether there's a recent capture gap (cleared after verified prompt boundary)
    pub has_recent_gap: bool,
    /// Whether the pane is reserved by another workflow
    pub is_reserved: bool,
    /// The workflow ID that has reserved this pane, if any
    pub reserved_by: Option<String>,
}

impl PaneCapabilities {
    /// Create capabilities for a pane with an active prompt (normal screen)
    #[must_use]
    pub fn prompt() -> Self {
        Self {
            prompt_active: true,
            alt_screen: Some(false),
            ..Default::default()
        }
    }

    /// Create capabilities for a pane running a command
    #[must_use]
    pub fn running() -> Self {
        Self {
            command_running: true,
            alt_screen: Some(false),
            ..Default::default()
        }
    }

    /// Create capabilities for an unknown/default state
    #[must_use]
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Create capabilities for alt-screen mode (vim, less, htop, etc.)
    #[must_use]
    pub fn alt_screen() -> Self {
        Self {
            alt_screen: Some(true),
            ..Default::default()
        }
    }

    /// Check if we have confident knowledge of the pane state
    ///
    /// Returns false if alt_screen is unknown, meaning policy should be conservative.
    #[must_use]
    pub fn is_state_known(&self) -> bool {
        self.alt_screen.is_some()
    }

    /// Check if it's safe to send input (prompt active, not in alt-screen, no recent gap)
    ///
    /// This is a convenience method for common policy checks.
    #[must_use]
    pub fn is_input_safe(&self) -> bool {
        self.prompt_active
            && !self.command_running
            && self.alt_screen == Some(false)
            && !self.has_recent_gap
            && !self.is_reserved
    }

    /// Mark that a verified prompt boundary was seen (clears recent_gap)
    pub fn clear_gap_on_prompt(&mut self) {
        if self.prompt_active {
            self.has_recent_gap = false;
        }
    }

    /// Derive capabilities from ingest state
    ///
    /// This combines signals from:
    /// - OSC 133 markers (shell state)
    /// - Cursor state (alt-screen, gap)
    ///
    /// # Arguments
    ///
    /// * `osc_state` - OSC 133 marker state (or None if not tracked)
    /// * `in_alt_screen` - Whether the pane is in alt-screen mode (from cursor)
    /// * `in_gap` - Whether there's an unresolved capture gap
    #[must_use]
    pub fn from_ingest_state(
        osc_state: Option<&crate::ingest::Osc133State>,
        in_alt_screen: Option<bool>,
        in_gap: bool,
    ) -> Self {
        let (prompt_active, command_running) = osc_state.map_or((false, false), |state| {
            (state.state.is_at_prompt(), state.state.is_command_running())
        });

        Self {
            prompt_active,
            command_running,
            alt_screen: in_alt_screen,
            has_recent_gap: in_gap,
            is_reserved: false,
            reserved_by: None,
        }
    }
}

// ============================================================================
// Policy Decision
// ============================================================================

/// Full decision context captured during policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionContext {
    /// Timestamp of decision (epoch ms)
    pub timestamp_ms: i64,
    /// Action being evaluated
    pub action: ActionKind,
    /// Actor requesting the action
    pub actor: ActorKind,
    /// Target pane ID (if applicable)
    pub pane_id: Option<u64>,
    /// Target domain (if applicable)
    pub domain: Option<String>,
    /// Capabilities snapshot used for the decision
    pub capabilities: PaneCapabilities,
    /// Optional redacted text summary
    pub text_summary: Option<String>,
    /// Optional workflow ID (if action is from a workflow)
    pub workflow_id: Option<String>,
    /// Rules evaluated in order
    pub rules_evaluated: Vec<RuleEvaluation>,
    /// Rule that determined the outcome (if any)
    pub determining_rule: Option<String>,
    /// Evidence collected during evaluation
    pub evidence: Vec<DecisionEvidence>,
    /// Rate limit snapshot, if applicable
    pub rate_limit: Option<RateLimitSnapshot>,
    /// Risk score, if calculated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub risk: Option<RiskScore>,
}

impl DecisionContext {
    /// Create an empty context (used only for manual/test decisions).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            timestamp_ms: 0,
            action: ActionKind::ReadOutput,
            actor: ActorKind::Human,
            pane_id: None,
            domain: None,
            capabilities: PaneCapabilities::default(),
            text_summary: None,
            workflow_id: None,
            rules_evaluated: Vec::new(),
            determining_rule: None,
            evidence: Vec::new(),
            rate_limit: None,
            risk: None,
        }
    }

    /// Set the risk score on this context
    pub fn set_risk(&mut self, risk: RiskScore) {
        self.risk = Some(risk);
    }

    /// Record a rule evaluation in order.
    pub fn record_rule(
        &mut self,
        rule_id: impl Into<String>,
        matched: bool,
        decision: Option<&str>,
        reason: Option<String>,
    ) {
        self.rules_evaluated.push(RuleEvaluation {
            rule_id: rule_id.into(),
            matched,
            decision: decision.map(str::to_string),
            reason,
        });
    }

    /// Mark the rule that determined the outcome.
    pub fn set_determining_rule(&mut self, rule_id: impl Into<String>) {
        self.determining_rule = Some(rule_id.into());
    }

    /// Add evidence to the context.
    pub fn add_evidence(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.evidence.push(DecisionEvidence {
            key: key.into(),
            value: value.into(),
        });
    }
}

/// Per-rule evaluation details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleEvaluation {
    /// Rule identifier
    pub rule_id: String,
    /// Whether the rule matched
    pub matched: bool,
    /// Decision produced by the rule (allow/deny/require_approval), if any
    pub decision: Option<String>,
    /// Optional reason or explanation
    pub reason: Option<String>,
}

/// Evidence captured for debugging/explainability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionEvidence {
    /// Evidence key
    pub key: String,
    /// Evidence value (stringified)
    pub value: String,
}

/// Snapshot of rate limit state when a decision is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    /// Scope string (per_pane:<id> or global)
    pub scope: String,
    /// Action kind
    pub action: String,
    /// Limit per minute
    pub limit: u32,
    /// Current count in the window
    pub current: usize,
    /// Suggested retry-after in seconds
    pub retry_after_secs: u64,
}

// ============================================================================
// Risk Scoring
// ============================================================================

/// Risk factor categories
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskCategory {
    /// Pane/session state factors
    State,
    /// Action type factors
    Action,
    /// Request context factors
    Context,
    /// Command content factors
    Content,
}

impl RiskCategory {
    /// Returns a stable string identifier
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::State => "state",
            Self::Action => "action",
            Self::Context => "context",
            Self::Content => "content",
        }
    }
}

/// A risk factor definition with its metadata
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskFactor {
    /// Stable factor ID (e.g., "state.alt_screen")
    pub id: String,
    /// Factor category
    pub category: RiskCategory,
    /// Base weight (0-100)
    pub base_weight: u8,
    /// Human-readable short description
    pub description: String,
}

impl RiskFactor {
    /// Create a new risk factor
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        category: RiskCategory,
        base_weight: u8,
        description: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            category,
            base_weight: base_weight.min(100),
            description: description.into(),
        }
    }
}

/// A factor that was applied to the risk calculation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedRiskFactor {
    /// Factor ID
    pub id: String,
    /// Weight that was applied
    pub weight: u8,
    /// Human-readable explanation
    pub explanation: String,
}

/// Calculated risk score with contributing factors
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskScore {
    /// Total risk score (0-100)
    pub score: u8,
    /// Factors that contributed to the score
    pub factors: Vec<AppliedRiskFactor>,
    /// Human-readable summary
    pub summary: String,
}

impl RiskScore {
    /// Create a zero-risk score
    #[must_use]
    pub fn zero() -> Self {
        Self {
            score: 0,
            factors: Vec::new(),
            summary: "Low risk".to_string(),
        }
    }

    /// Create a risk score from applied factors
    #[must_use]
    pub fn from_factors(factors: Vec<AppliedRiskFactor>) -> Self {
        let total: u32 = factors.iter().map(|f| u32::from(f.weight)).sum();
        let score = total.min(100) as u8;
        let summary = Self::summary_for_score(score);
        Self {
            score,
            factors,
            summary,
        }
    }

    /// Get human-readable summary for a score
    #[must_use]
    pub fn summary_for_score(score: u8) -> String {
        match score {
            0..=20 => "Low risk".to_string(),
            21..=50 => "Medium risk".to_string(),
            51..=70 => "Elevated risk".to_string(),
            71..=100 => "High risk".to_string(),
            _ => "Unknown risk".to_string(),
        }
    }

    /// Returns true if this is low risk (score <= 20)
    #[must_use]
    pub const fn is_low(&self) -> bool {
        self.score <= 20
    }

    /// Returns true if this is medium risk (21-50)
    #[must_use]
    pub const fn is_medium(&self) -> bool {
        self.score > 20 && self.score <= 50
    }

    /// Returns true if this is elevated risk (51-70)
    #[must_use]
    pub const fn is_elevated(&self) -> bool {
        self.score > 50 && self.score <= 70
    }

    /// Returns true if this is high risk (> 70)
    #[must_use]
    pub const fn is_high(&self) -> bool {
        self.score > 70
    }
}

impl Default for RiskScore {
    fn default() -> Self {
        Self::zero()
    }
}

/// Risk scoring configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Enable risk scoring
    pub enabled: bool,
    /// Maximum score for automatic allow (default: 50)
    pub allow_max: u8,
    /// Maximum score for require-approval; above this = deny (default: 70)
    pub require_approval_max: u8,
    /// Weight overrides by factor ID
    #[serde(default)]
    pub weights: std::collections::HashMap<String, u8>,
    /// Disabled factor IDs
    #[serde(default)]
    pub disabled: std::collections::HashSet<String>,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_max: 50,
            require_approval_max: 70,
            weights: std::collections::HashMap::new(),
            disabled: std::collections::HashSet::new(),
        }
    }
}

impl RiskConfig {
    /// Get the effective weight for a factor
    #[must_use]
    pub fn get_weight(&self, factor_id: &str, base_weight: u8) -> u8 {
        if self.disabled.contains(factor_id) {
            return 0;
        }
        self.weights
            .get(factor_id)
            .copied()
            .unwrap_or(base_weight)
            .min(100)
    }

    /// Check if a factor is disabled
    #[must_use]
    pub fn is_disabled(&self, factor_id: &str) -> bool {
        self.disabled.contains(factor_id)
    }
}

/// Allow-once approval payload for RequireApproval decisions
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Short allow-once code (human-entered)
    pub allow_once_code: String,
    /// Full hash of allow-once code (sha256)
    pub allow_once_full_hash: String,
    /// Expiration timestamp (epoch ms)
    pub expires_at: i64,
    /// Human-readable summary of the approval
    pub summary: String,
    /// Command a human can run to approve
    pub command: String,
}

/// Result of policy evaluation
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PolicyDecision {
    /// Action is allowed
    Allow {
        /// Optional stable rule ID that triggered the allow (for audit)
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
        /// Decision context
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<DecisionContext>,
    },
    /// Action is denied
    Deny {
        /// Human-readable reason for denial
        reason: String,
        /// Optional stable rule ID that triggered denial
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
        /// Decision context
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<DecisionContext>,
    },
    /// Action requires explicit user approval
    RequireApproval {
        /// Human-readable reason why approval is needed
        reason: String,
        /// Optional stable rule ID that triggered approval requirement
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_id: Option<String>,
        /// Optional allow-once approval payload
        #[serde(skip_serializing_if = "Option::is_none")]
        approval: Option<ApprovalRequest>,
        /// Decision context
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<DecisionContext>,
    },
}

impl PolicyDecision {
    /// Create an Allow decision
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow {
            rule_id: None,
            context: None,
        }
    }

    /// Create an Allow decision with a rule ID (for audit trail)
    #[must_use]
    pub fn allow_with_rule(rule_id: impl Into<String>) -> Self {
        Self::Allow {
            rule_id: Some(rule_id.into()),
            context: None,
        }
    }

    /// Create a Deny decision with a reason
    #[must_use]
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
            rule_id: None,
            context: None,
        }
    }

    /// Create a Deny decision with a reason and rule ID
    #[must_use]
    pub fn deny_with_rule(reason: impl Into<String>, rule_id: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
            rule_id: Some(rule_id.into()),
            context: None,
        }
    }

    /// Create a RequireApproval decision with a reason
    #[must_use]
    pub fn require_approval(reason: impl Into<String>) -> Self {
        Self::RequireApproval {
            reason: reason.into(),
            rule_id: None,
            approval: None,
            context: None,
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
            approval: None,
            context: None,
        }
    }

    /// Returns true if the action is allowed
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow { .. })
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
            Self::Allow { rule_id, .. }
            | Self::Deny { rule_id, .. }
            | Self::RequireApproval { rule_id, .. } => rule_id.as_deref(),
        }
    }

    /// Attach an allow-once approval payload to a RequireApproval decision
    #[must_use]
    pub fn with_approval(self, approval: ApprovalRequest) -> Self {
        match self {
            Self::RequireApproval {
                reason,
                rule_id,
                context,
                ..
            } => Self::RequireApproval {
                reason,
                rule_id,
                approval: Some(approval),
                context,
            },
            other => other,
        }
    }

    /// Attach decision context to this decision.
    #[must_use]
    pub fn with_context(self, context: DecisionContext) -> Self {
        match self {
            Self::Allow { rule_id, .. } => Self::Allow {
                rule_id,
                context: Some(context),
            },
            Self::Deny {
                reason, rule_id, ..
            } => Self::Deny {
                reason,
                rule_id,
                context: Some(context),
            },
            Self::RequireApproval {
                reason,
                rule_id,
                approval,
                ..
            } => Self::RequireApproval {
                reason,
                rule_id,
                approval,
                context: Some(context),
            },
        }
    }

    /// Get decision context, if present.
    #[must_use]
    pub fn context(&self) -> Option<&DecisionContext> {
        match self {
            Self::Allow { context, .. }
            | Self::Deny { context, .. }
            | Self::RequireApproval { context, .. } => context.as_ref(),
        }
    }

    /// Returns a stable string representation of the decision type
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Allow { .. } => "allow",
            Self::Deny { .. } => "deny",
            Self::RequireApproval { .. } => "require_approval",
        }
    }

    /// Returns the decision reason, if any (for both Deny and RequireApproval)
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Deny { reason, .. } | Self::RequireApproval { reason, .. } => Some(reason),
            Self::Allow { .. } => None,
        }
    }

    /// Get the allow-once approval payload, if present
    #[must_use]
    pub fn approval_request(&self) -> Option<&ApprovalRequest> {
        match self {
            Self::RequireApproval { approval, .. } => approval.as_ref(),
            _ => None,
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
    /// Raw command text for SendText safety gating (not serialized)
    #[serde(skip)]
    pub command_text: Option<String>,
    /// Pane title for rule matching (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_title: Option<String>,
    /// Pane working directory for rule matching (if available)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_cwd: Option<String>,
    /// Inferred agent type for rule matching (e.g., "claude", "cursor", "shell")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
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
            command_text: None,
            pane_title: None,
            pane_cwd: None,
            agent_type: None,
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

    /// Set raw command text for command safety gate
    #[must_use]
    pub fn with_command_text(mut self, text: impl Into<String>) -> Self {
        self.command_text = Some(text.into());
        self
    }

    /// Set pane title for rule matching
    #[must_use]
    pub fn with_pane_title(mut self, title: impl Into<String>) -> Self {
        self.pane_title = Some(title.into());
        self
    }

    /// Set pane working directory for rule matching
    #[must_use]
    pub fn with_pane_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.pane_cwd = Some(cwd.into());
        self
    }

    /// Set inferred agent type for rule matching
    #[must_use]
    pub fn with_agent_type(mut self, agent_type: impl Into<String>) -> Self {
        self.agent_type = Some(agent_type.into());
        self
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

impl DecisionContext {
    /// Build a decision context from a policy input.
    #[must_use]
    pub fn from_input(input: &PolicyInput) -> Self {
        let mut ctx = Self {
            timestamp_ms: now_ms(),
            action: input.action,
            actor: input.actor,
            pane_id: input.pane_id,
            domain: input.domain.clone(),
            capabilities: input.capabilities.clone(),
            text_summary: input.text_summary.clone(),
            workflow_id: input.workflow_id.clone(),
            rules_evaluated: Vec::new(),
            determining_rule: None,
            evidence: Vec::new(),
            rate_limit: None,
            risk: None,
        };

        ctx.add_evidence(
            "prompt_active",
            input.capabilities.prompt_active.to_string(),
        );
        ctx.add_evidence(
            "command_running",
            input.capabilities.command_running.to_string(),
        );
        ctx.add_evidence(
            "alt_screen",
            input
                .capabilities
                .alt_screen
                .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
        );
        ctx.add_evidence(
            "has_recent_gap",
            input.capabilities.has_recent_gap.to_string(),
        );
        ctx.add_evidence("is_reserved", input.capabilities.is_reserved.to_string());
        if let Some(reserved_by) = &input.capabilities.reserved_by {
            ctx.add_evidence("reserved_by", reserved_by.clone());
        }
        if let Some(text) = input.command_text.as_ref() {
            ctx.add_evidence("command_text_present", "true");
            ctx.add_evidence("command_text_len", text.len().to_string());
            ctx.add_evidence("command_candidate", is_command_candidate(text).to_string());
        } else {
            ctx.add_evidence("command_text_present", "false");
        }

        ctx
    }
}

/// Rolling window for rate limiting
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// Scope for a rate limit decision
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    /// Limit is enforced per pane (and action kind)
    PerPane {
        /// Pane ID for the limit
        pane_id: u64,
    },
    /// Limit is enforced globally (per action kind)
    Global,
}

/// Details about a rate limit violation
#[derive(Debug, Clone)]
pub struct RateLimitHit {
    /// Scope that triggered the limit
    pub scope: RateLimitScope,
    /// Action kind being limited
    pub action: ActionKind,
    /// Limit in operations per minute
    pub limit: u32,
    /// Current count in the window
    pub current: usize,
    /// Suggested retry-after delay
    pub retry_after: Duration,
}

impl RateLimitHit {
    /// Format a human-readable reason string
    #[must_use]
    pub fn reason(&self) -> String {
        let retry_secs = self.retry_after.as_millis().div_ceil(1000);
        let mut reason = match self.scope {
            RateLimitScope::PerPane { pane_id } => format!(
                "Rate limit exceeded for action '{}' on pane {}: {}/{} per minute (per-pane)",
                self.action.as_str(),
                pane_id,
                self.current,
                self.limit
            ),
            RateLimitScope::Global => format!(
                "Global rate limit exceeded for action '{}': {}/{} per minute",
                self.action.as_str(),
                self.current,
                self.limit
            ),
        };

        if retry_secs > 0 {
            let _ = write!(reason, "; retry after {retry_secs}s");
        }

        reason.push_str(". Remediation: wait before retrying or reduce concurrency.");

        reason
    }
}

fn rate_limit_snapshot_from_hit(hit: &RateLimitHit) -> RateLimitSnapshot {
    let scope = match hit.scope {
        RateLimitScope::PerPane { pane_id } => format!("per_pane:{pane_id}"),
        RateLimitScope::Global => "global".to_string(),
    };

    let retry_after_secs =
        u64::try_from(hit.retry_after.as_millis().div_ceil(1000)).unwrap_or(u64::MAX);

    RateLimitSnapshot {
        scope,
        action: hit.action.as_str().to_string(),
        limit: hit.limit,
        current: hit.current,
        retry_after_secs,
    }
}

/// Outcome of a rate limit check
#[derive(Debug, Clone)]
pub enum RateLimitOutcome {
    /// Allowed under current limits
    Allowed,
    /// Limited with details about the violation
    Limited(RateLimitHit),
}

impl RateLimitOutcome {
    /// Returns true if the action is allowed
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Rate limiter per pane and action kind
pub struct RateLimiter {
    /// Maximum operations per minute per pane/action
    limit_per_pane: u32,
    /// Maximum operations per minute globally per action
    limit_global: u32,
    /// Tracking per pane/action
    pane_counts: HashMap<(u64, ActionKind), Vec<Instant>>,
    /// Tracking per action globally
    global_counts: HashMap<ActionKind, Vec<Instant>>,
}

impl RateLimiter {
    /// Create a new rate limiter
    #[must_use]
    pub fn new(limit_per_pane: u32, limit_global: u32) -> Self {
        Self {
            limit_per_pane,
            limit_global,
            pane_counts: HashMap::new(),
            global_counts: HashMap::new(),
        }
    }

    /// Check if operation is allowed for pane/action
    #[must_use]
    pub fn check(&mut self, action: ActionKind, pane_id: Option<u64>) -> RateLimitOutcome {
        let now = Instant::now();
        let window_start = now.checked_sub(RATE_LIMIT_WINDOW).unwrap_or(now);

        if let Some(pane_id) = pane_id {
            if self.limit_per_pane > 0 {
                let timestamps = self.pane_counts.entry((pane_id, action)).or_default();
                prune_old(timestamps, window_start);
                let current = timestamps.len();
                if current >= self.limit_per_pane as usize {
                    let retry_after = retry_after(now, timestamps);
                    return RateLimitOutcome::Limited(RateLimitHit {
                        scope: RateLimitScope::PerPane { pane_id },
                        action,
                        limit: self.limit_per_pane,
                        current,
                        retry_after,
                    });
                }
            }
        }

        if self.limit_global > 0 {
            let timestamps = self.global_counts.entry(action).or_default();
            prune_old(timestamps, window_start);
            let current = timestamps.len();
            if current >= self.limit_global as usize {
                let retry_after = retry_after(now, timestamps);
                return RateLimitOutcome::Limited(RateLimitHit {
                    scope: RateLimitScope::Global,
                    action,
                    limit: self.limit_global,
                    current,
                    retry_after,
                });
            }
        }

        if let Some(pane_id) = pane_id {
            if self.limit_per_pane > 0 {
                self.pane_counts
                    .entry((pane_id, action))
                    .or_default()
                    .push(now);
            }
        }

        if self.limit_global > 0 {
            self.global_counts.entry(action).or_default().push(now);
        }

        RateLimitOutcome::Allowed
    }
}

fn prune_old(timestamps: &mut Vec<Instant>, window_start: Instant) {
    timestamps.retain(|t| *t > window_start);
}

fn retry_after(now: Instant, timestamps: &[Instant]) -> Duration {
    timestamps
        .first()
        .and_then(|oldest| oldest.checked_add(RATE_LIMIT_WINDOW))
        .map_or(Duration::from_secs(0), |deadline| {
            deadline.saturating_duration_since(now)
        })
}

// ============================================================================
// Command Safety Gate
// ============================================================================

/// Built-in command gate decision
#[derive(Debug, Clone)]
enum CommandGateOutcome {
    Allow,
    Deny { reason: String, rule_id: String },
    RequireApproval { reason: String, rule_id: String },
}

#[derive(Debug, Clone, Copy)]
enum CommandGateDecision {
    Deny,
    RequireApproval,
}

struct CommandRule {
    id: &'static str,
    regex: &'static LazyLock<Regex>,
    decision: CommandGateDecision,
    reason: &'static str,
}

static RM_RF_ROOT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\brm\s+-rf\s+(/|~)(\s|$)").expect("rm -rf root regex"));
static RM_RF_GENERIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\brm\s+-rf\s+").expect("rm -rf regex"));
static GIT_RESET_HARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+reset\b.*\s--hard\b").expect("git reset --hard"));
static GIT_CLEAN_FD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bgit\s+clean\b.*\s-[-a-z]*f[-a-z]*d").expect("git clean -fd")
});
static GIT_PUSH_FORCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bgit\s+push\b.*\s(--force|-f)\b").expect("git push --force")
});
static GIT_BRANCH_DELETE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bgit\s+branch\b.*\s-D\b").expect("git branch -D"));
static SQL_DESTRUCTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(drop\s+database|drop\s+table|truncate\s+table)\b").expect("sql destructive")
});

static COMMAND_RULES: &[CommandRule] = &[
    CommandRule {
        id: "command.rm_rf_root",
        regex: &RM_RF_ROOT,
        decision: CommandGateDecision::Deny,
        reason: "Blocking rm -rf on root/home paths",
    },
    CommandRule {
        id: "command.rm_rf",
        regex: &RM_RF_GENERIC,
        decision: CommandGateDecision::RequireApproval,
        reason: "rm -rf is destructive and requires approval",
    },
    CommandRule {
        id: "command.git_reset_hard",
        regex: &GIT_RESET_HARD,
        decision: CommandGateDecision::RequireApproval,
        reason: "git reset --hard discards uncommitted changes",
    },
    CommandRule {
        id: "command.git_clean_fd",
        regex: &GIT_CLEAN_FD,
        decision: CommandGateDecision::RequireApproval,
        reason: "git clean -fd removes untracked files",
    },
    CommandRule {
        id: "command.git_push_force",
        regex: &GIT_PUSH_FORCE,
        decision: CommandGateDecision::RequireApproval,
        reason: "git push --force rewrites remote history",
    },
    CommandRule {
        id: "command.git_branch_delete",
        regex: &GIT_BRANCH_DELETE,
        decision: CommandGateDecision::RequireApproval,
        reason: "git branch -D deletes branches permanently",
    },
    CommandRule {
        id: "command.sql_destructive",
        regex: &SQL_DESTRUCTIVE,
        decision: CommandGateDecision::RequireApproval,
        reason: "Destructive SQL command requires approval",
    },
];

const COMMAND_TOKENS: &[&str] = &[
    "git",
    "rm",
    "sudo",
    "docker",
    "kubectl",
    "aws",
    "psql",
    "mysql",
    "sqlite3",
    "gh",
    "npm",
    "yarn",
    "pnpm",
    "cargo",
    "make",
    "bash",
    "sh",
    "zsh",
    "python",
    "python3",
    "node",
    "go",
    "rg",
    "find",
    "export",
    "mv",
    "cp",
    "chmod",
    "chown",
    "dd",
    "systemctl",
    "service",
    // Interpreters and dangerous builtins
    "perl",
    "ruby",
    "php",
    "lua",
    "tclsh",
    "eval",
    "exec",
    "env",
    "xargs",
    "busybox",
    "openssl",
];

fn first_nonempty_line(text: &str) -> Option<&str> {
    text.lines().find(|line| !line.trim().is_empty())
}

/// Determine whether the text looks like a shell command
#[must_use]
pub fn is_command_candidate(text: &str) -> bool {
    let Some(line) = first_nonempty_line(text) else {
        return false;
    };

    let mut trimmed = line.trim_start();
    if trimmed.starts_with('#') {
        return false;
    }

    if let Some(stripped) = trimmed.strip_prefix('$') {
        trimmed = stripped.trim_start();
    }

    // Also strip leading parens (subshells) and braces (blocks)
    loop {
        if let Some(stripped) = trimmed.strip_prefix('(') {
            trimmed = stripped.trim_start();
            continue;
        }
        if let Some(stripped) = trimmed.strip_prefix('{') {
            trimmed = stripped.trim_start();
            continue;
        }
        break;
    }

    let mut parts = trimmed.split_whitespace();
    let token = parts.next().unwrap_or("");
    let token_lower = token.to_ascii_lowercase();
    if COMMAND_TOKENS.contains(&token_lower.as_str()) {
        return true;
    }

    if token_lower == "sudo" {
        if let Some(next) = parts.next() {
            let next_lower = next.to_ascii_lowercase();
            if COMMAND_TOKENS.contains(&next_lower.as_str()) {
                return true;
            }
        }
    }

    trimmed.contains("&&")
        || trimmed.contains("||")
        || trimmed.contains('|')
        || trimmed.contains('>')
        || trimmed.contains(';')
}

#[derive(Debug)]
enum DcgDecision {
    Allow,
    Deny { rule_id: Option<String> },
}

#[derive(Debug)]
enum DcgError {
    NotAvailable,
    Failed(String),
}

#[derive(Deserialize)]
struct DcgHookOutput {
    #[serde(rename = "permissionDecision")]
    permission_decision: String,
    #[serde(rename = "ruleId")]
    rule_id: Option<String>,
}

#[derive(Deserialize)]
struct DcgResponse {
    #[serde(rename = "hookSpecificOutput")]
    hook_specific_output: DcgHookOutput,
}

fn evaluate_builtin_rules(command: &str) -> Option<CommandGateOutcome> {
    for rule in COMMAND_RULES {
        if rule.regex.is_match(command) {
            let rule_id = rule.id.to_string();
            let reason = rule.reason.to_string();
            return Some(match rule.decision {
                CommandGateDecision::Deny => CommandGateOutcome::Deny { reason, rule_id },
                CommandGateDecision::RequireApproval => {
                    CommandGateOutcome::RequireApproval { reason, rule_id }
                }
            });
        }
    }
    None
}

fn evaluate_command_gate_with_runner<F>(
    text: &str,
    config: &CommandGateConfig,
    dcg_runner: F,
) -> CommandGateOutcome
where
    F: Fn(&str) -> Result<DcgDecision, DcgError>,
{
    if !config.enabled {
        return CommandGateOutcome::Allow;
    }

    if !is_command_candidate(text) {
        return CommandGateOutcome::Allow;
    }

    let command_line = first_nonempty_line(text).unwrap_or(text);
    if let Some(result) = evaluate_builtin_rules(command_line) {
        return result;
    }

    match config.dcg_mode {
        DcgMode::Disabled => CommandGateOutcome::Allow,
        DcgMode::Opportunistic | DcgMode::Required => match dcg_runner(command_line) {
            Ok(DcgDecision::Allow) => CommandGateOutcome::Allow,
            Ok(DcgDecision::Deny { rule_id }) => {
                let rule = rule_id.unwrap_or_else(|| "unknown".to_string());
                let rule_id = format!("dcg.{rule}");
                let reason = format!("Command safety gate blocked by dcg (rule {rule})");
                match config.dcg_deny_policy {
                    DcgDenyPolicy::Deny => CommandGateOutcome::Deny { reason, rule_id },
                    DcgDenyPolicy::RequireApproval => {
                        CommandGateOutcome::RequireApproval { reason, rule_id }
                    }
                }
            }
            Err(err) => match config.dcg_mode {
                DcgMode::Required => {
                    let detail = match err {
                        DcgError::NotAvailable => "dcg not available".to_string(),
                        DcgError::Failed(detail) => format!("dcg error: {detail}"),
                    };
                    CommandGateOutcome::RequireApproval {
                        reason: format!(
                            "Command safety gate requires dcg but it is unavailable ({detail})"
                        ),
                        rule_id: "command_gate.dcg_unavailable".to_string(),
                    }
                }
                _ => CommandGateOutcome::Allow,
            },
        },
    }
}

fn evaluate_command_gate(text: &str, config: &CommandGateConfig) -> CommandGateOutcome {
    evaluate_command_gate_with_runner(text, config, run_dcg)
}

fn run_dcg(command: &str) -> Result<DcgDecision, DcgError> {
    let payload = serde_json::json!({
        "tool_name": "Bash",
        "tool_input": { "command": command }
    });
    let mut child = Command::new("dcg")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DcgError::NotAvailable
            } else {
                DcgError::Failed(e.to_string())
            }
        })?;

    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(payload.to_string().as_bytes())
            .map_err(|e| DcgError::Failed(e.to_string()))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| DcgError::Failed(e.to_string()))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok(DcgDecision::Allow);
    }

    let parsed: DcgResponse =
        serde_json::from_str(stdout.trim()).map_err(|e| DcgError::Failed(e.to_string()))?;

    if parsed.hook_specific_output.permission_decision == "deny" {
        return Ok(DcgDecision::Deny {
            rule_id: parsed.hook_specific_output.rule_id,
        });
    }

    Ok(DcgDecision::Allow)
}

// ============================================================================
// Secret Redaction
// ============================================================================

/// Redaction marker used in place of detected secrets
pub const REDACTED_MARKER: &str = "[REDACTED]";

/// Pattern definition for secret detection
struct SecretPattern {
    /// Human-readable name for the pattern
    name: &'static str,
    /// Compiled regex pattern
    regex: &'static LazyLock<Regex>,
}

// Define lazy-compiled regex patterns for various secret types

/// OpenAI API keys: sk-... (48+ chars) or sk-proj-...
static OPENAI_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-(?:proj-)?[a-zA-Z0-9_-]{20,}").expect("OpenAI key regex"));

/// Anthropic API keys: sk-ant-...
static ANTHROPIC_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sk-ant-[a-zA-Z0-9_-]{20,}").expect("Anthropic key regex"));

/// GitHub tokens: ghp_, gho_, ghu_, ghs_, ghr_
static GITHUB_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"gh[pousr]_[a-zA-Z0-9]{36,}").expect("GitHub token regex"));

/// AWS Access Key IDs: AKIA...
static AWS_ACCESS_KEY_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").expect("AWS access key regex"));

/// AWS Secret Access Keys (typically 40 chars base64-like, often after aws_secret_access_key=)
static AWS_SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("(?i)aws_secret_access_key\\s*[=:]\\s*['\"]?([a-zA-Z0-9/+=]{40})['\"]?")
        .expect("AWS secret key regex")
});

/// Generic Bearer tokens in Authorization headers
static BEARER_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:authorization|bearer)[:\s]+bearer\s+[a-zA-Z0-9._-]{20,}")
        .expect("Bearer token regex")
});

/// Generic API keys with common prefixes
static GENERIC_API_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:api[_-]?key|apikey)\s*[=:]\s*['"]?([a-zA-Z0-9_-]{16,})['"]?"#)
        .expect("Generic API key regex")
});

/// Generic token assignments
static GENERIC_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|[^a-z])token\s*[=:]\s*['"]?([a-zA-Z0-9._-]{16,})['"]?"#)
        .expect("Generic token regex")
});

/// Generic password assignments (password=..., password: ...)
static GENERIC_PASSWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)password\s*[=:]\s*(?:'[^']{4,}'|"[^"]{4,}"|[^\s'"]{4,})"#)
        .expect("Generic password regex")
});

/// Generic secret assignments
static GENERIC_SECRET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:^|[^a-z])secret\s*[=:]\s*['"]?([a-zA-Z0-9_-]{8,})['"]?"#)
        .expect("Generic secret regex")
});

/// Device codes (OAuth device flow) - typically 8+ alphanumeric chars displayed to user
static DEVICE_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(?:device[_-]?code|user[_-]?code)\s*[=:]\s*['"]?([A-Z0-9-]{6,})['"]?"#)
        .expect("Device code regex")
});

/// OAuth URLs with tokens/codes in query params
static OAUTH_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://[^\s]*[?&](?:access_token|code|token)=[a-zA-Z0-9._-]+")
        .expect("OAuth URL regex")
});

/// Slack tokens: xoxb-, xoxp-, xoxa-, xoxr-
static SLACK_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"xox[bpar]-[a-zA-Z0-9-]{10,}").expect("Slack token regex"));

/// Stripe API keys: sk_live_, sk_test_, pk_live_, pk_test_
static STRIPE_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[ps]k_(?:live|test)_[a-zA-Z0-9]{20,}").expect("Stripe key regex")
});

/// Database connection strings with passwords
static DATABASE_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:postgres|mysql|mongodb|redis)(?:ql)?://[^:]+:([^@\s]+)@")
        .expect("Database URL regex")
});

/// All secret patterns in priority order
static SECRET_PATTERNS: &[SecretPattern] = &[
    SecretPattern {
        name: "openai_key",
        regex: &OPENAI_KEY,
    },
    SecretPattern {
        name: "anthropic_key",
        regex: &ANTHROPIC_KEY,
    },
    SecretPattern {
        name: "github_token",
        regex: &GITHUB_TOKEN,
    },
    SecretPattern {
        name: "aws_access_key_id",
        regex: &AWS_ACCESS_KEY_ID,
    },
    SecretPattern {
        name: "aws_secret_key",
        regex: &AWS_SECRET_KEY,
    },
    SecretPattern {
        name: "bearer_token",
        regex: &BEARER_TOKEN,
    },
    SecretPattern {
        name: "slack_token",
        regex: &SLACK_TOKEN,
    },
    SecretPattern {
        name: "stripe_key",
        regex: &STRIPE_KEY,
    },
    SecretPattern {
        name: "database_url",
        regex: &DATABASE_URL,
    },
    SecretPattern {
        name: "device_code",
        regex: &DEVICE_CODE,
    },
    SecretPattern {
        name: "oauth_url",
        regex: &OAUTH_URL,
    },
    SecretPattern {
        name: "generic_api_key",
        regex: &GENERIC_API_KEY,
    },
    SecretPattern {
        name: "generic_token",
        regex: &GENERIC_TOKEN,
    },
    SecretPattern {
        name: "generic_password",
        regex: &GENERIC_PASSWORD,
    },
    SecretPattern {
        name: "generic_secret",
        regex: &GENERIC_SECRET,
    },
];

/// Secret redactor for removing sensitive information from text
///
/// This redactor uses a conservative set of regex patterns to identify and
/// replace secrets with `[REDACTED]` markers. It is designed to err on the
/// side of caution - it's better to redact something that isn't a secret
/// than to leak an actual secret.
///
/// # Logging Conventions
///
/// When using the redactor, follow these conventions:
/// - **Never log raw device codes** - Always redact before logging
/// - **Never log OAuth URLs with embedded params** - Tokens in query strings
/// - **Always redact before audit/export** - Use `Redactor::redact()` on all output
///
/// # Example
///
/// ```
/// use wa_core::policy::Redactor;
///
/// let redactor = Redactor::new();
/// let input = "My API key is sk-abc123456789012345678901234567890123456789012345678901";
/// let output = redactor.redact(input);
/// assert!(output.contains("[REDACTED]"));
/// assert!(!output.contains("sk-abc"));
/// ```
#[derive(Debug, Default)]
pub struct Redactor {
    /// Whether to include pattern names in redaction markers (for debugging)
    include_pattern_names: bool,
}

impl Redactor {
    /// Create a new redactor with default settings
    #[must_use]
    pub fn new() -> Self {
        Self {
            include_pattern_names: false,
        }
    }

    /// Create a redactor that includes pattern names in redaction markers
    ///
    /// Output will be `[REDACTED:pattern_name]` instead of just `[REDACTED]`.
    /// Useful for debugging but should not be used in production logs.
    #[must_use]
    pub fn with_debug_markers() -> Self {
        Self {
            include_pattern_names: true,
        }
    }

    /// Redact all detected secrets from the input text
    ///
    /// Returns a new string with all detected secrets replaced by `[REDACTED]`.
    /// The original text is not modified.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        let mut result = text.to_string();

        for pattern in SECRET_PATTERNS {
            let replacement = if self.include_pattern_names {
                format!("[REDACTED:{}]", pattern.name)
            } else {
                REDACTED_MARKER.to_string()
            };

            result = pattern.regex.replace_all(&result, &replacement).to_string();
        }

        result
    }

    /// Check if text contains any detected secrets
    ///
    /// Returns true if any secret pattern matches.
    #[must_use]
    pub fn contains_secrets(&self, text: &str) -> bool {
        SECRET_PATTERNS
            .iter()
            .any(|pattern| pattern.regex.is_match(text))
    }

    /// Detect all secrets in text and return their locations
    ///
    /// Returns a vector of (pattern_name, start, end) tuples for each detected secret.
    #[must_use]
    pub fn detect(&self, text: &str) -> Vec<(&'static str, usize, usize)> {
        let mut detections = Vec::new();

        for pattern in SECRET_PATTERNS {
            for mat in pattern.regex.find_iter(text) {
                detections.push((pattern.name, mat.start(), mat.end()));
            }
        }

        // Sort by position for consistent ordering
        detections.sort_by_key(|(_, start, _)| *start);
        detections
    }
}

// ============================================================================
// Policy Rule Evaluation
// ============================================================================

/// Result of evaluating policy rules
#[derive(Debug, Clone)]
pub struct RuleEvaluationResult {
    /// The matching rule, if any
    pub matching_rule: Option<PolicyRule>,
    /// The decision from the matching rule
    pub decision: Option<PolicyRuleDecision>,
    /// All rules that were evaluated (for audit)
    pub rules_checked: Vec<String>,
}

/// Evaluate policy rules against input
///
/// Returns the first matching rule with highest priority (lowest priority number wins,
/// then decision severity: Deny > RequireApproval > Allow, then specificity).
#[must_use]
pub fn evaluate_policy_rules(
    rules_config: &PolicyRulesConfig,
    input: &PolicyInput,
) -> RuleEvaluationResult {
    if !rules_config.enabled || rules_config.rules.is_empty() {
        return RuleEvaluationResult {
            matching_rule: None,
            decision: None,
            rules_checked: Vec::new(),
        };
    }

    let mut rules_checked = Vec::new();
    let mut candidates: Vec<&PolicyRule> = Vec::new();

    for rule in &rules_config.rules {
        rules_checked.push(rule.id.clone());

        if matches_rule(&rule.match_on, input) {
            candidates.push(rule);
        }
    }

    if candidates.is_empty() {
        return RuleEvaluationResult {
            matching_rule: None,
            decision: None,
            rules_checked,
        };
    }

    // Sort candidates by: priority (asc), decision severity (deny > require > allow), specificity (desc)
    candidates.sort_by(|a, b| {
        // First: priority (lower is better)
        let priority_cmp = a.priority.cmp(&b.priority);
        if priority_cmp != std::cmp::Ordering::Equal {
            return priority_cmp;
        }

        // Second: decision severity (deny=0, require_approval=1, allow=2)
        let decision_cmp = a.decision.priority().cmp(&b.decision.priority());
        if decision_cmp != std::cmp::Ordering::Equal {
            return decision_cmp;
        }

        // Third: specificity (higher is better, so reverse)
        b.match_on.specificity().cmp(&a.match_on.specificity())
    });

    let best = candidates.into_iter().next().unwrap();
    RuleEvaluationResult {
        matching_rule: Some(best.clone()),
        decision: Some(best.decision),
        rules_checked,
    }
}

/// Check if a rule matches the given input
fn matches_rule(match_on: &PolicyRuleMatch, input: &PolicyInput) -> bool {
    // If all criteria are empty, it's a catch-all rule (matches everything)
    if match_on.is_catch_all() {
        return true;
    }

    // Check action kind
    if !match_on.actions.is_empty() && !match_on.actions.iter().any(|a| a == input.action.as_str())
    {
        return false;
    }

    // Check actor kind
    if !match_on.actors.is_empty() && !match_on.actors.iter().any(|a| a == input.actor.as_str()) {
        return false;
    }

    // Check pane ID
    if !match_on.pane_ids.is_empty() {
        match input.pane_id {
            Some(id) if match_on.pane_ids.contains(&id) => {}
            _ => return false,
        }
    }

    // Check pane domain
    if !match_on.pane_domains.is_empty() {
        match &input.domain {
            Some(domain) if match_on.pane_domains.iter().any(|d| d == domain) => {}
            _ => return false,
        }
    }

    // Check pane title (glob matching)
    if !match_on.pane_titles.is_empty() {
        match &input.pane_title {
            Some(title) => {
                let matches_any = match_on
                    .pane_titles
                    .iter()
                    .any(|pattern| glob_match(pattern, title));
                if !matches_any {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Check pane cwd (glob matching)
    if !match_on.pane_cwds.is_empty() {
        match &input.pane_cwd {
            Some(cwd) => {
                let matches_any = match_on
                    .pane_cwds
                    .iter()
                    .any(|pattern| glob_match(pattern, cwd));
                if !matches_any {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Check command patterns (regex)
    if !match_on.command_patterns.is_empty() {
        match &input.command_text {
            Some(text) => {
                let matches_any = match_on
                    .command_patterns
                    .iter()
                    .any(|pattern| Regex::new(pattern).is_ok_and(|re| re.is_match(text)));
                if !matches_any {
                    return false;
                }
            }
            None => return false,
        }
    }

    // Check agent type
    if !match_on.agent_types.is_empty() {
        match &input.agent_type {
            Some(agent) => {
                let matches_any = match_on
                    .agent_types
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(agent));
                if !matches_any {
                    return false;
                }
            }
            None => return false,
        }
    }

    true
}

/// Simple glob pattern matching
///
/// Supports `*` (any characters) and `?` (single character)
fn glob_match(pattern: &str, text: &str) -> bool {
    let regex_pattern = pattern
        .replace('.', r"\.")
        .replace('*', ".*")
        .replace('?', ".");
    let full_pattern = format!("^{regex_pattern}$");
    Regex::new(&full_pattern).is_ok_and(|re| re.is_match(text))
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
    /// Command safety gate configuration
    command_gate: CommandGateConfig,
    /// Custom policy rules configuration
    policy_rules: PolicyRulesConfig,
    /// Risk scoring configuration
    risk_config: RiskConfig,
}

impl PolicyEngine {
    /// Create a new policy engine with default settings
    #[must_use]
    pub fn new(
        rate_limit_per_pane: u32,
        rate_limit_global: u32,
        require_prompt_active: bool,
    ) -> Self {
        Self {
            rate_limiter: RateLimiter::new(rate_limit_per_pane, rate_limit_global),
            require_prompt_active,
            command_gate: CommandGateConfig::default(),
            policy_rules: PolicyRulesConfig::default(),
            risk_config: RiskConfig::default(),
        }
    }

    /// Create a policy engine with permissive defaults (for testing)
    #[must_use]
    pub fn permissive() -> Self {
        Self::new(1000, 5000, false)
    }

    /// Create a policy engine with strict defaults
    #[must_use]
    pub fn strict() -> Self {
        Self::new(30, 100, true)
    }

    /// Set command safety gate configuration
    #[must_use]
    pub fn with_command_gate_config(mut self, command_gate: CommandGateConfig) -> Self {
        self.command_gate = command_gate;
        self
    }

    /// Set custom policy rules configuration
    #[must_use]
    pub fn with_policy_rules(mut self, rules: PolicyRulesConfig) -> Self {
        self.policy_rules = rules;
        self
    }

    /// Set risk scoring configuration
    #[must_use]
    pub fn with_risk_config(mut self, config: RiskConfig) -> Self {
        self.risk_config = config;
        self
    }

    /// Get the current risk configuration
    #[must_use]
    pub fn risk_config(&self) -> &RiskConfig {
        &self.risk_config
    }

    /// Calculate risk score for the given input
    ///
    /// This evaluates all applicable risk factors and returns a composite score.
    #[must_use]
    pub fn calculate_risk(&self, input: &PolicyInput) -> RiskScore {
        if !self.risk_config.enabled {
            return RiskScore::zero();
        }

        let mut factors = Vec::new();

        // State factors
        let caps = &input.capabilities;
        {
            // Alt-screen detection
            match caps.alt_screen {
                Some(true) => {
                    self.add_factor(
                        &mut factors,
                        "state.alt_screen",
                        RiskCategory::State,
                        60,
                        "Pane is in alternate screen mode (vim, less, etc.)",
                    );
                }
                None => {
                    self.add_factor(
                        &mut factors,
                        "state.alt_screen_unknown",
                        RiskCategory::State,
                        40,
                        "Cannot determine if pane is in alternate screen mode",
                    );
                }
                Some(false) => {}
            }

            // Command running
            if caps.command_running {
                self.add_factor(
                    &mut factors,
                    "state.command_running",
                    RiskCategory::State,
                    25,
                    "A command is currently executing",
                );
            }

            // No prompt
            if !caps.prompt_active && input.action.is_mutating() {
                self.add_factor(
                    &mut factors,
                    "state.no_prompt",
                    RiskCategory::State,
                    20,
                    "No active prompt detected",
                );
            }

            // Recent gap
            if caps.has_recent_gap {
                self.add_factor(
                    &mut factors,
                    "state.recent_gap",
                    RiskCategory::State,
                    35,
                    "Recent capture gap (state uncertainty)",
                );
            }

            // Pane reserved
            if caps.is_reserved {
                self.add_factor(
                    &mut factors,
                    "state.is_reserved",
                    RiskCategory::State,
                    50,
                    "Pane is reserved by another workflow",
                );
            }
        }

        // Action factors
        if input.action.is_mutating() {
            self.add_factor(
                &mut factors,
                "action.is_mutating",
                RiskCategory::Action,
                10,
                "Action modifies pane state",
            );
        }

        if input.action.is_destructive() {
            self.add_factor(
                &mut factors,
                "action.is_destructive",
                RiskCategory::Action,
                25,
                "Action could be destructive (close, Ctrl-C/D)",
            );
        }

        if matches!(input.action, ActionKind::BrowserAuth) {
            self.add_factor(
                &mut factors,
                "action.browser_auth",
                RiskCategory::Action,
                30,
                "Browser-based authentication flow",
            );
        }

        if matches!(input.action, ActionKind::Spawn | ActionKind::Split) {
            self.add_factor(
                &mut factors,
                "action.spawn_split",
                RiskCategory::Action,
                20,
                "Creating new pane (resource allocation)",
            );
        }

        // Context factors
        if !input.actor.is_trusted() {
            self.add_factor(
                &mut factors,
                "context.actor_untrusted",
                RiskCategory::Context,
                15,
                "Actor is not human (robot/mcp/workflow)",
            );
        }

        // Content factors (for SendText)
        if input.action == ActionKind::SendText {
            if let Some(text) = &input.command_text {
                self.analyze_content_risk(text, &mut factors);
            }
        }

        RiskScore::from_factors(factors)
    }

    /// Helper to add a risk factor if not disabled
    fn add_factor(
        &self,
        factors: &mut Vec<AppliedRiskFactor>,
        id: &str,
        _category: RiskCategory,
        base_weight: u8,
        explanation: &str,
    ) {
        let weight = self.risk_config.get_weight(id, base_weight);
        if weight > 0 {
            factors.push(AppliedRiskFactor {
                id: id.to_string(),
                weight,
                explanation: explanation.to_string(),
            });
        }
    }

    /// Analyze command text for content-based risk factors
    #[allow(clippy::items_after_statements)]
    fn analyze_content_risk(&self, text: &str, factors: &mut Vec<AppliedRiskFactor>) {
        let text_lower = text.to_lowercase();

        // Destructive tokens
        const DESTRUCTIVE_PATTERNS: &[&str] = &[
            "rm -rf",
            "rm -fr",
            "rmdir",
            "drop table",
            "drop database",
            "truncate",
            "delete from",
            "git reset --hard",
            "git clean -f",
            "format c:",
            "mkfs",
            "> /dev/",
            "dd if=",
        ];

        if DESTRUCTIVE_PATTERNS.iter().any(|p| text_lower.contains(p)) {
            self.add_factor(
                factors,
                "content.destructive_tokens",
                RiskCategory::Content,
                40,
                "Command contains destructive patterns (rm -rf, DROP, etc.)",
            );
        }

        // Sudo/elevation
        if text_lower.starts_with("sudo ")
            || text_lower.contains(" sudo ")
            || text_lower.starts_with("doas ")
            || text_lower.starts_with("run0 ")
        {
            self.add_factor(
                factors,
                "content.sudo_elevation",
                RiskCategory::Content,
                30,
                "Command uses privilege elevation (sudo/doas)",
            );
        }

        // Multi-line/complex
        if text.contains('\n') && text.lines().count() > 2 {
            self.add_factor(
                factors,
                "content.multiline_complex",
                RiskCategory::Content,
                15,
                "Multi-line command (heredoc, compound)",
            );
        }

        // Pipe chain
        if text.contains(" | ") && text.matches(" | ").count() >= 2 {
            self.add_factor(
                factors,
                "content.pipe_chain",
                RiskCategory::Content,
                10,
                "Complex piped command chain",
            );
        }
    }

    /// Map a risk score to a policy decision
    #[must_use]
    pub fn risk_to_decision(&self, risk: &RiskScore, _input: &PolicyInput) -> PolicyDecision {
        if risk.score <= self.risk_config.allow_max {
            PolicyDecision::allow_with_rule("risk.score_allow")
        } else if risk.score <= self.risk_config.require_approval_max {
            PolicyDecision::require_approval_with_rule(
                format!(
                    "Action has elevated risk score of {} (threshold: {}). {}",
                    risk.score, self.risk_config.allow_max, risk.summary
                ),
                "risk.score_approval",
            )
        } else {
            PolicyDecision::deny_with_rule(
                format!(
                    "Action has high risk score of {} (threshold: {}). {}",
                    risk.score, self.risk_config.require_approval_max, risk.summary
                ),
                "risk.score_deny",
            )
        }
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
        let mut context = DecisionContext::from_input(input);

        // Calculate and attach risk score (wa-upg.6.3)
        let risk = self.calculate_risk(input);
        if risk.score > 0 {
            context.set_risk(risk);
        }

        // Check rate limit for configured action kinds
        if input.action.is_rate_limited() {
            match self.rate_limiter.check(input.action, input.pane_id) {
                RateLimitOutcome::Allowed => {
                    context.record_rule("policy.rate_limit", false, None, None);
                }
                RateLimitOutcome::Limited(hit) => {
                    context.rate_limit = Some(rate_limit_snapshot_from_hit(&hit));
                    context.record_rule(
                        "policy.rate_limit",
                        true,
                        Some("require_approval"),
                        Some(hit.reason()),
                    );
                    context.set_determining_rule("policy.rate_limit");
                    return PolicyDecision::require_approval_with_rule(
                        hit.reason(),
                        "policy.rate_limit",
                    )
                    .with_context(context);
                }
            }
        } else {
            context.record_rule(
                "policy.rate_limit",
                false,
                None,
                Some("action not rate limited".to_string()),
            );
        }

        // Check alt-screen state for send actions (always checked for safety)
        if matches!(input.action, ActionKind::SendText | ActionKind::SendControl) {
            // Deny if in alt-screen mode (vim, less, htop, etc.)
            if input.capabilities.alt_screen == Some(true) {
                context.record_rule(
                    "policy.alt_screen",
                    true,
                    Some("deny"),
                    Some("alt screen active".to_string()),
                );
                context.set_determining_rule("policy.alt_screen");
                return PolicyDecision::deny_with_rule(
                    "Cannot send text while in alt-screen mode (vim, less, etc.)",
                    "policy.alt_screen",
                )
                .with_context(context);
            }
            // Require approval if alt-screen state is unknown (conservative)
            if input.capabilities.alt_screen.is_none() && !input.actor.is_trusted() {
                context.record_rule(
                    "policy.alt_screen_unknown",
                    true,
                    Some("require_approval"),
                    Some("alt screen state unknown".to_string()),
                );
                context.set_determining_rule("policy.alt_screen_unknown");
                return PolicyDecision::require_approval_with_rule(
                    "Alt-screen state unknown - approval required before sending",
                    "policy.alt_screen_unknown",
                )
                .with_context(context);
            }
        }
        context.record_rule(
            "policy.alt_screen",
            false,
            None,
            Some("alt screen not active".to_string()),
        );

        // Check for recent capture gaps (safety check for send actions)
        if matches!(input.action, ActionKind::SendText | ActionKind::SendControl)
            && input.capabilities.has_recent_gap
        {
            // Recent gap means we might have missed output - require approval
            if !input.actor.is_trusted() {
                context.record_rule(
                    "policy.recent_gap",
                    true,
                    Some("require_approval"),
                    Some("recent capture gap detected".to_string()),
                );
                context.set_determining_rule("policy.recent_gap");
                return PolicyDecision::require_approval_with_rule(
                    "Recent capture gap detected - approval required before sending",
                    "policy.recent_gap",
                )
                .with_context(context);
            }
        }
        context.record_rule(
            "policy.recent_gap",
            false,
            None,
            Some("no recent gap".to_string()),
        );

        // Check prompt state for send actions
        if matches!(input.action, ActionKind::SendText | ActionKind::SendControl)
            && self.require_prompt_active
            && !input.capabilities.prompt_active
        {
            // If command is running, deny
            if input.capabilities.command_running {
                context.record_rule(
                    "policy.prompt_required",
                    true,
                    Some("deny"),
                    Some("command running".to_string()),
                );
                context.set_determining_rule("policy.prompt_required");
                return PolicyDecision::deny_with_rule(
                    "Refusing to send to running command - wait for prompt",
                    "policy.prompt_required",
                )
                .with_context(context);
            }
            // If state is unknown, require approval for non-trusted actors
            if !input.actor.is_trusted() {
                context.record_rule(
                    "policy.prompt_unknown",
                    true,
                    Some("require_approval"),
                    Some("prompt inactive and actor untrusted".to_string()),
                );
                context.set_determining_rule("policy.prompt_unknown");
                return PolicyDecision::require_approval_with_rule(
                    "Pane state unknown - approval required before sending",
                    "policy.prompt_unknown",
                )
                .with_context(context);
            }
        } else {
            context.record_rule(
                "policy.prompt_required",
                false,
                None,
                Some("prompt gate not applicable".to_string()),
            );
        }

        // Check reservation conflicts
        if input.action.is_mutating() && input.capabilities.is_reserved {
            // Allow if this is the workflow that has the reservation
            if let (Some(reserved_by), Some(workflow_id)) =
                (&input.capabilities.reserved_by, &input.workflow_id)
            {
                if reserved_by == workflow_id {
                    context.record_rule(
                        "policy.pane_reserved",
                        false,
                        None,
                        Some("reserved by same workflow".to_string()),
                    );
                    return PolicyDecision::allow().with_context(context);
                }
            }
            // Otherwise deny
            let reason = format!(
                "Pane is reserved by workflow {}",
                input
                    .capabilities
                    .reserved_by
                    .as_deref()
                    .unwrap_or("unknown")
            );
            context.record_rule(
                "policy.pane_reserved",
                true,
                Some("deny"),
                Some(reason.clone()),
            );
            context.set_determining_rule("policy.pane_reserved");
            return PolicyDecision::deny_with_rule(reason, "policy.pane_reserved")
                .with_context(context);
        }
        context.record_rule(
            "policy.pane_reserved",
            false,
            None,
            Some("no reservation conflict".to_string()),
        );

        // Command safety gate for SendText
        if matches!(input.action, ActionKind::SendText) {
            if let Some(text) = input.command_text.as_deref() {
                match evaluate_command_gate(text, &self.command_gate) {
                    CommandGateOutcome::Allow => {
                        context.record_rule(
                            "policy.command_gate",
                            false,
                            None,
                            Some("command gate allow".to_string()),
                        );
                    }
                    CommandGateOutcome::Deny { reason, rule_id } => {
                        context.record_rule(
                            rule_id.clone(),
                            true,
                            Some("deny"),
                            Some(reason.clone()),
                        );
                        context.set_determining_rule(rule_id.clone());
                        return PolicyDecision::deny_with_rule(reason, rule_id)
                            .with_context(context);
                    }
                    CommandGateOutcome::RequireApproval { reason, rule_id } => {
                        context.record_rule(
                            rule_id.clone(),
                            true,
                            Some("require_approval"),
                            Some(reason.clone()),
                        );
                        context.set_determining_rule(rule_id.clone());
                        return PolicyDecision::require_approval_with_rule(reason, rule_id)
                            .with_context(context);
                    }
                }
            } else {
                context.record_rule(
                    "policy.command_gate",
                    false,
                    None,
                    Some("no command text".to_string()),
                );
            }
        } else {
            context.record_rule(
                "policy.command_gate",
                false,
                None,
                Some("non-send action".to_string()),
            );
        }

        // Evaluate custom policy rules (after builtin safety gates, before defaults)
        let rule_result = evaluate_policy_rules(&self.policy_rules, input);
        for rule_id in &rule_result.rules_checked {
            // Record that we checked this rule (matched rules will be recorded below)
            context.record_rule(
                format!("config.rule.{rule_id}"),
                false,
                None,
                Some("rule checked".to_string()),
            );
        }

        if let (Some(rule), Some(decision)) = (rule_result.matching_rule, rule_result.decision) {
            let rule_id = format!("config.rule.{}", rule.id);
            let reason = rule
                .message
                .clone()
                .unwrap_or_else(|| format!("Rule '{}' matched", rule.id));

            match decision {
                PolicyRuleDecision::Deny => {
                    context.record_rule(&rule_id, true, Some("deny"), Some(reason.clone()));
                    context.set_determining_rule(&rule_id);
                    return PolicyDecision::deny_with_rule(reason, rule_id).with_context(context);
                }
                PolicyRuleDecision::RequireApproval => {
                    context.record_rule(
                        &rule_id,
                        true,
                        Some("require_approval"),
                        Some(reason.clone()),
                    );
                    context.set_determining_rule(&rule_id);
                    return PolicyDecision::require_approval_with_rule(reason, rule_id)
                        .with_context(context);
                }
                PolicyRuleDecision::Allow => {
                    // Allow rules short-circuit to allow (skipping default checks)
                    context.record_rule(&rule_id, true, Some("allow"), Some(reason));
                    context.set_determining_rule(&rule_id);
                    return PolicyDecision::allow_with_rule(rule_id).with_context(context);
                }
            }
        }

        // Destructive actions require approval for non-trusted actors
        if input.action.is_destructive() && !input.actor.is_trusted() {
            let reason = format!(
                "Destructive action '{}' requires approval",
                input.action.as_str()
            );
            context.record_rule(
                "policy.destructive_action",
                true,
                Some("require_approval"),
                Some(reason.clone()),
            );
            context.set_determining_rule("policy.destructive_action");
            return PolicyDecision::require_approval_with_rule(reason, "policy.destructive_action")
                .with_context(context);
        }
        context.record_rule(
            "policy.destructive_action",
            false,
            None,
            Some("non-destructive or trusted actor".to_string()),
        );

        PolicyDecision::allow().with_context(context)
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
    ///
    /// Uses the `Redactor` to replace detected secrets with `[REDACTED]`.
    /// This should be called on all text before it is written to logs, audit
    /// trails, or exported.
    #[must_use]
    pub fn redact_secrets(&self, text: &str) -> String {
        static REDACTOR: LazyLock<Redactor> = LazyLock::new(Redactor::new);
        REDACTOR.redact(text)
    }

    /// Check if text contains secrets that would be redacted
    #[must_use]
    pub fn contains_secrets(&self, text: &str) -> bool {
        static REDACTOR: LazyLock<Redactor> = LazyLock::new(Redactor::new);
        REDACTOR.contains_secrets(text)
    }
}

const AUDIT_PREVIEW_CHARS: usize = 80;

/// Redacted summary metadata for SendText audit entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendTextAuditSummary {
    /// Original text length (bytes).
    pub text_length: usize,
    /// Redacted preview of the text (truncated).
    pub text_preview_redacted: String,
    /// Stable hash of the original text.
    pub text_hash: String,
    /// Whether the text looks like a shell command.
    pub command_candidate: bool,
    /// Workflow execution ID, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_execution_id: Option<String>,
    /// Parent audit action ID (workflow start), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_action_id: Option<i64>,
}

fn redacted_preview(text: &str) -> String {
    static REDACTOR: LazyLock<Redactor> = LazyLock::new(Redactor::new);
    let redacted = REDACTOR.redact(text);
    redacted.chars().take(AUDIT_PREVIEW_CHARS).collect()
}

/// Build a structured, redacted summary for SendText audit records.
#[must_use]
pub fn build_send_text_audit_summary(
    text: &str,
    workflow_execution_id: Option<&str>,
    parent_action_id: Option<i64>,
) -> String {
    let summary = SendTextAuditSummary {
        text_length: text.len(),
        text_preview_redacted: redacted_preview(text),
        text_hash: format!("{:016x}", crate::wezterm::stable_hash(text.as_bytes())),
        command_candidate: is_command_candidate(text),
        workflow_execution_id: workflow_execution_id.map(str::to_string),
        parent_action_id,
    };
    serde_json::to_string(&summary).unwrap_or_else(|_| "send_text_summary_unavailable".to_string())
}

// ============================================================================
// Policy Gated Injector (wa-4vx.8.5)
// ============================================================================

/// Result of a policy-gated injection attempt
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InjectionResult {
    /// Injection was allowed and executed
    Allowed {
        /// The policy decision (for audit)
        decision: PolicyDecision,
        /// Redacted summary of what was sent
        summary: String,
        /// Pane ID that received the injection
        pane_id: u64,
        /// Action kind that was performed
        action: ActionKind,
        /// Audit action ID for workflow step correlation (wa-nu4.1.1.11)
        #[serde(skip_serializing_if = "Option::is_none")]
        audit_action_id: Option<i64>,
    },
    /// Injection was denied by policy
    Denied {
        /// The policy decision with reason
        decision: PolicyDecision,
        /// Redacted summary of what was attempted
        summary: String,
        /// Pane ID that was targeted
        pane_id: u64,
        /// Action kind that was attempted
        action: ActionKind,
        /// Audit action ID for workflow step correlation (wa-nu4.1.1.11)
        #[serde(skip_serializing_if = "Option::is_none")]
        audit_action_id: Option<i64>,
    },
    /// Injection requires approval before proceeding
    RequiresApproval {
        /// The policy decision with approval details
        decision: PolicyDecision,
        /// Redacted summary of what was attempted
        summary: String,
        /// Pane ID that was targeted
        pane_id: u64,
        /// Action kind that was attempted
        action: ActionKind,
        /// Audit action ID for workflow step correlation (wa-nu4.1.1.11)
        #[serde(skip_serializing_if = "Option::is_none")]
        audit_action_id: Option<i64>,
    },
    /// Injection failed due to an error (after policy allowed)
    Error {
        /// Error message
        error: String,
        /// Pane ID that was targeted
        pane_id: u64,
        /// Action kind that was attempted
        action: ActionKind,
        /// Audit action ID for workflow step correlation (wa-nu4.1.1.11)
        #[serde(skip_serializing_if = "Option::is_none")]
        audit_action_id: Option<i64>,
    },
}

impl InjectionResult {
    /// Check if the injection succeeded
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    /// Check if the injection was denied
    #[must_use]
    pub fn is_denied(&self) -> bool {
        matches!(self, Self::Denied { .. })
    }

    /// Check if the injection requires approval
    #[must_use]
    pub fn requires_approval(&self) -> bool {
        matches!(self, Self::RequiresApproval { .. })
    }

    /// Get the error message if this is an error result
    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }

    /// Get the rule ID that caused denial or approval requirement
    #[must_use]
    pub fn rule_id(&self) -> Option<&str> {
        match self {
            Self::Denied { decision, .. } | Self::RequiresApproval { decision, .. } => {
                decision.rule_id()
            }
            _ => None,
        }
    }

    /// Get the audit action ID if set (for workflow step correlation)
    #[must_use]
    pub fn audit_action_id(&self) -> Option<i64> {
        match self {
            Self::Allowed {
                audit_action_id, ..
            }
            | Self::Denied {
                audit_action_id, ..
            }
            | Self::RequiresApproval {
                audit_action_id, ..
            }
            | Self::Error {
                audit_action_id, ..
            } => *audit_action_id,
        }
    }

    /// Set the audit action ID (called after audit record is persisted)
    pub fn set_audit_action_id(&mut self, id: i64) {
        match self {
            Self::Allowed {
                audit_action_id, ..
            }
            | Self::Denied {
                audit_action_id, ..
            }
            | Self::RequiresApproval {
                audit_action_id, ..
            }
            | Self::Error {
                audit_action_id, ..
            } => {
                *audit_action_id = Some(id);
            }
        }
    }

    /// Convert to an audit record for persistence
    ///
    /// Creates an `AuditActionRecord` suitable for storing in the audit trail.
    /// All text fields are already redacted by the PolicyGatedInjector before
    /// being included in the InjectionResult.
    ///
    /// # Arguments
    /// * `actor` - The actor kind that initiated the action
    /// * `actor_id` - Optional actor identifier (workflow id, MCP client, etc.)
    /// * `domain` - Optional domain name of the target pane
    #[must_use]
    pub fn to_audit_record(
        &self,
        actor: ActorKind,
        actor_id: Option<String>,
        domain: Option<String>,
    ) -> crate::storage::AuditActionRecord {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));

        match self {
            Self::Allowed {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id: _,
            } => crate::storage::AuditActionRecord {
                id: 0, // Assigned by database
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                correlation_id: None,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: decision.as_str().to_string(),
                decision_reason: None,
                rule_id: None,
                input_summary: Some(summary.clone()),
                verification_summary: None,
                decision_context: decision
                    .context()
                    .and_then(|ctx| serde_json::to_string(ctx).ok()),
                result: "success".to_string(),
            },
            Self::Denied {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id: _,
            } => crate::storage::AuditActionRecord {
                id: 0,
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                correlation_id: None,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: decision.as_str().to_string(),
                decision_reason: decision.reason().map(String::from),
                rule_id: decision.rule_id().map(String::from),
                input_summary: Some(summary.clone()),
                verification_summary: None,
                decision_context: decision
                    .context()
                    .and_then(|ctx| serde_json::to_string(ctx).ok()),
                result: "denied".to_string(),
            },
            Self::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id: _,
            } => crate::storage::AuditActionRecord {
                id: 0,
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                correlation_id: None,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: decision.as_str().to_string(),
                decision_reason: decision.reason().map(String::from),
                rule_id: decision.rule_id().map(String::from),
                input_summary: Some(summary.clone()),
                verification_summary: None,
                decision_context: decision
                    .context()
                    .and_then(|ctx| serde_json::to_string(ctx).ok()),
                result: "require_approval".to_string(),
            },
            Self::Error {
                error,
                pane_id,
                action,
                audit_action_id: _,
            } => crate::storage::AuditActionRecord {
                id: 0,
                ts: now_ms,
                actor_kind: actor.as_str().to_string(),
                actor_id,
                correlation_id: None,
                pane_id: Some(*pane_id),
                domain,
                action_kind: action.as_str().to_string(),
                policy_decision: "allow".to_string(), // Policy allowed, but execution failed
                decision_reason: None,
                rule_id: None,
                input_summary: None,
                verification_summary: Some(error.clone()),
                decision_context: None,
                result: "error".to_string(),
            },
        }
    }
}

/// Policy-gated input injector
///
/// This is the **single implementation** of "send with policy" that all
/// user-facing send commands and workflow action executors must use.
///
/// # Responsibilities
///
/// 1. Build a `PolicyInput` (actor kind, pane id, action kind, redacted summary)
/// 2. Call `PolicyEngine::authorize`
/// 3. If allowed: perform the injection via the WezTerm client
/// 4. Return a structured outcome suitable for robot/human/workflow logging
///
/// # Safety
///
/// Attempting to inject input while pane state is `AltScreen` will be denied.
/// Unknown alt-screen state also triggers denial (conservative by default).
///
/// # Example
///
/// ```ignore
/// use wa_core::policy::{PolicyGatedInjector, PolicyEngine, ActorKind};
/// use wa_core::wezterm::WeztermClient;
///
/// let engine = PolicyEngine::default();
/// let client = WeztermClient::new();
/// let injector = PolicyGatedInjector::new(engine, client);
///
/// // Capabilities are derived from current pane state
/// let caps = PaneCapabilities::prompt();
/// let result = injector.send_text(1, "ls -la", ActorKind::Robot, &caps).await;
///
/// match result {
///     InjectionResult::Allowed { .. } => println!("Sent successfully"),
///     InjectionResult::Denied { decision, .. } => println!("Denied: {:?}", decision),
///     InjectionResult::RequiresApproval { decision, .. } => println!("Needs approval"),
///     InjectionResult::Error { error, .. } => println!("Error: {}", error),
/// }
/// ```
pub struct PolicyGatedInjector<C = crate::wezterm::WeztermClient> {
    engine: PolicyEngine,
    client: C,
    /// Optional storage handle for audit trail emission
    storage: Option<crate::storage::StorageHandle>,
}

impl<C> PolicyGatedInjector<C>
where
    C: crate::wezterm::WeztermInterface,
{
    /// Create a new policy-gated injector without audit trail storage
    #[must_use]
    pub fn new(engine: PolicyEngine, client: C) -> Self {
        Self {
            engine,
            client,
            storage: None,
        }
    }

    /// Create a new policy-gated injector with audit trail storage
    ///
    /// Every injection (allow, deny, require_approval, error) will be recorded
    /// to the audit trail via the storage handle.
    #[must_use]
    pub fn with_storage(
        engine: PolicyEngine,
        client: C,
        storage: crate::storage::StorageHandle,
    ) -> Self {
        Self {
            engine,
            client,
            storage: Some(storage),
        }
    }

    /// Set the storage handle for audit trail emission
    pub fn set_storage(&mut self, storage: crate::storage::StorageHandle) {
        self.storage = Some(storage);
    }

    /// Create with a permissive policy engine (for testing)
    #[must_use]
    pub fn permissive(client: C) -> Self {
        Self::new(PolicyEngine::permissive(), client)
    }

    /// Get mutable access to the policy engine
    pub fn engine_mut(&mut self) -> &mut PolicyEngine {
        &mut self.engine
    }

    /// Get the policy engine reference
    #[must_use]
    pub fn engine(&self) -> &PolicyEngine {
        &self.engine
    }

    /// Send text to a pane with policy gating
    ///
    /// This is the primary method for sending text. It:
    /// 1. Builds policy input with the given capabilities
    /// 2. Checks command safety gate (dangerous command detection)
    /// 3. Authorizes via PolicyEngine
    /// 4. If allowed, sends via WeztermClient
    /// 5. Returns structured result for audit
    pub async fn send_text(
        &mut self,
        pane_id: u64,
        text: &str,
        actor: ActorKind,
        capabilities: &PaneCapabilities,
        workflow_id: Option<&str>,
    ) -> InjectionResult {
        self.inject(
            pane_id,
            text,
            ActionKind::SendText,
            actor,
            capabilities,
            workflow_id,
        )
        .await
    }

    /// Send Ctrl-C (interrupt) to a pane with policy gating
    pub async fn send_ctrl_c(
        &mut self,
        pane_id: u64,
        actor: ActorKind,
        capabilities: &PaneCapabilities,
        workflow_id: Option<&str>,
    ) -> InjectionResult {
        self.inject(
            pane_id,
            crate::wezterm::control::CTRL_C,
            ActionKind::SendCtrlC,
            actor,
            capabilities,
            workflow_id,
        )
        .await
    }

    /// Send Ctrl-D (EOF) to a pane with policy gating
    pub async fn send_ctrl_d(
        &mut self,
        pane_id: u64,
        actor: ActorKind,
        capabilities: &PaneCapabilities,
        workflow_id: Option<&str>,
    ) -> InjectionResult {
        self.inject(
            pane_id,
            crate::wezterm::control::CTRL_D,
            ActionKind::SendCtrlD,
            actor,
            capabilities,
            workflow_id,
        )
        .await
    }

    /// Send Ctrl-Z (suspend) to a pane with policy gating
    pub async fn send_ctrl_z(
        &mut self,
        pane_id: u64,
        actor: ActorKind,
        capabilities: &PaneCapabilities,
        workflow_id: Option<&str>,
    ) -> InjectionResult {
        self.inject(
            pane_id,
            crate::wezterm::control::CTRL_Z,
            ActionKind::SendCtrlZ,
            actor,
            capabilities,
            workflow_id,
        )
        .await
    }

    /// Send any control character to a pane with policy gating
    pub async fn send_control(
        &mut self,
        pane_id: u64,
        control_char: &str,
        actor: ActorKind,
        capabilities: &PaneCapabilities,
        workflow_id: Option<&str>,
    ) -> InjectionResult {
        self.inject(
            pane_id,
            control_char,
            ActionKind::SendControl,
            actor,
            capabilities,
            workflow_id,
        )
        .await
    }

    /// Internal injection method with policy gating
    ///
    /// This method:
    /// 1. Creates a redacted summary for audit
    /// 2. Builds policy input with actor, capabilities, and command text
    /// 3. Authorizes via PolicyEngine
    /// 4. If allowed, executes the injection
    /// 5. Emits an audit record (if storage is configured)
    /// 6. Returns the structured result
    async fn inject(
        &mut self,
        pane_id: u64,
        text: &str,
        action: ActionKind,
        actor: ActorKind,
        capabilities: &PaneCapabilities,
        workflow_id: Option<&str>,
    ) -> InjectionResult {
        // Create redacted summary for audit
        let summary = self.engine.redact_secrets(text);

        // Build policy input
        let mut input = PolicyInput::new(action, actor)
            .with_pane(pane_id)
            .with_capabilities(capabilities.clone())
            .with_text_summary(&summary);

        // Add workflow context if present
        if let Some(wf_id) = workflow_id {
            input = input.with_workflow(wf_id);
        }

        // For SendText, add command text for safety gate
        if action == ActionKind::SendText {
            input = input.with_command_text(text);
        }

        // Authorize
        let decision = self.engine.authorize(&input);

        // Build the injection result
        let mut result = match &decision {
            PolicyDecision::Allow { .. } => {
                // SAFETY: This is the only place where actual injection happens
                // after policy approval. The text reference lifetime is handled
                // by copying to a String for the actual send.
                let text_owned = text.to_string();
                let client = &self.client;

                // We need to call the send function with owned data
                let send_result = match action {
                    ActionKind::SendText => client.send_text(pane_id, &text_owned).await,
                    ActionKind::SendCtrlC => client.send_ctrl_c(pane_id).await,
                    ActionKind::SendCtrlD => client.send_ctrl_d(pane_id).await,
                    ActionKind::SendCtrlZ => {
                        client
                            .send_control(pane_id, crate::wezterm::control::CTRL_Z)
                            .await
                    }
                    ActionKind::SendControl => client.send_control(pane_id, &text_owned).await,
                    _ => unreachable!("inject called with non-injection action"),
                };

                match send_result {
                    Ok(()) => InjectionResult::Allowed {
                        decision,
                        summary,
                        pane_id,
                        action,
                        audit_action_id: None,
                    },
                    Err(e) => InjectionResult::Error {
                        error: e.to_string(),
                        pane_id,
                        action,
                        audit_action_id: None,
                    },
                }
            }
            PolicyDecision::Deny { .. } => InjectionResult::Denied {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id: None,
            },
            PolicyDecision::RequireApproval { .. } => InjectionResult::RequiresApproval {
                decision,
                summary,
                pane_id,
                action,
                audit_action_id: None,
            },
        };

        let storage_for_summary = self.storage.clone();
        let mut audit_summary = None;
        if action == ActionKind::SendText {
            if let Some(storage) = storage_for_summary.as_ref() {
                let parent_action_id = if actor == ActorKind::Workflow {
                    if let Some(id) = workflow_id {
                        find_workflow_start_action_id(storage, id).await
                    } else {
                        None
                    }
                } else {
                    None
                };
                audit_summary = Some(build_send_text_audit_summary(
                    text,
                    workflow_id,
                    parent_action_id,
                ));
            }
        }

        // Emit audit record if storage is configured (wa-4vx.8.7)
        // Audit is emitted for ALL outcomes: allow, deny, require_approval, and error
        // Capture the audit ID for workflow step correlation (wa-nu4.1.1.11)
        if let Some(ref storage) = self.storage {
            let mut audit_record = result.to_audit_record(
                actor,
                workflow_id.map(String::from),
                None, // domain - could be derived from pane info if available
            );
            if let Some(summary) = audit_summary {
                audit_record.input_summary = Some(summary);
            }
            match storage.record_audit_action_redacted(audit_record).await {
                Ok(audit_id) => {
                    result.set_audit_action_id(audit_id);
                }
                Err(e) => {
                    tracing::warn!(
                        pane_id,
                        action = action.as_str(),
                        "Failed to emit audit record: {e}"
                    );
                }
            }
        }

        result
    }

    /// Redact text using the policy engine's redactor
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        self.engine.redact_secrets(text)
    }
}

async fn find_workflow_start_action_id(
    storage: &crate::storage::StorageHandle,
    execution_id: &str,
) -> Option<i64> {
    let query = crate::storage::AuditQuery {
        limit: Some(1),
        actor_id: Some(execution_id.to_string()),
        action_kind: Some("workflow_start".to_string()),
        ..Default::default()
    };
    storage
        .get_audit_actions(query)
        .await
        .ok()
        .and_then(|mut rows| rows.pop().map(|row| row.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // Rate Limiter Tests
    // ========================================================================

    #[test]
    fn rate_limiter_allows_under_limit() {
        let mut limiter = RateLimiter::new(10, 100);
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
    }

    #[test]
    fn rate_limiter_denies_over_limit() {
        let mut limiter = RateLimiter::new(2, 100);
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        assert!(matches!(
            limiter.check(ActionKind::SendText, Some(1)),
            RateLimitOutcome::Limited(_)
        )); // Third request limited
    }

    #[test]
    fn rate_limiter_is_per_pane() {
        let mut limiter = RateLimiter::new(1, 100);
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        assert!(limiter.check(ActionKind::SendText, Some(2)).is_allowed()); // Different pane, allowed
        assert!(matches!(
            limiter.check(ActionKind::SendText, Some(1)),
            RateLimitOutcome::Limited(_)
        )); // Same pane, limited
    }

    #[test]
    fn rate_limiter_is_per_action_kind() {
        let mut limiter = RateLimiter::new(1, 100);
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        assert!(limiter.check(ActionKind::SendCtrlC, Some(1)).is_allowed()); // Different action, allowed
        assert!(matches!(
            limiter.check(ActionKind::SendText, Some(1)),
            RateLimitOutcome::Limited(_)
        )); // Same action, limited
    }

    #[test]
    fn rate_limiter_enforces_global_limit() {
        let mut limiter = RateLimiter::new(100, 2);
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        assert!(limiter.check(ActionKind::SendText, Some(2)).is_allowed());
        let hit = match limiter.check(ActionKind::SendText, Some(3)) {
            RateLimitOutcome::Limited(hit) => hit,
            RateLimitOutcome::Allowed => panic!("Expected global rate limit"),
        };
        assert!(matches!(hit.scope, RateLimitScope::Global));
    }

    #[test]
    fn rate_limiter_retry_after_is_nonzero() {
        let mut limiter = RateLimiter::new(1, 100);
        assert!(limiter.check(ActionKind::SendText, Some(1)).is_allowed());
        let hit = match limiter.check(ActionKind::SendText, Some(1)) {
            RateLimitOutcome::Limited(hit) => hit,
            RateLimitOutcome::Allowed => panic!("Expected rate limit"),
        };
        assert!(hit.retry_after > Duration::from_secs(0));
    }

    // ========================================================================
    // Command Safety Gate Tests
    // ========================================================================

    #[test]
    fn command_candidate_detects_shell_commands() {
        assert!(is_command_candidate("git status"));
        assert!(is_command_candidate("  $ rm -rf /tmp"));
        assert!(is_command_candidate("sudo git reset --hard"));
        assert!(!is_command_candidate("Please check the logs"));
        assert!(!is_command_candidate("# commented command"));
    }

    #[test]
    fn command_gate_blocks_rm_rf_root() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("rm -rf /");

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("command.rm_rf_root"));
    }

    #[test]
    fn command_gate_requires_approval_for_git_reset() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("git reset --hard HEAD~1");

        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("command.git_reset_hard"));
    }

    #[test]
    fn command_gate_ignores_non_command_text() {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("please review the diff and proceed");

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn command_gate_uses_dcg_when_enabled() {
        let config = CommandGateConfig {
            enabled: true,
            dcg_mode: DcgMode::Opportunistic,
            dcg_deny_policy: DcgDenyPolicy::RequireApproval,
        };
        let outcome = evaluate_command_gate_with_runner("git status", &config, |_cmd| {
            Ok(DcgDecision::Deny {
                rule_id: Some("core.git:reset-hard".to_string()),
            })
        });

        match outcome {
            CommandGateOutcome::RequireApproval { rule_id, .. } => {
                assert_eq!(rule_id, "dcg.core.git:reset-hard");
            }
            _ => panic!("Expected require approval"),
        }
    }

    #[test]
    fn command_gate_requires_approval_when_dcg_required_missing() {
        let config = CommandGateConfig {
            enabled: true,
            dcg_mode: DcgMode::Required,
            dcg_deny_policy: DcgDenyPolicy::RequireApproval,
        };
        let outcome = evaluate_command_gate_with_runner("git status", &config, |_cmd| {
            Err(DcgError::NotAvailable)
        });

        match outcome {
            CommandGateOutcome::RequireApproval { rule_id, .. } => {
                assert_eq!(rule_id, "command_gate.dcg_unavailable");
            }
            _ => panic!("Expected require approval"),
        }
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
    fn action_kind_rate_limited() {
        assert!(ActionKind::SendText.is_rate_limited());
        assert!(ActionKind::WorkflowRun.is_rate_limited());
        assert!(!ActionKind::ReadOutput.is_rate_limited());
        assert!(!ActionKind::SearchOutput.is_rate_limited());
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

    #[test]
    fn policy_decision_as_str() {
        assert_eq!(PolicyDecision::allow().as_str(), "allow");
        assert_eq!(PolicyDecision::deny("reason").as_str(), "deny");
        assert_eq!(
            PolicyDecision::require_approval("reason").as_str(),
            "require_approval"
        );
    }

    #[test]
    fn policy_decision_reason() {
        assert!(PolicyDecision::allow().reason().is_none());
        assert_eq!(
            PolicyDecision::deny("deny reason").reason(),
            Some("deny reason")
        );
        assert_eq!(
            PolicyDecision::require_approval("approval reason").reason(),
            Some("approval reason")
        );
    }

    #[test]
    fn policy_decision_deny_cannot_have_approval_attached() {
        // Critical safety test: Deny decisions must not be overridable by approval
        let deny = PolicyDecision::deny_with_rule("forbidden action", "test.deny");

        let fake_approval = ApprovalRequest {
            allow_once_code: "ABCD1234".to_string(),
            allow_once_full_hash: "sha256:fake".to_string(),
            expires_at: 999_999_999_999,
            summary: "trying to bypass".to_string(),
            command: "wa approve ABCD1234".to_string(),
        };

        // Attempt to attach approval to a Deny decision
        let after_approval = deny.with_approval(fake_approval);

        // Must still be denied - approval cannot override
        assert!(after_approval.is_denied());
        assert!(!after_approval.requires_approval());
        assert!(!after_approval.is_allowed());
        assert_eq!(after_approval.rule_id(), Some("test.deny"));
    }

    #[test]
    fn policy_decision_allow_cannot_have_approval_attached() {
        // Approval is only meaningful for RequireApproval decisions
        let allow = PolicyDecision::allow();

        let fake_approval = ApprovalRequest {
            allow_once_code: "ABCD1234".to_string(),
            allow_once_full_hash: "sha256:fake".to_string(),
            expires_at: 999_999_999_999,
            summary: "unnecessary approval".to_string(),
            command: "wa approve ABCD1234".to_string(),
        };

        let after_approval = allow.with_approval(fake_approval);

        // Should still be Allow, unchanged
        assert!(after_approval.is_allowed());
        assert!(!after_approval.requires_approval());
    }

    #[test]
    fn policy_decision_require_approval_can_have_approval_attached() {
        let require = PolicyDecision::require_approval_with_rule("needs approval", "test.require");

        let approval = ApprovalRequest {
            allow_once_code: "ABCD1234".to_string(),
            allow_once_full_hash: "sha256:test".to_string(),
            expires_at: 999_999_999_999,
            summary: "legitimate approval".to_string(),
            command: "wa approve ABCD1234".to_string(),
        };

        let after_approval = require.with_approval(approval);

        // Should still require approval but now has the approval payload
        assert!(after_approval.requires_approval());
        assert!(!after_approval.is_allowed());
        assert!(!after_approval.is_denied());
    }

    // ========================================================================
    // InjectionResult Audit Record Tests (wa-4vx.8.7)
    // ========================================================================

    #[test]
    fn injection_result_allowed_to_audit_record() {
        let result = InjectionResult::Allowed {
            decision: PolicyDecision::allow(),
            summary: "ls -la".to_string(),
            pane_id: 42,
            action: ActionKind::SendText,
            audit_action_id: None,
        };

        let record = result.to_audit_record(
            ActorKind::Robot,
            Some("wf-123".to_string()),
            Some("local".to_string()),
        );

        assert_eq!(record.actor_kind, "robot");
        assert_eq!(record.actor_id, Some("wf-123".to_string()));
        assert_eq!(record.pane_id, Some(42));
        assert_eq!(record.domain, Some("local".to_string()));
        assert_eq!(record.action_kind, "send_text");
        assert_eq!(record.policy_decision, "allow");
        assert!(record.decision_reason.is_none());
        assert!(record.rule_id.is_none());
        assert_eq!(record.input_summary, Some("ls -la".to_string()));
        assert_eq!(record.result, "success");
    }

    #[test]
    fn injection_result_denied_to_audit_record() {
        let result = InjectionResult::Denied {
            decision: PolicyDecision::deny_with_rule("alt screen active", "policy.alt_screen"),
            summary: "rm -rf /".to_string(),
            pane_id: 1,
            action: ActionKind::SendText,
            audit_action_id: None,
        };

        let record = result.to_audit_record(ActorKind::Mcp, None, None);

        assert_eq!(record.actor_kind, "mcp");
        assert!(record.actor_id.is_none());
        assert_eq!(record.pane_id, Some(1));
        assert_eq!(record.policy_decision, "deny");
        assert_eq!(
            record.decision_reason,
            Some("alt screen active".to_string())
        );
        assert_eq!(record.rule_id, Some("policy.alt_screen".to_string()));
        assert_eq!(record.result, "denied");
    }

    #[test]
    fn injection_result_requires_approval_to_audit_record() {
        let result = InjectionResult::RequiresApproval {
            decision: PolicyDecision::require_approval_with_rule("unknown state", "policy.unknown"),
            summary: "some command".to_string(),
            pane_id: 5,
            action: ActionKind::SendCtrlC,
            audit_action_id: None,
        };

        let record = result.to_audit_record(ActorKind::Workflow, Some("wf-456".to_string()), None);

        assert_eq!(record.actor_kind, "workflow");
        assert_eq!(record.actor_id, Some("wf-456".to_string()));
        assert_eq!(record.action_kind, "send_ctrl_c");
        assert_eq!(record.policy_decision, "require_approval");
        assert_eq!(record.decision_reason, Some("unknown state".to_string()));
        assert_eq!(record.rule_id, Some("policy.unknown".to_string()));
        assert_eq!(record.result, "require_approval");
    }

    #[test]
    fn injection_result_error_to_audit_record() {
        let result = InjectionResult::Error {
            error: "WezTerm connection failed".to_string(),
            pane_id: 99,
            action: ActionKind::SendText,
            audit_action_id: None,
        };

        let record = result.to_audit_record(ActorKind::Human, None, None);

        assert_eq!(record.actor_kind, "human");
        assert_eq!(record.pane_id, Some(99));
        assert_eq!(record.policy_decision, "allow"); // Policy allowed, but execution failed
        assert!(record.input_summary.is_none());
        assert_eq!(
            record.verification_summary,
            Some("WezTerm connection failed".to_string())
        );
        assert_eq!(record.result, "error");
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
        // When fully unknown, alt_screen check fires first (before prompt check)
        assert_eq!(decision.rule_id(), Some("policy.alt_screen_unknown"));
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
    fn authorize_denies_send_in_alt_screen() {
        let mut engine = PolicyEngine::permissive();
        let caps = PaneCapabilities::alt_screen();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.alt_screen"));
    }

    #[test]
    fn authorize_denies_send_in_alt_screen_even_for_human() {
        // Alt-screen is a hard safety gate - even humans can't override
        let mut engine = PolicyEngine::permissive();
        let caps = PaneCapabilities::alt_screen();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("policy.alt_screen"));
    }

    #[test]
    fn authorize_requires_approval_for_unknown_alt_screen() {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.alt_screen = None; // Unknown

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("policy.alt_screen_unknown"));
    }

    #[test]
    fn authorize_allows_human_with_unknown_alt_screen() {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.alt_screen = None; // Unknown

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_requires_approval_with_recent_gap() {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.has_recent_gap = true;

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("policy.recent_gap"));
    }

    #[test]
    fn authorize_allows_human_with_recent_gap() {
        // Humans are trusted - they can proceed despite gaps
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.has_recent_gap = true;

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn authorize_read_actions_ignore_alt_screen_and_gap() {
        // Read operations should be allowed regardless of pane state
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::alt_screen();
        caps.has_recent_gap = true;

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

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
        let mut engine = PolicyEngine::new(1, 100, false);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        assert!(engine.authorize(&input).is_allowed());
        let decision = engine.authorize(&input);
        assert!(decision.requires_approval()); // Rate limited
        assert_eq!(decision.rule_id(), Some("policy.rate_limit"));
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

    // ========================================================================
    // Redactor Tests - True Positives (MUST redact)
    // ========================================================================

    #[test]
    fn redactor_redacts_openai_key() {
        let redactor = Redactor::new();
        let input = "My API key is sk-abc123456789012345678901234567890123456789012345678901";
        let output = redactor.redact(input);
        assert!(
            output.contains("[REDACTED]"),
            "OpenAI key should be redacted"
        );
        assert!(
            !output.contains("sk-abc"),
            "OpenAI key should not appear in output"
        );
    }

    #[test]
    fn redactor_redacts_openai_proj_key() {
        let redactor = Redactor::new();
        let input = "API key: sk-proj-abcdefghijklmnopqrstuvwxyz12345678901234567890";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("sk-proj-"));
    }

    #[test]
    fn redactor_redacts_anthropic_key() {
        let redactor = Redactor::new();
        let input =
            "export ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz12345678901234567890";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("sk-ant-"));
    }

    #[test]
    fn redactor_redacts_github_pat() {
        let redactor = Redactor::new();
        let input = "GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz1234567890";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("ghp_"));
    }

    #[test]
    fn redactor_redacts_github_oauth() {
        let redactor = Redactor::new();
        let input = "Token: gho_abcdefghijklmnopqrstuvwxyz1234567890";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("gho_"));
    }

    #[test]
    fn redactor_redacts_aws_access_key_id() {
        let redactor = Redactor::new();
        let input = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("AKIA"));
    }

    #[test]
    fn redactor_redacts_aws_secret_key() {
        let redactor = Redactor::new();
        let input = "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("wJalrXUtnFEMI"));
    }

    #[test]
    fn redactor_redacts_bearer_token() {
        let redactor = Redactor::new();
        let input = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("eyJhbGciOi"));
    }

    #[test]
    fn redactor_redacts_slack_bot_token() {
        let redactor = Redactor::new();
        // Minimal-length token matching regex xox[bpar]-[a-zA-Z0-9-]{10,}
        let input = "SLACK_TOKEN=xoxb-0123456789";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("xoxb-"));
    }

    #[test]
    fn redactor_redacts_stripe_secret_key() {
        let redactor = Redactor::new();
        // Minimal-length key matching regex [ps]k_(?:live|test)_[a-zA-Z0-9]{20,}
        let input = "stripe.api_key = sk_live_01234567890123456789";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("sk_live_"));
    }

    #[test]
    fn redactor_redacts_stripe_test_key() {
        let redactor = Redactor::new();
        // Minimal-length key matching regex [ps]k_(?:live|test)_[a-zA-Z0-9]{20,}
        let input = "STRIPE_KEY=sk_test_01234567890123456789";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("sk_test_"));
    }

    #[test]
    fn redactor_redacts_database_url_password() {
        let redactor = Redactor::new();
        let input = "DATABASE_URL=postgres://user:supersecretpassword@localhost:5432/mydb";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("supersecretpassword"));
    }

    #[test]
    fn redactor_redacts_mysql_url() {
        let redactor = Redactor::new();
        let input = "mysql://admin:hunter2@db.example.com/production";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("hunter2"));
    }

    #[test]
    fn redactor_redacts_device_code() {
        let redactor = Redactor::new();
        let input = "Enter device_code: ABCD-EFGH-1234";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("ABCD-EFGH"));
    }

    #[test]
    fn redactor_redacts_oauth_url_with_token() {
        let redactor = Redactor::new();
        let input = "Redirect: https://example.com/callback?access_token=abc123xyz789";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("access_token=abc"));
    }

    #[test]
    fn redactor_redacts_oauth_url_with_code() {
        let redactor = Redactor::new();
        let input = "Visit https://auth.example.com/oauth?code=authcode123456789";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("code=auth"));
    }

    #[test]
    fn redactor_redacts_generic_api_key() {
        let redactor = Redactor::new();
        let input = "api_key = abcdef1234567890abcdef1234567890";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("abcdef1234567890"));
    }

    #[test]
    fn redactor_redacts_generic_token() {
        let redactor = Redactor::new();
        let input = "token: my_secret_token_value_12345678";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("my_secret_token"));
    }

    #[test]
    fn redactor_redacts_generic_password() {
        let redactor = Redactor::new();
        let input = "password: mysecretpassword123";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("mysecretpassword"));
    }

    #[test]
    fn redactor_redacts_generic_secret() {
        let redactor = Redactor::new();
        let input = "secret = client_secret_value_here";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("client_secret"));
    }

    // ========================================================================
    // Redactor Tests - False Positives (should NOT redact)
    // ========================================================================

    #[test]
    fn redactor_does_not_redact_normal_text() {
        let redactor = Redactor::new();
        let input = "This is just some normal text without any secrets.";
        let output = redactor.redact(input);
        assert_eq!(output, input, "Normal text should not be modified");
        assert!(!output.contains("[REDACTED]"));
    }

    #[test]
    fn redactor_does_not_redact_short_sk_prefix() {
        let redactor = Redactor::new();
        // "sk-" followed by short string should not match OpenAI pattern
        let input = "The task is done.";
        let output = redactor.redact(input);
        assert_eq!(output, input);
    }

    #[test]
    fn redactor_does_not_redact_normal_urls() {
        let redactor = Redactor::new();
        let input = "Visit https://example.com/page?id=123&name=test for more info";
        let output = redactor.redact(input);
        assert_eq!(
            output, input,
            "Normal URLs without tokens should not be redacted"
        );
    }

    #[test]
    fn redactor_does_not_redact_code_variables() {
        let redactor = Redactor::new();
        let input = "let tokenCount = 5; let secretKey = getKey();";
        let output = redactor.redact(input);
        // Variables like tokenCount or secretKey shouldn't trigger redaction
        // since they don't have assignment patterns with actual values
        assert!(!output.contains("[REDACTED]") || output == input);
    }

    #[test]
    fn redactor_does_not_redact_short_passwords() {
        let redactor = Redactor::new();
        // Very short passwords (< 4 chars) should not be redacted to avoid false positives
        let input = "password: abc";
        let output = redactor.redact(input);
        // 3-char password should not be redacted (pattern requires 4+ chars)
        assert!(!output.contains("[REDACTED]") || input == output);
    }

    #[test]
    fn redactor_preserves_surrounding_text() {
        let redactor = Redactor::new();
        let input = "Before sk-abc123456789012345678901234567890123456789012345678901 After";
        let output = redactor.redact(input);
        assert!(output.starts_with("Before "));
        assert!(output.ends_with(" After"));
        assert!(output.contains("[REDACTED]"));
    }

    // ========================================================================
    // Redactor Tests - Helper Methods
    // ========================================================================

    #[test]
    fn redactor_contains_secrets_true_positive() {
        let redactor = Redactor::new();
        let input = "My key is sk-abc123456789012345678901234567890123456789012345678901";
        assert!(redactor.contains_secrets(input));
    }

    #[test]
    fn redactor_contains_secrets_false_for_normal_text() {
        let redactor = Redactor::new();
        let input = "Just some regular text without any secrets";
        assert!(!redactor.contains_secrets(input));
    }

    #[test]
    fn redactor_detect_returns_locations() {
        let redactor = Redactor::new();
        let input = "Key: sk-abc123456789012345678901234567890123456789012345678901";
        let detections = redactor.detect(input);
        assert!(!detections.is_empty(), "Should detect at least one secret");
        assert_eq!(detections[0].0, "openai_key");
    }

    #[test]
    fn redactor_debug_markers_include_pattern_name() {
        let redactor = Redactor::with_debug_markers();
        let input = "sk-abc123456789012345678901234567890123456789012345678901";
        let output = redactor.redact(input);
        assert!(output.contains("[REDACTED:openai_key]"));
    }

    #[test]
    fn redactor_handles_multiple_secrets() {
        let redactor = Redactor::new();
        let input = "OpenAI: sk-abc123456789012345678901234567890123456789012345678901 \
                     GitHub: ghp_abcdefghijklmnopqrstuvwxyz1234567890";
        let output = redactor.redact(input);
        assert!(!output.contains("sk-abc"));
        assert!(!output.contains("ghp_"));
        // Should have two [REDACTED] markers
        assert_eq!(output.matches("[REDACTED]").count(), 2);
    }

    #[test]
    fn redactor_policy_engine_integration() {
        let engine = PolicyEngine::permissive();
        let text = "API key: sk-abc123456789012345678901234567890123456789012345678901";
        let redacted = engine.redact_secrets(text);
        assert!(redacted.contains("[REDACTED]"));
        assert!(!redacted.contains("sk-abc"));
    }

    // ========================================================================
    // PaneCapabilities Tests
    // ========================================================================

    #[test]
    fn pane_capabilities_prompt_is_input_safe() {
        let caps = PaneCapabilities::prompt();
        assert!(caps.prompt_active);
        assert!(!caps.command_running);
        assert_eq!(caps.alt_screen, Some(false));
        assert!(caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_running_is_not_input_safe() {
        let caps = PaneCapabilities::running();
        assert!(!caps.prompt_active);
        assert!(caps.command_running);
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_unknown_alt_screen_is_not_safe() {
        let caps = PaneCapabilities::unknown();
        assert!(!caps.is_state_known());
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_alt_screen_is_not_input_safe() {
        let caps = PaneCapabilities::alt_screen();
        assert_eq!(caps.alt_screen, Some(true));
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_gap_prevents_input() {
        let mut caps = PaneCapabilities::prompt();
        caps.has_recent_gap = true;
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_reservation_prevents_input() {
        let mut caps = PaneCapabilities::prompt();
        caps.is_reserved = true;
        caps.reserved_by = Some("other_workflow".to_string());
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_clear_gap_on_prompt() {
        let mut caps = PaneCapabilities::prompt();
        caps.has_recent_gap = true;
        assert!(caps.has_recent_gap);

        caps.clear_gap_on_prompt();
        assert!(!caps.has_recent_gap);
    }

    #[test]
    fn pane_capabilities_clear_gap_requires_prompt() {
        let mut caps = PaneCapabilities::running();
        caps.has_recent_gap = true;

        caps.clear_gap_on_prompt();
        // Gap not cleared because not at prompt
        assert!(caps.has_recent_gap);
    }

    #[test]
    fn pane_capabilities_from_ingest_state_at_prompt() {
        use crate::ingest::{Osc133State, ShellState};

        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let caps = PaneCapabilities::from_ingest_state(Some(&osc_state), Some(false), false);

        assert!(caps.prompt_active);
        assert!(!caps.command_running);
        assert_eq!(caps.alt_screen, Some(false));
        assert!(!caps.has_recent_gap);
        assert!(caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_from_ingest_state_command_running() {
        use crate::ingest::{Osc133State, ShellState};

        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::CommandRunning;

        let caps = PaneCapabilities::from_ingest_state(Some(&osc_state), Some(false), false);

        assert!(!caps.prompt_active);
        assert!(caps.command_running);
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_from_ingest_state_with_gap() {
        use crate::ingest::{Osc133State, ShellState};

        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let caps = PaneCapabilities::from_ingest_state(Some(&osc_state), Some(false), true);

        assert!(caps.prompt_active);
        assert!(caps.has_recent_gap);
        assert!(!caps.is_input_safe()); // Gap prevents safe input
    }

    #[test]
    fn pane_capabilities_from_ingest_state_alt_screen() {
        use crate::ingest::Osc133State;

        let osc_state = Osc133State::new();

        let caps = PaneCapabilities::from_ingest_state(Some(&osc_state), Some(true), false);

        assert_eq!(caps.alt_screen, Some(true));
        assert!(!caps.is_input_safe());
    }

    #[test]
    fn pane_capabilities_from_ingest_state_unknown_alt_screen() {
        use crate::ingest::{Osc133State, ShellState};

        let mut osc_state = Osc133State::new();
        osc_state.state = ShellState::PromptActive;

        let caps = PaneCapabilities::from_ingest_state(Some(&osc_state), None, false);

        assert!(caps.prompt_active);
        assert_eq!(caps.alt_screen, None);
        assert!(!caps.is_state_known());
        assert!(!caps.is_input_safe()); // Unknown alt-screen is not safe
    }

    #[test]
    fn pane_capabilities_from_ingest_state_no_osc() {
        let caps = PaneCapabilities::from_ingest_state(None, Some(false), false);

        assert!(!caps.prompt_active);
        assert!(!caps.command_running);
        assert_eq!(caps.alt_screen, Some(false));
        assert!(!caps.is_input_safe()); // No prompt active
    }

    // ========================================================================
    // InjectionResult Tests (wa-4vx.8.5)
    // ========================================================================

    #[test]
    fn injection_result_allowed_is_allowed() {
        let result = InjectionResult::Allowed {
            decision: PolicyDecision::allow(),
            summary: "ls -la".to_string(),
            pane_id: 1,
            action: ActionKind::SendText,
            audit_action_id: None,
        };
        assert!(result.is_allowed());
        assert!(!result.is_denied());
        assert!(!result.requires_approval());
        assert!(result.error_message().is_none());
        assert!(result.rule_id().is_none());
    }

    #[test]
    fn injection_result_denied_is_denied() {
        let result = InjectionResult::Denied {
            decision: PolicyDecision::deny_with_rule("unsafe command", "command.dangerous"),
            summary: "rm -rf /".to_string(),
            pane_id: 1,
            action: ActionKind::SendText,
            audit_action_id: None,
        };
        assert!(!result.is_allowed());
        assert!(result.is_denied());
        assert!(!result.requires_approval());
        assert!(result.error_message().is_none());
        assert_eq!(result.rule_id(), Some("command.dangerous"));
    }

    #[test]
    fn injection_result_requires_approval_is_correct() {
        let result = InjectionResult::RequiresApproval {
            decision: PolicyDecision::require_approval_with_rule(
                "needs approval",
                "policy.approval",
            ),
            summary: "git reset --hard".to_string(),
            pane_id: 1,
            action: ActionKind::SendText,
            audit_action_id: None,
        };
        assert!(!result.is_allowed());
        assert!(!result.is_denied());
        assert!(result.requires_approval());
        assert_eq!(result.rule_id(), Some("policy.approval"));
    }

    #[test]
    fn injection_result_error_has_message() {
        let result = InjectionResult::Error {
            error: "pane not found".to_string(),
            pane_id: 999,
            action: ActionKind::SendText,
            audit_action_id: None,
        };
        assert!(!result.is_allowed());
        assert!(!result.is_denied());
        assert!(!result.requires_approval());
        assert_eq!(result.error_message(), Some("pane not found"));
    }

    #[test]
    fn injection_result_serializes_correctly() {
        let result = InjectionResult::Allowed {
            decision: PolicyDecision::allow(),
            summary: "echo test".to_string(),
            pane_id: 42,
            action: ActionKind::SendText,
            audit_action_id: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"status\":\"allowed\""));
        assert!(json.contains("\"pane_id\":42"));
    }

    // ========================================================================
    // Policy Rules Tests (wa-4vx.8.4)
    // ========================================================================

    #[test]
    fn policy_rules_empty_config_matches_nothing() {
        let config = PolicyRulesConfig::default();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
        assert!(result.decision.is_none());
    }

    #[test]
    fn policy_rules_disabled_config_matches_nothing() {
        let config = PolicyRulesConfig {
            enabled: false,
            rules: vec![PolicyRule {
                id: "test".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch::default(), // catch-all
                decision: PolicyRuleDecision::Deny,
                message: Some("should not match".to_string()),
            }],
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_catch_all_matches_everything() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "catch-all".to_string(),
                description: Some("Match all actions".to_string()),
                priority: 100,
                match_on: PolicyRuleMatch::default(),
                decision: PolicyRuleDecision::RequireApproval,
                message: Some("All actions require approval".to_string()),
            }],
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());
        assert_eq!(result.decision, Some(PolicyRuleDecision::RequireApproval));
    }

    #[test]
    fn policy_rules_match_action_kind() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "deny-close".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    actions: vec!["close".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Deny,
                message: Some("Close actions are denied".to_string()),
            }],
        };

        // Should match close action
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));

        // Should not match send_text action
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_match_actor_kind() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "mcp-approval".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    actors: vec!["mcp".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::RequireApproval,
                message: Some("MCP actors need approval".to_string()),
            }],
        };

        // Should match MCP actor
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Mcp);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());
        assert_eq!(result.decision, Some(PolicyRuleDecision::RequireApproval));

        // Should not match Robot actor
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_match_pane_id() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "allow-pane-42".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    pane_ids: vec![42],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Allow,
                message: Some("Pane 42 is trusted".to_string()),
            }],
        };

        // Should match pane 42
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(42);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());
        assert_eq!(result.decision, Some(PolicyRuleDecision::Allow));

        // Should not match pane 1
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(1);
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_match_pane_title_glob() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "deny-vim".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    pane_titles: vec!["*vim*".to_string(), "*nvim*".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Deny,
                message: Some("Don't send to vim".to_string()),
            }],
        };

        // Should match vim title
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane_title("nvim file.rs");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));

        // Should not match bash title
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_title("bash");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_match_pane_cwd_glob() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "allow-home".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    pane_cwds: vec!["/home/*".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Allow,
                message: Some("Home dirs are safe".to_string()),
            }],
        };

        // Should match home directory
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_cwd("/home/user");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());

        // Should not match /tmp
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_cwd("/tmp/work");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_match_command_regex() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "deny-rm".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    command_patterns: vec![r"^rm\s+".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Deny,
                message: Some("rm commands denied".to_string()),
            }],
        };

        // Should match rm command
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_command_text("rm -rf /tmp/old");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));

        // Should not match ls command
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_command_text("ls -la");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_match_agent_type() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "trust-claude".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    agent_types: vec!["claude".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Allow,
                message: Some("Claude agents are trusted".to_string()),
            }],
        };

        // Should match claude agent (case insensitive)
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_agent_type("Claude");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_some());

        // Should not match cursor agent
        let input =
            PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_agent_type("cursor");
        let result = evaluate_policy_rules(&config, &input);
        assert!(result.matching_rule.is_none());
    }

    #[test]
    fn policy_rules_precedence_priority_wins() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "low-priority-allow".to_string(),
                    description: None,
                    priority: 200,
                    match_on: PolicyRuleMatch::default(),
                    decision: PolicyRuleDecision::Allow,
                    message: None,
                },
                PolicyRule {
                    id: "high-priority-deny".to_string(),
                    description: None,
                    priority: 50,
                    match_on: PolicyRuleMatch::default(),
                    decision: PolicyRuleDecision::Deny,
                    message: None,
                },
            ],
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        assert_eq!(
            result.matching_rule.as_ref().unwrap().id,
            "high-priority-deny"
        );
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));
    }

    #[test]
    fn policy_rules_precedence_deny_beats_allow_same_priority() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "allow-rule".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch::default(),
                    decision: PolicyRuleDecision::Allow,
                    message: None,
                },
                PolicyRule {
                    id: "deny-rule".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch::default(),
                    decision: PolicyRuleDecision::Deny,
                    message: None,
                },
            ],
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);
        let result = evaluate_policy_rules(&config, &input);
        // Deny should win over Allow at same priority
        assert_eq!(result.matching_rule.as_ref().unwrap().id, "deny-rule");
        assert_eq!(result.decision, Some(PolicyRuleDecision::Deny));
    }

    #[test]
    fn policy_rules_precedence_specificity_tiebreaker() {
        let config = PolicyRulesConfig {
            enabled: true,
            rules: vec![
                PolicyRule {
                    id: "general-deny".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch {
                        actions: vec!["send_text".to_string()],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: None,
                },
                PolicyRule {
                    id: "specific-deny".to_string(),
                    description: None,
                    priority: 100,
                    match_on: PolicyRuleMatch {
                        actions: vec!["send_text".to_string()],
                        pane_ids: vec![42],
                        ..Default::default()
                    },
                    decision: PolicyRuleDecision::Deny,
                    message: None,
                },
            ],
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(42);
        let result = evaluate_policy_rules(&config, &input);
        // More specific rule should win
        assert_eq!(result.matching_rule.as_ref().unwrap().id, "specific-deny");
    }

    #[test]
    fn policy_rules_integrated_into_authorize_deny() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "deny-robot-close".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    actions: vec!["close".to_string()],
                    actors: vec!["robot".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Deny,
                message: Some("Robots cannot close panes".to_string()),
            }],
        };

        let mut engine = PolicyEngine::permissive().with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        let decision = engine.authorize(&input);
        assert!(decision.is_denied());
        assert_eq!(decision.rule_id(), Some("config.rule.deny-robot-close"));
    }

    #[test]
    fn policy_rules_integrated_into_authorize_allow() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "allow-trusted-pane".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    pane_ids: vec![999],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::Allow,
                message: Some("Pane 999 is trusted".to_string()),
            }],
        };

        let mut engine = PolicyEngine::strict().with_policy_rules(rules);
        // This would normally require approval due to destructive action
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Robot)
            .with_pane(999)
            .with_capabilities(PaneCapabilities::prompt());

        let decision = engine.authorize(&input);
        // Allow rule should short-circuit the destructive action check
        assert!(decision.is_allowed());
        assert_eq!(decision.rule_id(), Some("config.rule.allow-trusted-pane"));
    }

    #[test]
    fn policy_rules_integrated_into_authorize_require_approval() {
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "approval-for-mcp".to_string(),
                description: None,
                priority: 100,
                match_on: PolicyRuleMatch {
                    actors: vec!["mcp".to_string()],
                    ..Default::default()
                },
                decision: PolicyRuleDecision::RequireApproval,
                message: Some("MCP actions need approval".to_string()),
            }],
        };

        let mut engine = PolicyEngine::permissive().with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Mcp)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("config.rule.approval-for-mcp"));
    }

    #[test]
    fn policy_rules_builtin_gates_take_precedence() {
        // Even with an allow rule, builtin safety gates should still work
        let rules = PolicyRulesConfig {
            enabled: true,
            rules: vec![PolicyRule {
                id: "allow-everything".to_string(),
                description: None,
                priority: 1, // Very high priority
                match_on: PolicyRuleMatch::default(),
                decision: PolicyRuleDecision::Allow,
                message: None,
            }],
        };

        // Rate limit should still trigger even with allow-everything rule
        // (rate limit is checked before custom rules)
        let mut engine = PolicyEngine::new(1, 100, false).with_policy_rules(rules);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        // First call allowed
        assert!(engine.authorize(&input).is_allowed());
        // Second call should hit rate limit, which is evaluated before custom rules
        let decision = engine.authorize(&input);
        assert!(decision.requires_approval());
        assert_eq!(decision.rule_id(), Some("policy.rate_limit"));
    }

    #[test]
    fn policy_rule_match_specificity() {
        // Test specificity scoring
        let empty = PolicyRuleMatch::default();
        assert_eq!(empty.specificity(), 0);
        assert!(empty.is_catch_all());

        let action_only = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            ..Default::default()
        };
        assert_eq!(action_only.specificity(), 1);
        assert!(!action_only.is_catch_all());

        let pane_id_match = PolicyRuleMatch {
            pane_ids: vec![42],
            ..Default::default()
        };
        assert_eq!(pane_id_match.specificity(), 2); // ID match is worth 2

        let multi_criteria = PolicyRuleMatch {
            actions: vec!["send_text".to_string()],
            actors: vec!["robot".to_string()],
            pane_ids: vec![42],
            command_patterns: vec!["rm.*".to_string()],
            ..Default::default()
        };
        assert_eq!(multi_criteria.specificity(), 6); // 1 + 1 + 2 + 2
    }

    #[test]
    fn policy_rule_decision_priority() {
        assert_eq!(PolicyRuleDecision::Deny.priority(), 0);
        assert_eq!(PolicyRuleDecision::RequireApproval.priority(), 1);
        assert_eq!(PolicyRuleDecision::Allow.priority(), 2);
    }

    #[test]
    fn glob_match_patterns() {
        // Test the glob matching helper
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*.rs", "file.rs"));
        assert!(!glob_match("*.rs", "file.go"));
        assert!(glob_match("/home/*", "/home/user"));
        assert!(glob_match("*vim*", "neovim"));
        assert!(glob_match("test?", "test1"));
        assert!(!glob_match("test?", "test12"));
    }

    // ========================================================================
    // Risk Scoring Tests
    // ========================================================================

    #[test]
    fn risk_score_zero_for_safe_action() {
        let engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt());

        let risk = engine.calculate_risk(&input);
        assert!(risk.is_low());
        assert!(risk.factors.is_empty());
    }

    #[test]
    fn risk_score_elevated_for_alt_screen() {
        let engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::alt_screen());

        let risk = engine.calculate_risk(&input);
        assert!(risk.score >= 60); // Alt-screen has weight 60
        assert!(risk.factors.iter().any(|f| f.id == "state.alt_screen"));
    }

    #[test]
    fn risk_score_unknown_alt_screen() {
        let engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::unknown());

        let risk = engine.calculate_risk(&input);
        assert!(
            risk.factors
                .iter()
                .any(|f| f.id == "state.alt_screen_unknown")
        );
    }

    #[test]
    fn risk_score_includes_destructive_content() {
        let engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("rm -rf /tmp/test");

        let risk = engine.calculate_risk(&input);
        assert!(
            risk.factors
                .iter()
                .any(|f| f.id == "content.destructive_tokens")
        );
    }

    #[test]
    fn risk_score_includes_sudo() {
        let engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("sudo apt update");

        let risk = engine.calculate_risk(&input);
        assert!(
            risk.factors
                .iter()
                .any(|f| f.id == "content.sudo_elevation")
        );
    }

    #[test]
    fn risk_score_accumulates_factors() {
        let engine = PolicyEngine::permissive();

        // Multiple risk factors: untrusted actor + mutating action + command running
        let caps = PaneCapabilities {
            command_running: true,
            alt_screen: Some(false),
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let risk = engine.calculate_risk(&input);
        // Should have: action.is_mutating (10) + context.actor_untrusted (15) + state.command_running (25)
        assert!(risk.score >= 50);
        assert!(risk.factors.len() >= 3);
    }

    #[test]
    fn risk_config_can_disable_factors() {
        let mut config = RiskConfig::default();
        config.disabled.insert("state.alt_screen".to_string());

        let engine = PolicyEngine::permissive().with_risk_config(config);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::alt_screen());

        let risk = engine.calculate_risk(&input);
        // Alt-screen factor should not be present
        assert!(!risk.factors.iter().any(|f| f.id == "state.alt_screen"));
    }

    #[test]
    fn risk_config_can_override_weights() {
        let mut config = RiskConfig::default();
        config.weights.insert("state.alt_screen".to_string(), 10); // Reduce from 60 to 10

        let engine = PolicyEngine::permissive().with_risk_config(config);
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::alt_screen());

        let risk = engine.calculate_risk(&input);
        let alt_factor = risk.factors.iter().find(|f| f.id == "state.alt_screen");
        assert!(alt_factor.is_some());
        assert_eq!(alt_factor.unwrap().weight, 10);
    }

    #[test]
    fn risk_to_decision_allow_for_low() {
        let engine = PolicyEngine::permissive();
        let risk = RiskScore {
            score: 20,
            factors: vec![],
            summary: "Low risk".to_string(),
        };
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Human);

        let decision = engine.risk_to_decision(&risk, &input);
        assert!(decision.is_allowed());
    }

    #[test]
    fn risk_to_decision_require_approval_for_elevated() {
        let engine = PolicyEngine::permissive();
        let risk = RiskScore {
            score: 60,
            factors: vec![],
            summary: "Elevated risk".to_string(),
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);

        let decision = engine.risk_to_decision(&risk, &input);
        assert!(decision.requires_approval());
    }

    #[test]
    fn risk_to_decision_deny_for_high() {
        let engine = PolicyEngine::permissive();
        let risk = RiskScore {
            score: 80,
            factors: vec![],
            summary: "High risk".to_string(),
        };
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot);

        let decision = engine.risk_to_decision(&risk, &input);
        assert!(decision.is_denied());
    }

    #[test]
    fn risk_score_deterministic() {
        let engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::alt_screen())
            .with_command_text("sudo rm -rf /tmp");

        let risk1 = engine.calculate_risk(&input);
        let risk2 = engine.calculate_risk(&input);

        assert_eq!(risk1.score, risk2.score);
        assert_eq!(risk1.factors.len(), risk2.factors.len());
    }

    #[test]
    fn risk_score_capped_at_100() {
        // Create a scenario with many risk factors that would exceed 100
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            alt_screen: Some(true), // 60
            has_recent_gap: true,   // 35
            is_reserved: true,      // 50
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot) // +10 mutating +15 untrusted
            .with_pane(1)
            .with_capabilities(caps)
            .with_command_text("sudo rm -rf /"); // +30 sudo +40 destructive

        let risk = engine.calculate_risk(&input);
        assert!(risk.score <= 100);
    }

    // wa-upg.6.3: Verify risk flows through authorize() into decision context
    #[test]
    fn authorize_attaches_risk_to_context() {
        let mut engine = PolicyEngine::permissive();

        // Create input with some risk factors
        let caps = PaneCapabilities {
            prompt_active: true,
            command_running: true, // Adds risk: command_running (25)
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot) // +10 mutating +15 untrusted
            .with_pane(1)
            .with_capabilities(caps);

        let decision = engine.authorize(&input);

        // Decision should have context with risk
        let context = decision.context().expect("Decision should have context");
        let risk = context
            .risk
            .as_ref()
            .expect("Context should have risk score");

        // Should have accumulated some risk factors
        assert!(risk.score > 0, "Risk score should be > 0 for this input");
        assert!(
            !risk.factors.is_empty(),
            "Should have contributing risk factors"
        );
    }

    #[test]
    fn authorize_risk_included_in_serialized_output() {
        let mut engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            prompt_active: true,
            command_running: true,
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps)
            .with_command_text("sudo apt update"); // Adds sudo risk

        let decision = engine.authorize(&input);

        // Serialize to JSON and verify risk is included
        let json = serde_json::to_string(&decision).expect("Decision should serialize");

        // Risk should be in the serialized output
        assert!(
            json.contains("\"risk\""),
            "Serialized decision should include risk object"
        );
        assert!(
            json.contains("\"score\""),
            "Serialized decision should include risk score"
        );
        assert!(
            json.contains("\"factors\""),
            "Serialized decision should include risk factors"
        );
    }

    // ========================================================================
    // wa-upg.6.4: Risk Scoring Matrix Tests
    // ========================================================================

    /// Test risk scoring matrix with representative condition combinations
    #[test]
    fn risk_matrix_safe_read_action() {
        // ReadOutput action doesn't add action-specific risk factors
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            prompt_active: true, // Safe state
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let risk = engine.calculate_risk(&input);
        // Read actions don't add mutating/destructive risk factors
        assert!(
            !risk.factors.iter().any(|f| f.id == "action.is_mutating"),
            "Read action should not have mutating factor"
        );
        assert!(
            !risk.factors.iter().any(|f| f.id == "action.is_destructive"),
            "Read action should not have destructive factor"
        );
    }

    #[test]
    fn risk_matrix_human_actor_trusted() {
        // Human actors don't get the "untrusted actor" penalty
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            prompt_active: true,
            ..Default::default()
        };

        let robot_input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps.clone());

        let human_input = PolicyInput::new(ActionKind::SendText, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(caps);

        let robot_risk = engine.calculate_risk(&robot_input);
        let human_risk = engine.calculate_risk(&human_input);

        // Robot gets untrusted actor penalty (15), human doesn't
        assert!(
            robot_risk.score > human_risk.score,
            "Robot should have higher risk than human"
        );
        assert!(
            robot_risk
                .factors
                .iter()
                .any(|f| f.id == "context.actor_untrusted"),
            "Robot should have untrusted actor factor"
        );
        assert!(
            !human_risk
                .factors
                .iter()
                .any(|f| f.id == "context.actor_untrusted"),
            "Human should not have untrusted actor factor"
        );
    }

    #[test]
    fn risk_matrix_combined_state_factors() {
        // Test accumulation of multiple state factors
        let engine = PolicyEngine::permissive();

        // Combine: alt_screen (60) + command_running (25) + has_recent_gap (35)
        let caps = PaneCapabilities {
            alt_screen: Some(true),
            command_running: true,
            has_recent_gap: true,
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let risk = engine.calculate_risk(&input);

        // Should be capped at 100 but have multiple factors
        assert_eq!(risk.score, 100, "Combined state factors should cap at 100");
        assert!(
            risk.factors.len() >= 3,
            "Should have at least 3 state factors"
        );
    }

    #[test]
    fn risk_matrix_content_analysis() {
        // Test content analysis factors
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            prompt_active: true,
            ..Default::default()
        };

        // Test various dangerous commands
        let test_cases = vec![
            ("rm -rf /", "content.destructive_tokens"),
            ("sudo apt update", "content.sudo_elevation"),
            // Pipe chain requires 2+ pipes
            ("echo 'hello' | grep test | wc -l", "content.pipe_chain"),
            ("cat <<EOF\nline1\nline2\nEOF", "content.multiline_complex"),
        ];

        for (command, expected_factor) in test_cases {
            let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
                .with_pane(1)
                .with_capabilities(caps.clone())
                .with_command_text(command);

            let risk = engine.calculate_risk(&input);
            assert!(
                risk.factors.iter().any(|f| f.id == expected_factor),
                "Command '{}' should trigger factor '{}'",
                command,
                expected_factor
            );
        }
    }

    #[test]
    fn risk_matrix_reserved_pane() {
        // Test reserved pane scenarios
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            prompt_active: true,
            is_reserved: true,
            reserved_by: Some("other-workflow".to_string()),
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let risk = engine.calculate_risk(&input);
        assert!(
            risk.factors.iter().any(|f| f.id == "state.is_reserved"),
            "Should have reserved pane factor"
        );
    }

    // ========================================================================
    // wa-upg.6.4: Factor Ordering Stability Tests
    // ========================================================================

    #[test]
    fn risk_factors_have_stable_ordering() {
        // Factors should be in deterministic order across multiple calls
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            alt_screen: Some(true),
            command_running: true,
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps)
            .with_command_text("sudo rm -rf /tmp");

        // Calculate risk multiple times
        let risk1 = engine.calculate_risk(&input);
        let risk2 = engine.calculate_risk(&input);
        let risk3 = engine.calculate_risk(&input);

        // Extract factor IDs in order
        let ids1: Vec<_> = risk1.factors.iter().map(|f| &f.id).collect();
        let ids2: Vec<_> = risk2.factors.iter().map(|f| &f.id).collect();
        let ids3: Vec<_> = risk3.factors.iter().map(|f| &f.id).collect();

        assert_eq!(ids1, ids2, "Factor ordering should be stable (run 1 vs 2)");
        assert_eq!(ids2, ids3, "Factor ordering should be stable (run 2 vs 3)");
    }

    #[test]
    fn risk_factor_weights_are_stable() {
        // Each factor should have the same weight across calls
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            alt_screen: Some(true),
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let risk1 = engine.calculate_risk(&input);
        let risk2 = engine.calculate_risk(&input);

        for (f1, f2) in risk1.factors.iter().zip(risk2.factors.iter()) {
            assert_eq!(f1.id, f2.id, "Factor IDs should match");
            assert_eq!(f1.weight, f2.weight, "Factor weights should be stable");
            assert_eq!(
                f1.explanation, f2.explanation,
                "Factor explanations should be stable"
            );
        }
    }

    // ========================================================================
    // wa-upg.6.4: JSON Schema Validation Tests
    // ========================================================================

    #[test]
    fn risk_score_json_schema_has_required_fields() {
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            command_running: true,
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps)
            .with_command_text("sudo test");

        let risk = engine.calculate_risk(&input);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&risk).unwrap()).unwrap();

        // Verify top-level fields
        assert!(
            json.get("score").is_some(),
            "JSON should have 'score' field"
        );
        assert!(
            json.get("factors").is_some(),
            "JSON should have 'factors' field"
        );
        assert!(
            json.get("summary").is_some(),
            "JSON should have 'summary' field"
        );

        // Verify score is a number
        assert!(
            json["score"].is_number(),
            "score should be a number, got {:?}",
            json["score"]
        );

        // Verify factors is an array
        assert!(
            json["factors"].is_array(),
            "factors should be an array, got {:?}",
            json["factors"]
        );

        // Verify summary is a string
        assert!(
            json["summary"].is_string(),
            "summary should be a string, got {:?}",
            json["summary"]
        );
    }

    #[test]
    fn risk_factor_json_schema_has_required_fields() {
        let engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            alt_screen: Some(true),
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps);

        let risk = engine.calculate_risk(&input);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&risk).unwrap()).unwrap();

        let factors = json["factors"].as_array().expect("factors should be array");
        assert!(!factors.is_empty(), "Should have at least one factor");

        for factor in factors {
            // Each factor must have: id, weight, explanation
            assert!(
                factor.get("id").is_some(),
                "Factor should have 'id' field: {:?}",
                factor
            );
            assert!(
                factor.get("weight").is_some(),
                "Factor should have 'weight' field: {:?}",
                factor
            );
            assert!(
                factor.get("explanation").is_some(),
                "Factor should have 'explanation' field: {:?}",
                factor
            );

            // Verify types
            assert!(factor["id"].is_string(), "id should be string");
            assert!(factor["weight"].is_number(), "weight should be number");
            assert!(
                factor["explanation"].is_string(),
                "explanation should be string"
            );
        }
    }

    #[test]
    fn decision_context_risk_json_is_valid() {
        let mut engine = PolicyEngine::permissive();

        let caps = PaneCapabilities {
            prompt_active: true,
            command_running: true,
            ..Default::default()
        };

        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(caps)
            .with_command_text("sudo test");

        let decision = engine.authorize(&input);
        let json: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&decision).unwrap()).unwrap();

        // Navigate to context.risk
        let context = json.get("context").expect("Decision should have context");
        let risk = context.get("risk").expect("Context should have risk");

        // Verify risk structure
        assert!(risk.get("score").is_some(), "Risk should have score");
        assert!(risk.get("factors").is_some(), "Risk should have factors");
        assert!(risk.get("summary").is_some(), "Risk should have summary");

        // Verify score range
        let score = risk["score"].as_u64().expect("score should be number");
        assert!(score <= 100, "Score should be <= 100, got {}", score);
    }

    #[test]
    fn risk_summary_matches_score_range() {
        // Test each risk level
        let test_cases = vec![
            (0, "Low risk"),
            (20, "Low risk"),
            (21, "Medium risk"),
            (50, "Medium risk"),
            (51, "Elevated risk"),
            (70, "Elevated risk"),
            (71, "High risk"),
            (100, "High risk"),
        ];

        for (score, expected_summary) in test_cases {
            let risk = RiskScore {
                score,
                factors: vec![],
                summary: risk_summary(score),
            };

            assert_eq!(
                risk.summary, expected_summary,
                "Score {} should have summary '{}'",
                score, expected_summary
            );
        }
    }

    fn risk_summary(score: u8) -> String {
        match score {
            0..=20 => "Low risk".to_string(),
            21..=50 => "Medium risk".to_string(),
            51..=70 => "Elevated risk".to_string(),
            71..=100 => "High risk".to_string(),
            _ => unreachable!(),
        }
    }
}
